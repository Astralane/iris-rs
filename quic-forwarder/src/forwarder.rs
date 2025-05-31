use crate::vendor::quic::configure_server;
use crossbeam_channel::Sender;
use quinn::{Endpoint, TokioRuntime};
use quinn_proto::EndpointConfig;
use solana_perf::packet::PacketBatch;
use solana_sdk::net::DEFAULT_TPU_COALESCE;
use solana_sdk::signature::Keypair;
use solana_streamer::nonblocking::quic::DEFAULT_WAIT_FOR_CHUNK_TIMEOUT;
use solana_streamer::quic::QuicServerError;
use std::net::UdpSocket;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::thread;
use std::thread::JoinHandle;
use tracing::warn;

pub(crate) const DEFAULT_MAX_COALESCE_CHANNEL_SIZE: usize = 250_000;

pub struct IrisQuicForwarder {
    thread: std::thread::JoinHandle<()>,
}

impl IrisQuicForwarder {
    pub fn create_new(
        thread_name: &str,
        tpu_socket: UdpSocket,
        tpu_sender: Sender<PacketBatch>,
        identity_keypair: Keypair,
        exit: Arc<AtomicBool>,
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
