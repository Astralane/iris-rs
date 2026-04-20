use crate::rpc_server::invalid_request;
use async_trait::async_trait;
use jsonrpsee::core::RpcResult;
use jsonrpsee::proc_macros::rpc;
use solana_sdk::signature::{EncodableKey, Keypair};
use std::net::SocketAddr;
use std::path::PathBuf;
use tokio_util::sync::CancellationToken;
use tracing::info;

pub struct AdminRpcImpl {
    pub(crate) tpu_client: solana_tpu_client_next::Client,
}
#[rpc(client, server)]
pub trait AdminRpc {
    #[method(name = "setIdentity")]
    async fn set_identity(&self, identity: PathBuf) -> RpcResult<()>;
}

#[async_trait]
impl AdminRpcServer for AdminRpcImpl {
    async fn set_identity(&self, path: PathBuf) -> RpcResult<()> {
        let keypair = Keypair::read_from_file(&path)
            .map_err(|e| invalid_request(&format!("Failed to read keypair: {}", e)))?;
        self.tpu_client
            .update_identity(&keypair)
            .map_err(|e| invalid_request(&format!("Failed to update identity: {}", e)))?;
        Ok(())
    }
}

pub fn spawn_admin_rpc_server(
    cancel: CancellationToken,
    bind_addr: SocketAddr,
    tpu_client: solana_tpu_client_next::Client,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("admin-rpc-server".to_string())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async move {
                let admin_rpc = AdminRpcImpl { tpu_client };
                let rpc = jsonrpsee::server::ServerBuilder::default()
                    .build(bind_addr)
                    .await
                    .unwrap();
                info!("running admin server in {bind_addr}");
                let handle = rpc.start(admin_rpc.into_rpc());
                cancel.cancelled().await;
                let _ = handle.stop();
            })
        })
        .unwrap()
}
