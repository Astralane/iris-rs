#![warn(unused_crate_dependencies)]

use crate::chain_state::ChainStateWsClient;
use crate::otel_tracer::{get_subscriber_with_otpl, init_subscriber};
use crate::tpu_next_client::TpuClientNextSender;
use figment::providers::Env;
use figment::Figment;
use metrics_exporter_prometheus::PrometheusBuilder;
use rustls::crypto::CryptoProvider;
use serde::{Deserialize, Serialize};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::signature::{read_keypair_file, Keypair};
use std::fmt::Debug;
use std::net::{SocketAddr, UdpSocket};
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use tracing::info;

mod chain_state;
mod otel_tracer;
mod quic_forwarder;
mod rpc;
mod rpc_server;
mod store;
mod tpu_next_client;
mod utils;
mod vendor;

#[derive(Debug, Serialize, Deserialize)]
pub struct Config {
    rpc_url: String,
    ws_url: String,
    address: SocketAddr,
    quic_bind_address: SocketAddr,
    quic_server_threads: Option<usize>,
    identity_keypair_file: Option<String>,
    grpc_url: Option<String>,
    max_retries: u32,
    rpc_num_threads: Option<usize>,
    tpu_client_num_threads: Option<usize>,
    //The number of connections to be maintained by the scheduler.
    num_connections: usize,
    //Whether to skip checking the transaction blockhash expiration.
    skip_check_transaction_age: bool,
    //The size of the channel used to transmit transaction batches to the worker tasks.
    worker_channel_size: usize,
    //The maximum number of reconnection attempts allowed in case of connection failure.
    max_reconnect_attempts: usize,
    //The number of slots to look ahead during the leader estimation procedure.
    //Determines how far into the future leaders are estimated,
    //allowing connections to be established with those leaders in advance.
    leader_forward_count: u64,
    prometheus_addr: SocketAddr,
    retry_interval_seconds: u32,
    otpl_endpoint: String,
}

fn main() -> anyhow::Result<()> {
    //for some reason ths is required to make rustls work
    CryptoProvider::install_default(rustls::crypto::ring::default_provider())
        .expect("Failed to install default crypto provider");

    dotenv::dotenv().ok();
    let config: Config = Figment::new().merge(Env::raw()).extract()?;
    let subscriber = get_subscriber_with_otpl(config.otpl_endpoint.clone(), std::io::stdout);
    init_subscriber(subscriber);
    let _num_cores = std::thread::available_parallelism().map_or(1, NonZeroUsize::get);

    let identity_keypair = config
        .identity_keypair_file
        .as_ref()
        .map(|file| read_keypair_file(file).expect("Failed to read identity keypair file"))
        .unwrap_or(Keypair::new());

    let _metrics = PrometheusBuilder::new()
        .with_http_listener(config.prometheus_addr)
        .install()
        .expect("failed to install recorder/exporter");
    let rpc = Arc::new(RpcClient::new(config.rpc_url.to_owned()));

    info!("config: {:?}", config);
    let cancel = CancellationToken::new();
    let txn_store = Arc::new(store::TransactionStoreImpl::new());

    let (chain_state, chain_state_hdl) = ChainStateWsClient::spawn_new(
        cancel.clone(),
        800, // around 4 mins
        config.ws_url.clone(),
        config.grpc_url,
    );

    let (tx_client, tpu_next_handle) = TpuClientNextSender::spawn_client(
        config.tpu_client_num_threads.unwrap_or(4usize),
        rpc.clone(),
        config.ws_url,
        config.leader_forward_count as usize,
        &identity_keypair,
        cancel.clone(),
    );

    let tx_client = Arc::new(tx_client);

    let quic_handle = quic_forwarder::QuicTxForwarder::spawn_new(
        UdpSocket::bind(config.quic_bind_address)?,
        tx_client.clone(),
        &identity_keypair,
        cancel.clone(),
        config.quic_server_threads.unwrap_or(4usize),
    );

    let rpc_handle = rpc_server::spawn_jsonrpc_server(
        config.address,
        config.rpc_num_threads.unwrap_or(4usize),
        tx_client,
        txn_store,
        Arc::new(chain_state),
        Duration::from_secs(config.retry_interval_seconds as u64),
        cancel.clone(),
        config.max_retries,
    );

    rpc_handle.join().unwrap();
    tpu_next_handle.join().unwrap();
    chain_state_hdl.join().unwrap();
    quic_handle.join().unwrap();
    Ok(())
}
