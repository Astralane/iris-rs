#![warn(unused_crate_dependencies)]
use crate::broadcaster::MevProtectedBroadcaster;
use crate::chain_state::ChainStateWsClient;
use crate::http_middleware::HttpLoggingMiddleware;
use crate::otel_tracer::{
    get_subscriber_with_otpl, init_subscriber, init_subscriber_without_signoz,
};
use crate::rpc::IrisRpcServer;
use crate::rpc_server::IrisRpcServerImpl;
use figment::providers::Env;
use figment::Figment;
use jsonrpsee::server::{ServerBuilder, ServerConfig};
use metrics_exporter_prometheus::PrometheusBuilder;
use rustls::crypto::CryptoProvider;
use serde::{Deserialize, Serialize};
use solana_client::nonblocking::pubsub_client::PubsubClient;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::read_keypair_file;
use solana_tpu_client_next::node_address_service::LeaderTpuCacheServiceConfig;
use solana_tpu_client_next::websocket_node_address_service::WebsocketNodeAddressService;
use std::fmt::Debug;
use std::net::SocketAddr;
use std::process;
use std::str::FromStr;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;
use tokio::runtime::Handle;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

mod broadcaster;
mod chain_state;
mod gossip_service;
mod http_middleware;
mod otel_tracer;
mod rpc;
mod rpc_server;
mod shield;
mod store;
mod tpu_next_client;
mod utils;
mod vendor;
mod quic_server;

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
    leaders_fanout: u8,
    use_tpu_client_next: bool,
    prometheus_addr: SocketAddr,
    metrics_update_interval_secs: u64,
    tx_retry_interval_ms: u32,
    shield_policy_key: Option<String>,
    otpl_endpoint: Option<String>,
    dedup_cache_max_size: usize,
    gossip_keypair_file: Option<String>,
    gossip_port_range: Option<(u16, u16)>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    //for some reason ths is required to make rustls work
    CryptoProvider::install_default(rustls::crypto::ring::default_provider())
        .expect("Failed to install default crypto provider");

    dotenv::dotenv().ok();
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    //read config from env variables
    let config: Config = Figment::new()
        .merge(Env::raw())
        .extract()
        .expect("config not valid");

    match config.otpl_endpoint.clone() {
        Some(endpoint) => {
            let subscriber = get_subscriber_with_otpl(
                env_filter,
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
        .and_then(|file| read_keypair_file(file).ok())
        .expect("No identity keypair file");

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

    let _gossip_task = if let Some(port_range) = config.gossip_port_range {
        let gossip_keypair = config
            .gossip_keypair_file
            .as_ref()
            .and_then(|file| read_keypair_file(file).ok());
        Some(gossip_service::run_gossip_service(
            port_range,
            gossip_keypair,
            shutdown.clone(),
        ))
    } else {
        None
    };

    let rpc = Arc::new(RpcClient::new(config.rpc_url.to_owned()));
    info!("creating leader updater...");
    let tpu_cache_config = LeaderTpuCacheServiceConfig {
        lookahead_leaders: config.leaders_fanout + 1,
        refresh_nodes_info_every: Default::default(),
        max_consecutive_failures: 10,
    };
    let leader_updater = WebsocketNodeAddressService::run(
        rpc.clone(),
        config.ws_url.clone(),
        tpu_cache_config,
        cancel.clone(),
    )
    .await
    .expect("Failed to start leader updater");
    info!("leader updater created");
    let txn_store = Arc::new(store::TransactionStoreImpl::new());
    let (mev_protected_broadcaster, _broadcaster_jh) =
        MevProtectedBroadcaster::run(shield_policy_key, rpc.clone(), cancel.clone());

    let (tx_client, _client) = tpu_next_client::spawn_tpu_client_next(
        mev_protected_broadcaster,
        Box::new(leader_updater),
        config.leaders_fanout as usize,
        identity_keypair,
        config.worker_channel_size,
        config.max_reconnect_attempts,
        cancel.clone(),
    )
    .expect("cannot create tpu client next");
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

    let server_config = ServerConfig::builder()
        .max_request_body_size(10 * 1024 * 1024)
        .max_connections(10_000)
        .set_keep_alive(Some(Duration::from_secs(60)))
        .set_tcp_no_delay(true)
        .build();

    let http_middleware = tower::ServiceBuilder::new().layer_fn(HttpLoggingMiddleware);
    let server = ServerBuilder::with_config(server_config)
        .set_http_middleware(http_middleware)
        .build(config.address)
        .await?;

    info!("server starting in {:?}", config.address);
    let _server_hdl = server.start(iris.into_rpc());
    // if the solana rpc server connection is lost, the server will exit
    info!("waiting for shutdown signal");
    while !shutdown.load(std::sync::atomic::Ordering::Relaxed) {
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    process::exit(0);
}
