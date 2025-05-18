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

pub(crate) const DEFAULT_MAX_COALESCE_CHANNEL_SIZE: usize = 250_000;

pub struct IrisQuicForwarder {
    threads: std::thread::JoinHandle<()>,
}

impl IrisQuicForwarder {
    pub fn create_new(
        tpu_socket: UdpSocket,
        tpu_sender: Sender<PacketBatch>,
        identity_keypair: Keypair,
        exit: Arc<AtomicBool>,
        num_threads: usize,
    ) -> Result<Self, QuicServerError> {
        let (config, _) = configure_server(&identity_keypair)?;
        let endpoint = Endpoint::new(
            EndpointConfig::default(),
            Some(config.clone()),
            tpu_socket,
            Arc::new(TokioRuntime),
        )
        .map_err(QuicServerError::EndpointFailed)?;

        let hdl = crate::quic_server::spawn_server(
            "quic-server-t",
            endpoint,
            tpu_sender,
            exit,
            DEFAULT_WAIT_FOR_CHUNK_TIMEOUT,
            DEFAULT_TPU_COALESCE,
            DEFAULT_MAX_COALESCE_CHANNEL_SIZE,
            num_threads,
        )?;
        Ok(Self { threads: hdl })
    }
}
