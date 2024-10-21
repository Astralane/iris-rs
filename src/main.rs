use crate::rpc::IrisRpcServer;
use crate::rpc_server::IrisRpcServerImpl;
use crate::transaction_sender::TpuClientNextSender;
use figment::providers::Env;
use figment::Figment;
use jsonrpsee::server::ServerBuilder;
use serde::{Deserialize, Serialize};
use solana_client::nonblocking::rpc_client::RpcClient;
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
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenv::dotenv().ok();
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

    let rpc = RpcClient::new(config.rpc_url);
    let tpu_sender_client = TpuClientNextSender::new(rpc, config.ws_url, None).await;
    let iris = IrisRpcServerImpl::new(
        Arc::new(tpu_sender_client),
        Arc::new(store::TransactionStoreImpl::new()),
    );
    let server = ServerBuilder::default()
        .max_request_body_size(15_000_000)
        .max_connections(1_000_000)
        .build(config.address)
        .await
        .unwrap();
    let server_hdl = server.start(iris.into_rpc());
    server_hdl.stopped().await;
    Ok(())
}
