use crate::dedup_and_retry::DedupPacketPayload;
use crate::runtime::{build_runtime, TokioRtConfig};
use crate::types::{PacketSource, TransactionPacket};
use crossbeam_channel::Sender;
use metrics::{counter, histogram};
use pem::Pem;
use quinn::crypto::rustls::QuicServerConfig;
use quinn::{
    AckFrequencyConfig, Connecting, ConnectionError, Endpoint, IdleTimeout, RecvStream,
    ServerConfig, VarInt,
};
use rustls::KeyLogFile;
use solana_measure::measure_us;
use solana_sdk::signature::Keypair;
use solana_streamer::nonblocking::quic::ALPN_TPU_PROTOCOL_ID;
use solana_streamer::packet::PACKET_DATA_SIZE;
use solana_streamer::quic::{QuicServerError, QUIC_MAX_TIMEOUT};
use solana_tls_utils::{new_dummy_x509_certificate, tls_server_config_builder};
use std::net::SocketAddr;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;
use tracing::{error, info};

const QUIC_MAX_SIZE: usize = 2 * PACKET_DATA_SIZE;

pub(crate) fn configure_server(
    identity_keypair: &Keypair,
    is_colo: bool,
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

    if is_colo {
        config.initial_rtt(Duration::from_millis(5));
        // Ask the peer to ACK more aggressively.
        // This is ignored if the peer doesn't support the ACK Frequency extension.
        let mut ack = AckFrequencyConfig::default();
        ack.ack_eliciting_threshold(VarInt::from_u32(0)); // ACK every packet
        ack.max_ack_delay(Some(Duration::from_millis(1)));
        ack.reordering_threshold(VarInt::from_u32(2));
        config.ack_frequency_config(Some(ack));
    }
    // // Make sure stream credit isn't a limiter.
    config.max_concurrent_uni_streams(16_384u32.into());
    let timeout = IdleTimeout::try_from(QUIC_MAX_TIMEOUT).unwrap();
    config.max_idle_timeout(Some(timeout));

    // disable bidi & datagrams
    config.max_concurrent_bidi_streams(0u32.into());
    config.datagram_receive_buffer_size(None);
    // Many tiny independent streams.
    config.send_fairness(false);

    // Disable GSO. The server only accepts inbound unidirectional streams initiated by clients,
    // which means that reply data never exceeds one MTU. By disabling GSO, we make
    // quinn_proto::Connection::poll_transmit allocate only 1 MTU vs 10 * MTU for _each_ transmit.
    // See https://github.com/anza-xyz/agave/pull/1647.
    config.enable_segmentation_offload(false);

    Ok((server_config, cert_chain_pem))
}

pub fn spawn_new(
    thread_name: &'static str,
    bind_addr: SocketAddr,
    rt_config: TokioRtConfig,
    identity_keypair: &Keypair,
    dedup_sender: Sender<DedupPacketPayload>,
    is_colo: bool,
    cancel: CancellationToken,
) -> anyhow::Result<JoinHandle<()>> {
    let (config, _cert_chain_pem) =
        configure_server(identity_keypair, is_colo).map_err(anyhow::Error::msg)?;
    let rt = build_runtime("quic-server-rt", &rt_config);

    let quic_t = std::thread::Builder::new()
        .name(thread_name.into())
        .spawn(move || {
            rt.block_on(async move {
                let endpoint = Endpoint::server(config, bind_addr).expect("cannot create endpoint");
                quic_server_loop(endpoint, dedup_sender, cancel).await
            })
        })
        .unwrap();
    Ok(quic_t)
}

async fn quic_server_loop(
    endpoint: Endpoint,
    dedup_sender: Sender<DedupPacketPayload>,
    cancel: CancellationToken,
) {
    info!("quic server loop starting");
    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                return;
            }
            maybe_incoming = endpoint.accept() => {
                if let Some(incoming) = maybe_incoming {
                    info!("got quic connection from {:?}", incoming.remote_address());
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
                if !matches!(e, ConnectionError::ApplicationClosed(_)) {
                    error!("quic accept uni stream error: {}", e);
                    counter!("quic_server_incoming_err", "error" => e.to_string() ).increment(1);
                }
                break;
            }
        };
        tokio::task::spawn(handle_uni_stream(stream, dedup_sender.clone()));
    }
}

async fn handle_uni_stream(mut stream: RecvStream, dedup_sender: Sender<DedupPacketPayload>) {
    let now = Instant::now();
    const READ_TIMEOUT: Duration = Duration::from_secs(2);
    let data = match tokio::time::timeout(READ_TIMEOUT, stream.read_to_end(QUIC_MAX_SIZE)).await {
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

    histogram!("quic_packet_data_size").record(data.len() as f64);

    let (packet, micros) = measure_us!(match wincode::deserialize::<TransactionPacket>(&data) {
        Ok(packet) => packet,
        Err(err) => {
            error!("cannot decode packet {err:?}");
            counter!("quic_txn_decode_error").increment(1);
            return;
        }
    });
    histogram!("wincode_deserialize_micros").record(micros as f64);
    if let Err(e) = dedup_sender.try_send((packet, Instant::now(), PacketSource::Quic)) {
        error!("cannot send from quic-server to dedup {e:?}");
        counter!("quic_to_dedup_send_err", "error" => e.to_string()).increment(1);
    }
    counter!("txn_quic_count").increment(1);
    histogram!("handle_uni_stream_latency").record(now.elapsed().as_micros() as f64);
}
