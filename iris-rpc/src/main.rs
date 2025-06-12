#![warn(unused_crate_dependencies)]

use crate::chain_state::ChainStateWsClient;
use crate::otel_tracer::{get_subscriber_with_otpl, init_subscriber};
use crate::rpc::IrisRpcServer;
use crate::rpc_server::IrisRpcServerImpl;
use crate::tpu_next_client::TpuClientNextSender;
use crate::utils::{ChainStateClient, SendTransactionClient};
use anyhow::{anyhow, Context};
use figment::providers::Env;
use figment::Figment;
use jsonrpsee::server::ServerBuilder;
use metrics_exporter_prometheus::PrometheusBuilder;
use rustls::crypto::CryptoProvider;
use serde::{Deserialize, Serialize};
use solana_client::nonblocking::pubsub_client::PubsubClient;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::signature::{read_keypair_file, Keypair};
use solana_tpu_client_next::leader_updater::create_leader_updater;
use solana_tpu_client_next::workers_cache::shutdown_worker;
use std::fmt::Debug;
use std::net::SocketAddr;
use std::num::NonZeroUsize;
use std::process;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;
use tokio::runtime::{Handle, Runtime};
use tokio_util::sync::CancellationToken;
use tracing::info;
use tracing_subscriber::EnvFilter;

mod chain_state;
mod otel_tracer;
mod quic_server;
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
    identity_keypair_file: Option<String>,
    grpc_url: Option<String>,
    max_retries: u32,
    num_threads: Option<usize>,
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
    lookahead_slots: u64,
    prometheus_addr: SocketAddr,
    retry_interval_seconds: u32,
    otpl_endpoint: String,
}

fn default_true() -> bool {
    true
}

fn main() -> anyhow::Result<()> {
    //for some reason ths is required to make rustls work
    CryptoProvider::install_default(rustls::crypto::ring::default_provider())
        .expect("Failed to install default crypto provider");

    dotenv::dotenv().ok();
    let config: Config = Figment::new().merge(Env::raw()).extract()?;

    let num_cores = std::thread::available_parallelism().map_or(1, NonZeroUsize::get);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(config.num_threads.unwrap_or(num_cores))
        .thread_name("iris-rpc-worker")
        .enable_all()
        .build()
        .context("Failed to build tokio runtime")?;
    //listen for
    runtime.block_on(run_async(config))?;
    Ok(())
}
async fn run_async(config: Config) -> anyhow::Result<()> {
    let subscriber = get_subscriber_with_otpl(config.otpl_endpoint.clone(), std::io::stdout).await;
    init_subscriber(subscriber);

    info!("config: {:?}", config);
    let shutdown = Arc::new(AtomicBool::new(false));

    tokio::spawn({
        let shutdown = shutdown.clone();
        async move {
            tokio::signal::ctrl_c()
                .await
                .expect("failed to install CTRL+C signal handler");
            shutdown.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    });

    let cancel = CancellationToken::new();

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

    info!("creating leader updater...");
    let leader_updater = create_leader_updater(rpc.clone(), config.ws_url.to_owned(), None)
        .await
        .map_err(|e| anyhow!(e))?;
    info!("leader updater created");
    let txn_store = Arc::new(store::TransactionStoreImpl::new());

    let tx_client: Arc<dyn SendTransactionClient> = Arc::new(TpuClientNextSender::create_client(
        Handle::current(),
        leader_updater,
        config.lookahead_slots as usize,
        identity_keypair,
        cancel.clone(),
    ));

    let ws_client = PubsubClient::new(&config.ws_url)
        .await
        .context("Failed to connect to websocket")?;

    let chain_state: Arc<dyn ChainStateClient> = Arc::new(ChainStateWsClient::new(
        Handle::current(),
        shutdown.clone(),
        800, // around 4 mins
        Arc::new(ws_client),
        config.grpc_url,
    ));

    let iris_rpc = IrisRpcServerImpl::new(
        tx_client,
        txn_store,
        chain_state,
        Duration::from_secs(config.retry_interval_seconds as u64),
        shutdown.clone(),
        config.max_retries,
    );

    let rpc_server = ServerBuilder::default()
        .max_request_body_size(15_000_000)
        .max_connections(1_000)
        .build(config.address)
        .await?;


    info!("server starting in {:?}", config.address);
    let server_hdl = rpc_server.start(iris_rpc.into_rpc());
    //exit when shutdown is triggered
    while !shutdown.load(std::sync::atomic::Ordering::Relaxed) {
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    server_hdl.stop()?;
    cancel.cancel();
    server_hdl.stopped().await;
    Ok(())
}
