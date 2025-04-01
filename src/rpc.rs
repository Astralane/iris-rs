use jsonrpsee::core::RpcResult;
use jsonrpsee::proc_macros::rpc;
use serde::{Deserialize, Serialize};
use solana_rpc_client_api::config::RpcSendTransactionConfig;

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "kebab-case")]
pub struct VersionResponse {
    pub solana_core: String,
    pub feature_set: u64
}

#[rpc(server)]
pub trait IrisRpc {
    #[method(name = "health")]
    async fn health(&self) -> String;
    #[method(name = "sendTransaction")]
    async fn send_transaction(
        &self,
        txn: String,
        params: RpcSendTransactionConfig,
    ) -> RpcResult<String>;

    #[method(name = "sendTransactionBatch")]
    async fn send_transaction_batch(
        &self,
        txns: Vec<String>,
        params: RpcSendTransactionConfig,
    ) -> RpcResult<Vec<String>>;

    #[method(name = "getVersion")]
    async fn get_version(
        &self
    ) -> RpcResult<VersionResponse>;
}
