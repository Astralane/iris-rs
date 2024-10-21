use jsonrpsee::core::RpcResult;
use jsonrpsee::proc_macros::rpc;
use serde::Deserialize;
use solana_rpc_client_api::config::RpcSendTransactionConfig;

#[derive(Deserialize, Clone, Debug)]
#[serde(rename_all(serialize = "camelCase", deserialize = "camelCase"))]
pub struct RequestMetadata {
    pub api_key: String,
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
        request_metadata: Option<RequestMetadata>,
    ) -> RpcResult<String>;
}
