use jsonrpsee::core::RpcResult;
use jsonrpsee::proc_macros::rpc;
use solana_rpc_client_api::config::RpcSendTransactionConfig;

#[rpc(server)]
pub trait IrisRpc {
    #[method(name = "health")]
    async fn health(&self) -> String;
    #[method(name = "sendTransaction")]
    async fn send_transaction(
        &self,
        txn: String,
        params: RpcSendTransactionConfig,
        mev_protect: Option<bool>,
    ) -> RpcResult<String>;

    #[method(name = "sendTransactionBatch")]
    async fn send_transaction_batch(
        &self,
        txns: Vec<String>,
        params: RpcSendTransactionConfig,
        mev_protect: Option<bool>,
    ) -> RpcResult<Vec<String>>;
}
