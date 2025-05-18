use crate::vendor::nonblocking::quic::{packet_batch_sender, PacketAccumulator};
use bytes::Bytes;
use crossbeam_channel::{Sender, TrySendError};
use quinn::{Connecting, Endpoint};
use solana_perf::packet::{Meta, PacketBatch, PACKET_DATA_SIZE};
use solana_quic_definitions::QUIC_CONNECTION_HANDSHAKE_TIMEOUT;
use solana_streamer::quic::QuicServerError;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use std::{array, thread};
use tracing::log::debug;
use tracing::{error, warn};

pub fn spawn_server(
    thread_name: &'static str,
    endpoint: Endpoint,
    packet_sender: Sender<PacketBatch>,
    exit: Arc<AtomicBool>,
    wait_for_chunk_timeout: Duration,
    qcoalesce: Duration,
    coalesce_channel_size: usize,
    threads: usize,
) -> Result<thread::JoinHandle<()>, QuicServerError> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(threads)
        .thread_name(format!("{thread_name}Rt"))
        .enable_all()
        .build()
        .unwrap();

    let task = {
        let _guard = runtime.enter();
        tokio::spawn(run_server(
            endpoint,
            packet_sender,
            exit,
            wait_for_chunk_timeout,
            qcoalesce,
            coalesce_channel_size,
        ))
    };
    let handle = std::thread::Builder::new()
        .name(thread_name.into())
        .spawn(move || {
            if let Err(e) = runtime.block_on(task) {
                warn!("error from runtime.block_on: {:?}", e);
            }
        })
        .unwrap();
    Ok(handle)
}

async fn run_server(
    endpoint: Endpoint,
    packet_sender: Sender<PacketBatch>,
    exit: Arc<AtomicBool>,
    wait_for_chunk_timeout: Duration,
    coalesce: Duration,
    coalesce_channel_size: usize,
) {
    let (sender, receiver) = crossbeam_channel::bounded(coalesce_channel_size);

    std::thread::spawn({
        let exit = exit.clone();
        move || {
            packet_batch_sender(packet_sender, receiver, exit, coalesce);
        }
    });

    while !exit.load(Ordering::Relaxed) {
        let incoming = endpoint.accept().await;
        if let Some(incoming) = incoming {
            let connecting = incoming.accept();
            if let Ok(connecting) = connecting {
                tokio::spawn(handle_connection(
                    connecting,
                    sender.clone(),
                    wait_for_chunk_timeout,
                ));
            }
        }
    }
}

async fn handle_connection(
    connecting: Connecting,
    packet_sender: Sender<PacketAccumulator>,
    wait_for_chunk_timeout: Duration,
) {
    let res = tokio::time::timeout(QUIC_CONNECTION_HANDSHAKE_TIMEOUT, connecting).await;
    if let Ok(connection_res) = res {
        match connection_res {
            Ok(connection) => {
                //TODO: handle cancellation for this loop.
                'outer: loop {
                    let mut stream = match connection.accept_uni().await {
                        Ok(stream) => stream,
                        Err(e) => {
                            error!("Error accepting stream: {:?}", e);
                            break 'outer;
                        }
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
