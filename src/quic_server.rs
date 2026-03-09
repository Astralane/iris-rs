use crate::dedup_and_retry::DedupPacketPayload;
use crate::types::{PacketSource, TransactionPacket};
use crossbeam_channel::Sender;
use metrics::{counter, histogram};
use pem::Pem;
use quinn::crypto::rustls::QuicServerConfig;
use quinn::{
    Connecting, Endpoint, EndpointConfig, IdleTimeout, RecvStream, ServerConfig, TokioRuntime,
};
use rustls::KeyLogFile;
use solana_measure::measure_us;
use solana_sdk::signature::Keypair;
use solana_streamer::nonblocking::quic::ALPN_TPU_PROTOCOL_ID;
use solana_streamer::packet::PACKET_DATA_SIZE;
use solana_streamer::quic::{QuicServerError, QUIC_MAX_TIMEOUT};
use solana_tls_utils::{new_dummy_x509_certificate, tls_server_config_builder};
use std::net::UdpSocket;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;
use tracing::{error, warn};

const MAX_PACKET_SIZE: usize = PACKET_DATA_SIZE;

pub struct QuicServerResult {
    server_t: std::thread::JoinHandle<()>,
}

impl QuicServerResult {
    pub fn join(self) -> std::thread::Result<()> {
        self.server_t.join()
    }
}

pub(crate) fn configure_server(
    identity_keypair: &Keypair,
) -> Result<(ServerConfig, String), QuicServerError> {
    let (cert, priv_key) = new_dummy_x509_certificate(identity_keypair);
    let cert_chain_pem_parts = vec![Pem {
        tag: "CERTIFICATE".to_string(),
        contents: cert.as_ref().to_vec(),
    }];
    let cert_chain_pem = pem::encode_many(&cert_chain_pem_parts);

    let mut server_tls_config =
        tls_server_config_builder().with_single_cert(vec![cert], priv_key)?;
    server_tls_config.alpn_protocols = vec![ALPN_TPU_PROTOCOL_ID.to_vec()];
    server_tls_config.key_log = Arc::new(KeyLogFile::new());
    let quic_server_config = QuicServerConfig::try_from(server_tls_config)?;

    let mut server_config = ServerConfig::with_crypto(Arc::new(quic_server_config));

    // disable path migration as we do not expect TPU clients to be on a mobile device
    server_config.migration(false);

    let config = Arc::get_mut(&mut server_config.transport).unwrap();
    // to support 1000 txns concurrently
    config.max_concurrent_uni_streams(1024u32.into());
    let timeout = IdleTimeout::try_from(QUIC_MAX_TIMEOUT).unwrap();
    config.max_idle_timeout(Some(timeout));

    // disable bidi & datagrams
    config.max_concurrent_bidi_streams(0u32.into());
    config.datagram_receive_buffer_size(None);

    // Disable GSO. The server only accepts inbound unidirectional streams initiated by clients,
    // which means that reply data never exceeds one MTU. By disabling GSO, we make
    // quinn_proto::Connection::poll_transmit allocate only 1 MTU vs 10 * MTU for _each_ transmit.
    // See https://github.com/anza-xyz/agave/pull/1647.
    config.enable_segmentation_offload(false);

    Ok((server_config, cert_chain_pem))
}

pub fn spawn_new(
    thread_name: &'static str,
    bind_port: u16,
    num_threads: usize,
    identity_keypair: &Keypair,
    dedup_sender: Sender<DedupPacketPayload>,
    cancel: CancellationToken,
) -> anyhow::Result<QuicServerResult> {
    let sock = UdpSocket::bind(("0.0.0.0", bind_port)).map_err(anyhow::Error::msg)?;
    sock.set_nonblocking(true)?;

    let (config, _cert_chain_pem) =
        configure_server(identity_keypair).map_err(anyhow::Error::msg)?;
    let endpoint = Endpoint::new(
        EndpointConfig::default(),
        Some(config.clone()),
        sock,
        Arc::new(TokioRuntime),
    )
    .map_err(anyhow::Error::msg)?;
    let rt = tokio::runtime::Builder::new_multi_thread()
        .thread_name("quic-server-rt")
        .worker_threads(num_threads)
        .enable_all()
        .build()
        .expect("failed to build tokio runtime");

    let server_handle = {
        let _guard = rt.enter();
        tokio::spawn(quic_server_loop(endpoint, dedup_sender, cancel))
    };
    let server_t = std::thread::Builder::new()
        .name(thread_name.into())
        .spawn(move || {
            if let Err(e) = rt.block_on(server_handle) {
                warn!("error from runtime.block_on: {e:?}");
            }
        })
        .unwrap();
    Ok(QuicServerResult { server_t })
}

async fn quic_server_loop(
    endpoint: Endpoint,
    dedup_sender: Sender<DedupPacketPayload>,
    cancel: CancellationToken,
) {
    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                return;
            }
            maybe_incoming = endpoint.accept() => {
                if let Some(incoming) = maybe_incoming {
                    match incoming.accept() {
                        Ok(connecting) => {
                            tokio::spawn(handle_connection(connecting, dedup_sender.clone()));
                        }
                        Err(err) => {
                            error!("quic server incoming conn error: {}", err);
                            counter!("quic_server_incoming_err", "error" => err.to_string() ).increment(1);
                        }
                    }
                }
            }
        }
    }
}

async fn handle_connection(connecting: Connecting, dedup_sender: Sender<DedupPacketPayload>) {
    let conn = match connecting.await {
        Ok(conn) => conn,
        Err(e) => {
            error!("quic connection error: {}", e);
            return;
        }
    };

    loop {
        let stream = match conn.accept_uni().await {
            Ok(stream) => stream,
            Err(e) => {
                error!("quic accept uni stream error: {}", e);
                counter!("quic_server_incoming_err", "error" => e.to_string() ).increment(1);
                break;
            }
        };
        tokio::task::spawn(handle_uni_stream(stream, dedup_sender.clone()));
    }
}

async fn handle_uni_stream(mut stream: RecvStream, dedup_sender: Sender<DedupPacketPayload>) {
    let now = Instant::now();
    const READ_TIMEOUT: Duration = Duration::from_secs(2);
    let data = match tokio::time::timeout(READ_TIMEOUT, stream.read_to_end(MAX_PACKET_SIZE)).await {
        Ok(Ok(packet)) => packet,
        Ok(Err(e)) => {
            error!("stream read error {e:?}");
            counter!("quic_stream_read_error", "error" => e.to_string()).increment(1);
            return;
        }
        Err(_) => {
            error!("stream timed out after {READ_TIMEOUT:?}");
            counter!("quic_stream_timed_out").increment(1);
            return;
        }
    };

    let (packet, micros) = measure_us!(match wincode::deserialize::<TransactionPacket>(&data) {
        Ok(packet) => packet,
        Err(err) => {
            error!("cannot decode packet {err:?}");
            return;
        }
    });
    histogram!("wincode_deserialize_micros").record(micros as f64);
    if let Err(e) = dedup_sender.try_send((packet, Instant::now(), PacketSource::Quic)) {
        error!("cannot send from quic-server to dedup {e:?}");
        counter!("quic_to_dedup_send_err", "error" => e.to_string()).increment(1);
    }
    histogram!("handle_uni_stream_latency").record(now.elapsed().as_micros() as f64);
}
