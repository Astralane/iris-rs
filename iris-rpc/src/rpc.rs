use jsonrpsee::core::RpcResult;
use jsonrpsee::proc_macros::rpc;
use solana_rpc_client_api::config::RpcSendTransactionConfig;
use solana_rpc_client_api::response::RpcVersionInfo;

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
    #[method(name = "getVersion")]
    async fn get_version(&self) -> RpcResult<RpcVersionInfo>;
    #[method(name = "sendTransactionBatch")]
    async fn send_transaction_batch(
        &self,
        txns: Vec<String>,
        params: RpcSendTransactionConfig,
    ) -> RpcResult<Vec<String>>;
}
