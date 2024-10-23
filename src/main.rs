use crate::rpc::IrisRpcServer;
use crate::rpc_server::IrisRpcServerImpl;
use crate::transaction_sender::TpuClientNextSender;
use figment::providers::Env;
use figment::Figment;
use jsonrpsee::server::ServerBuilder;
use serde::{Deserialize, Serialize};
use std::fmt::Debug;
use std::net::SocketAddr;
use std::sync::Arc;
use tracing::info;
use tracing_subscriber::EnvFilter;

mod rpc;
mod rpc_server;
mod store;
mod transaction_sender;
mod vendor;

#[derive(Debug, Serialize, Deserialize)]
pub struct Config {
    rpc_url: String,
    ws_url: String,
    address: SocketAddr,
    bind: SocketAddr,
    #[serde(default)]
    identity_keypair_file: Option<String>,
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

    let address = config.address;
    let tpu_sender_client = TpuClientNextSender::new(config).await;
    let iris = IrisRpcServerImpl::new(
        Arc::new(tpu_sender_client),
        Arc::new(store::TransactionStoreImpl::new()),
    );
    let server = ServerBuilder::default()
        .max_request_body_size(15_000_000)
        .max_connections(1_000_000)
        .build(address)
        .await
        .unwrap();
    info!("server starting in {:?}", address);
    let server_hdl = server.start(iris.into_rpc());
    server_hdl.stopped().await;
    Ok(())
}
