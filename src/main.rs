#![warn(unused_crate_dependencies)]
use crate::chain_state::ChainStateWsClient;
use crate::otel_tracer::{
    get_subscriber_with_otpl, init_subscriber, init_subscriber_without_signoz,
};
use crate::rpc::IrisRpcServer;
use crate::rpc_server::IrisRpcServerImpl;
use anyhow::anyhow;
use figment::providers::Env;
use figment::Figment;
use jsonrpsee::server::ServerBuilder;
use metrics_exporter_prometheus::PrometheusBuilder;
use rustls::crypto::CryptoProvider;
use serde::{Deserialize, Serialize};
use solana_client::nonblocking::pubsub_client::PubsubClient;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{read_keypair_file, Keypair};
use solana_streamer::socket::SocketAddrSpace;
use solana_tpu_client_next::leader_updater::create_leader_updater;
use std::fmt::Debug;
use std::net::SocketAddr;
use std::process;
use std::str::FromStr;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;
use tokio::runtime::Handle;
use tokio_util::sync::CancellationToken;
use tracing::info;

mod broadcaster;
mod chain_state;
mod otel_tracer;
mod rpc;
mod rpc_server;
mod shield;
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
    tx_max_retries: u32,
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
    leaders_fanout: u64,
    use_tpu_client_next: bool,
    prometheus_addr: SocketAddr,
    metrics_update_interval_secs: u64,
    tx_retry_interval_ms: u32,
    shield_policy_key: Option<String>,
    otpl_endpoint: Option<String>,
    dedup_cache_max_size: usize,
    shred_version: u16,
    rust_log: Option<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    //for some reason ths is required to make rustls work
    CryptoProvider::install_default(rustls::crypto::ring::default_provider())
        .expect("Failed to install default crypto provider");

    dotenv::dotenv().ok();

    //read config from env variables
    let config: Config = Figment::new().merge(Env::raw()).extract().unwrap();

    match config.otpl_endpoint.clone() {
        Some(endpoint) => {
            let subscriber = get_subscriber_with_otpl(
                config.rust_log.clone().unwrap_or("info".to_string()),
                endpoint,
                config.address.port().clone(),
                std::io::stdout,
            )
            .await;
            init_subscriber(subscriber)
        }
        None => init_subscriber_without_signoz(std::io::stdout),
    }
    info!("config: {:?}", config);

    let identity_keypair = config
        .identity_keypair_file
        .as_ref()
        .map(|file| read_keypair_file(file).expect("Failed to read identity keypair file"))
        .unwrap_or(Keypair::new());

    let shield_policy_key = config
        .shield_policy_key
        .map(|s| Pubkey::from_str(&s))
        .and_then(|p| p.ok())
        .expect("Failed to parse shield policy key");

    PrometheusBuilder::new()
        .with_http_listener(config.prometheus_addr)
        .install()
        .expect("failed to install recorder/exporter");

    let shutdown = Arc::new(AtomicBool::new(false));
    let cancel = CancellationToken::new();
    let rpc = Arc::new(RpcClient::new(config.rpc_url.to_owned()));
    info!("creating leader updater...");
    let leader_updater = create_leader_updater(rpc.clone(), config.ws_url.to_owned(), None)
        .await
        .map_err(|e| anyhow!(e))?;
    info!("leader updater created");
    let txn_store = Arc::new(store::TransactionStoreImpl::new());
    let socket_addr_space = SocketAddrSpace::new(false);
    let (_gossip_service, _ip_echo, _s_py_ref) = solana_gossip::gossip_service::make_gossip_node(
        Keypair::new(),
        None,
        shutdown.clone(),
        None,
        config.shred_version,
        false,
        socket_addr_space,
    );

    let (tx_client, tpu_client_jh) = tpu_next_client::spawn_tpu_client_send_txs(
        leader_updater,
        config.leaders_fanout,
        identity_keypair,
        rpc.clone(),
        shield_policy_key,
        config.metrics_update_interval_secs,
        config.worker_channel_size,
        config.max_reconnect_attempts,
        cancel.clone(),
    );
    let ws_client = PubsubClient::new(&config.ws_url)
        .await
        .expect("Failed to connect to websocket");

    let chain_state = ChainStateWsClient::new(
        Handle::current(),
        shutdown.clone(),
        800, // around 4 mins
        Arc::new(ws_client),
        config.grpc_url,
    );

    let iris = IrisRpcServerImpl::new(
        Arc::new(tx_client),
        txn_store,
        Arc::new(chain_state),
        Duration::from_millis(config.tx_retry_interval_ms as u64),
        shutdown.clone(),
        config.tx_max_retries,
        config.dedup_cache_max_size,
    );

    let server = ServerBuilder::default()
        .max_request_body_size(15_000_000)
        .max_connections(1_000_000)
        .build(config.address)
        .await?;

    info!("server starting in {:?}", config.address);
    let server_hdl = server.start(iris.into_rpc());
    // if the solana rpc server connection is lost, the server will exit
    info!("waiting for shutdown signal");
    while !shutdown.load(std::sync::atomic::Ordering::Relaxed) {
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    cancel.cancel();
    server_hdl.stop()?;
    server_hdl.stopped().await;
    tpu_client_jh.await.expect("failed to join tpu client");
    process::exit(1);
}
