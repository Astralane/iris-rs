use crate::legacy_client::ConnectionCacheClient;
use crate::rpc::IrisRpcServer;
use crate::rpc_server::IrisRpcServerImpl;
use anyhow::anyhow;
use figment::providers::Env;
use figment::Figment;
use jsonrpsee::server::ServerBuilder;
use serde::{Deserialize, Serialize};
use solana_client::connection_cache::ConnectionCache;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::signature::{read_keypair_file, Keypair};
use solana_tpu_client_next::leader_updater::create_leader_updater;
use std::fmt::Debug;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use tracing::info;
use tracing_subscriber::EnvFilter;

mod leader_updater;
mod legacy_client;
mod rpc;
mod rpc_server;
mod store;
mod tpu_next_client;
mod tx_sender;
mod vendor;

#[derive(Debug, Serialize, Deserialize)]
pub struct Config {
    rpc_url: String,
    ws_url: String,
    address: SocketAddr,
    bind: SocketAddr,
    #[serde(default)]
    identity_keypair_file: Option<String>,
    //forwards to known rpcs
    #[serde(default)]
    friendly_rpcs: Vec<String>,
    //should enable forwards to leader
    #[serde(default="default_true")]
    enable_leader_forwards: bool,
    max_retries: usize,
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
}

fn default_true() -> bool {
    true
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenv::dotenv().ok();
    env_logger::init();
    //setup tracing
    tracing::subscriber::set_global_default(
        tracing_subscriber::fmt()
            .with_env_filter(EnvFilter::from_env("RUST_LOG"))
            .finish(),
    )
    .expect("Failed to set up tracing");

    //read config from env variables
    let config: Config = Figment::new().merge(Env::raw()).extract().unwrap();
    info!("config: {:?}", config);

    let friendly_rpcs = config
        .friendly_rpcs
        .iter()
        .map(|rpc_url| Arc::new(RpcClient::new(rpc_url.to_owned())))
        .collect();

    let address = config.address;
    let rpc = Arc::new(RpcClient::new(config.rpc_url.to_owned()));
    let identity_keypair = config
        .identity_keypair_file
        .as_ref()
        .map(|file| read_keypair_file(file).expect("Failed to read identity keypair file"))
        .unwrap_or(Keypair::new());

    let connection_cache = Arc::new(ConnectionCache::new_with_client_options(
        "iris",
        24,
        None, // created if none specified
        Some((&identity_keypair, IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)))),
        None, // not used as far as I can tell
    ));

    let leader_updater = create_leader_updater(rpc.clone(), config.ws_url.to_owned(), None)
        .await
        .map_err(|e| anyhow!(e))?;

    // let tpu_sender_client = TpuClientNextSender::new(config).await;
    let legacy_client = ConnectionCacheClient::new(
        connection_cache,
        leader_updater,
        config.lookahead_slots,
        friendly_rpcs,
        config.enable_leader_forwards,
    )
    .await?;

    let iris = IrisRpcServerImpl::new(
        Arc::new(legacy_client),
        Arc::new(store::TransactionStoreImpl::new()),
    );

    let server = ServerBuilder::default()
        .max_request_body_size(15_000_000)
        .max_connections(1_000_000)
        .build(address)
        .await?;

    info!("server starting in {:?}", address);
    let server_hdl = server.start(iris.into_rpc());
    server_hdl.stopped().await;
    Ok(())
}
