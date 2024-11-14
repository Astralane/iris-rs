use crate::connection_cache_client::ConnectionCacheClient;
use crate::rpc::IrisRpcServer;
use crate::rpc_forwards::RpcForwards;
use crate::rpc_server::IrisRpcServerImpl;
use crate::tpu_next_client::TpuClientNextSender;
use crate::transaction_client::{CreateClient, SendTransactionClient};
use anyhow::anyhow;
use figment::providers::Env;
use figment::Figment;
use jsonrpsee::server::ServerBuilder;
use serde::{Deserialize, Serialize};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::signature::{read_keypair_file, Keypair};
use solana_tpu_client_next::leader_updater::create_leader_updater;
use std::fmt::Debug;
use std::net::SocketAddr;
use std::sync::Arc;
use tracing::info;
use tracing_subscriber::EnvFilter;

mod connection_cache_client;
mod rpc;
mod rpc_forwards;
mod rpc_server;
mod store;
mod tpu_next_client;
mod transaction_client;
mod vendor;

#[derive(Debug, Serialize, Deserialize)]
pub struct Config {
    rpc_url: String,
    ws_url: String,
    address: SocketAddr,
    bind: SocketAddr,
    identity_keypair_file: Option<String>,
    //forwards to known rpcs
    friendly_rpcs: Vec<String>,
    //jito Block engine urls
    jito_urls: Vec<String>,
    //should enable forwards to leader
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
    use_tpu_client_next: bool,
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

    // let friendly_rpcs = config
    //     .friendly_rpcs
    //     .iter()
    //     .map(|rpc_url| Arc::new(RpcClient::new(rpc_url.to_owned())))
    //     .collect();

    let address = config.address;
    let rpc = Arc::new(RpcClient::new(config.rpc_url.to_owned()));
    let identity_keypair = config
        .identity_keypair_file
        .as_ref()
        .map(|file| read_keypair_file(file).expect("Failed to read identity keypair file"))
        .unwrap_or(Keypair::new());

    let leader_updater = create_leader_updater(rpc.clone(), config.ws_url.to_owned(), None)
        .await
        .map_err(|e| anyhow!(e))?;

    let client: Arc<dyn SendTransactionClient> = if config.use_tpu_client_next {
        log::info!("Using TpuClientNextSender");
        Arc::new(TpuClientNextSender::create_client(
            tokio::runtime::Handle::current(),
            leader_updater,
            config.enable_leader_forwards,
            config.lookahead_slots,
            identity_keypair,
        ))
    } else {
        log::info!("Using ConnectionCacheClient");
        Arc::new(ConnectionCacheClient::create_client(
            tokio::runtime::Handle::current(),
            leader_updater,
            config.enable_leader_forwards,
            config.lookahead_slots,
            identity_keypair,
        ))
    };

    let forwarder = RpcForwards::new(
        tokio::runtime::Handle::current(),
        config
            .friendly_rpcs
            .iter()
            .map(|rpc_url| Arc::new(RpcClient::new(rpc_url.to_owned())))
            .collect(),
        config.jito_urls,
    );
    let iris = IrisRpcServerImpl::new(
        client,
        Arc::new(store::TransactionStoreImpl::new()),
        Arc::new(forwarder),
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
