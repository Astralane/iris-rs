use crate::vendor::nonblocking::quic::{packet_batch_sender, PacketAccumulator};
use crate::vendor::quic::configure_server;
use bytes::Bytes;
use crossbeam_channel::{Sender, TrySendError};
use quinn::{Connecting, Endpoint, TokioRuntime};
use quinn_proto::EndpointConfig;
use solana_perf::packet::{Meta, PacketBatch, PACKET_DATA_SIZE};
use solana_quic_definitions::QUIC_CONNECTION_HANDSHAKE_TIMEOUT;
use solana_sdk::net::DEFAULT_TPU_COALESCE;
use solana_sdk::signature::Keypair;
use solana_streamer::nonblocking::quic::DEFAULT_WAIT_FOR_CHUNK_TIMEOUT;
use solana_streamer::quic::QuicServerError;
use std::net::UdpSocket;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use std::{array, thread};
use tokio::select;
use tokio_util::sync::CancellationToken;
use tracing::log::debug;
use tracing::{error, info, warn};

pub(crate) const DEFAULT_MAX_COALESCE_CHANNEL_SIZE: usize = 250_000;

pub struct IrisQuicServer {
    thread: std::thread::JoinHandle<()>,
}

impl IrisQuicServer {
    pub fn spawn_new(
        thread_name: &str,
        tpu_socket: UdpSocket,
        tpu_sender: Sender<PacketBatch>,
        identity_keypair: &Keypair,
        exit: CancellationToken,
        num_threads: usize,
    ) -> Result<Self, QuicServerError> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(num_threads)
            .thread_name(format!("{thread_name}Rt"))
            .enable_all()
            .build()
            .unwrap();
        let task = {
            let _guard = runtime.enter();
            let hdl = crate::quic_server::spawn_server(
                tpu_socket,
                identity_keypair,
                tpu_sender,
                exit,
                DEFAULT_WAIT_FOR_CHUNK_TIMEOUT,
                DEFAULT_TPU_COALESCE,
                DEFAULT_MAX_COALESCE_CHANNEL_SIZE,
            );
            hdl
        };
        let handle = std::thread::Builder::new()
            .name(thread_name.into())
            .spawn(move || {
                if let Err(e) = runtime.block_on(task.unwrap()) {
                    warn!("error from runtime.block_on: {:?}", e);
                }
            })
            .unwrap();

        Ok(Self { thread: handle })
    }

    pub fn join(self) -> thread::Result<()> {
        self.thread.join()
    }
}

pub(crate) fn spawn_server(
    tpu_socket: UdpSocket,
    keypair: &Keypair,
    packet_sender: Sender<PacketBatch>,
    exit: CancellationToken,
    wait_for_chunk_timeout: Duration,
    qcoalesce: Duration,
    coalesce_channel_size: usize,
) -> Result<tokio::task::JoinHandle<()>, QuicServerError> {
    let (config, _) = configure_server(keypair)?;
    let endpoint = Endpoint::new(
        EndpointConfig::default(),
        Some(config.clone()),
        tpu_socket,
        Arc::new(TokioRuntime),
    )
    .map_err(QuicServerError::EndpointFailed)?;
    let addr = endpoint.local_addr();
    info!("listening on {:?}", addr.unwrap());
    let handle = tokio::spawn(run_server(
        endpoint,
        packet_sender,
        exit,
        wait_for_chunk_timeout,
        qcoalesce,
        coalesce_channel_size,
    ));
    Ok(handle)
}

async fn run_server(
    endpoint: Endpoint,
    packet_sender: Sender<PacketBatch>,
    cancel: CancellationToken,
    wait_for_chunk_timeout: Duration,
    coalesce: Duration,
    coalesce_channel_size: usize,
) {
    let (sender, receiver) = crossbeam_channel::bounded(coalesce_channel_size);
    std::thread::spawn({
        let cancel = cancel.clone();
        move || {
            packet_batch_sender(packet_sender, receiver, cancel, coalesce);
        }
    });
    const WAIT_FOR_CONNECTION_TIMEOUT: Duration = Duration::from_secs(1);

    loop {
        let incoming = select! {
            incoming = endpoint.accept() => incoming,
             _ = tokio::time::sleep(WAIT_FOR_CONNECTION_TIMEOUT) => {
               None
            }
            _ = cancel.cancelled() => break,
        };
        if let Some(incoming) = incoming {
            let connecting = incoming.accept();
            if let Ok(connecting) = connecting {
                tokio::spawn(handle_connection(
                    connecting,
                    sender.clone(),
                    wait_for_chunk_timeout,
                    cancel.clone(),
                ));
            }
        }
    }
}

async fn handle_connection(
    connecting: Connecting,
    packet_sender: Sender<PacketAccumulator>,
    wait_for_chunk_timeout: Duration,
    cancel: CancellationToken,
) {
    let res = tokio::time::timeout(QUIC_CONNECTION_HANDSHAKE_TIMEOUT, connecting).await;
    if let Ok(connection_res) = res {
        match connection_res {
            Ok(connection) => {
                'outer: loop {
                    let mut stream = select! {
                        conn = connection.accept_uni() => {
                            match conn {
                                Ok(stream) => stream,
                                Err(e) => {
                                    error!("Error accepting stream: {:?}", e);
                                    break 'outer;
                                }
                            }
                        },
                        _ = cancel.cancelled() => break,
                    };

                    let mut accum = PacketAccumulator::new(Meta::default());
                    // each solana txn should come withing a single udp datagram due to size constraints on
                    // the network, sometimes comes in 4 datagrams as well depending on how header positions
                    // and quic protocol packets, expects packets with only one transaction to be sent via stream
                    let mut chunks: [Bytes; 4] = array::from_fn(|_| Bytes::new());
                    loop {
                        let n_chunks = match tokio::time::timeout(
                            wait_for_chunk_timeout,
                            stream.read_chunks(&mut chunks),
                        )
                        .await
                        {
                            Ok(Ok(Some(chunk))) => chunk,
                            Ok(Ok(None)) => {
                                //the stream has ended!
                                break;
                            }
                            Ok(Err(e)) => {
                                debug!("Received stream error: {:?}", e);
                                break;
                            }
                            Err(e) => {
                                debug!("Timeout in receiving on stream {e}");
                                break;
                            }
                        };
                        // Bytes clone is cheap
                        let received_chunks = chunks.iter().take(n_chunks).cloned();
                        for chunk in received_chunks {
                            accum.meta.size += chunk.len();
                            if accum.meta.size > PACKET_DATA_SIZE {
                                // The stream window size is set to PACKET_DATA_SIZE, so one individual chunk can
                                // never exceed this size. A peer can send two chunks that together exceed the size
                                // tho, in which case we report the error.
                                debug!("invalid stream size {}", accum.meta.size);
                                break;
                            }
                            accum.chunks.push(chunk);
                        }
                        // send to the enxt stage
                        if let Err(err) = packet_sender.try_send(accum.clone()) {
                            match err {
                                TrySendError::Full(_) => {
                                    debug!("Packet sender is full, dropping packet");
                                }
                                TrySendError::Disconnected(_) => {
                                    debug!("Packet sender is disconnected, dropping packet");
                                }
                            }
                        }
                    }
                }
            }
            Err(_) => {}
        }
    }
}
