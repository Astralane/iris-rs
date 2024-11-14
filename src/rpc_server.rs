use crate::rpc::IrisRpcServer;
use crate::store::{TransactionData, TransactionStore};
use crate::transaction_client::SendTransactionClient;
use crate::vendor::solana_rpc::decode_and_deserialize;
use jsonrpsee::core::{async_trait, RpcResult};
use jsonrpsee::types::error::INVALID_PARAMS_CODE;
use jsonrpsee::types::ErrorObjectOwned;
use solana_rpc_client_api::config::RpcSendTransactionConfig;
use solana_sdk::transaction::VersionedTransaction;
use solana_transaction_status::UiTransactionEncoding;
use std::sync::Arc;
use tokio::time::Instant;

pub struct IrisRpcServerImpl {
    pub txn_sender: Arc<dyn SendTransactionClient>,
    pub store: Arc<dyn TransactionStore>,
}

pub fn invalid_request(reason: &str) -> ErrorObjectOwned {
    ErrorObjectOwned::owned(
        INVALID_PARAMS_CODE,
        format!("Invalid Request: {reason}"),
        None::<String>,
    )
}

impl IrisRpcServerImpl {
    pub fn new(
        txn_sender: Arc<dyn SendTransactionClient>,
        store: Arc<dyn TransactionStore>,
    ) -> Self {
        Self { txn_sender, store }
    }
}
#[async_trait]
impl IrisRpcServer for IrisRpcServerImpl {
    async fn health(&self) -> String {
        "OK".to_string()
    }

    async fn send_transaction(
        &self,
        txn: String,
        params: RpcSendTransactionConfig,
    ) -> RpcResult<String> {
        let sent_at = Instant::now();
        let encoding = params.encoding.unwrap_or(UiTransactionEncoding::Base58);
        if !params.skip_preflight {
            return Err(invalid_request("running preflight check is not supported"));
        }
        let binary_encoding = encoding.into_binary_encoding().ok_or_else(|| {
            invalid_request(&format!(
                "unsupported encoding: {encoding}. Supported encodings: base58, base64"
            ))
        })?;
        let (wire_transaction, versioned_transaction) =
            match decode_and_deserialize::<VersionedTransaction>(txn, binary_encoding) {
                Ok((wire_transaction, versioned_transaction)) => {
                    (wire_transaction, versioned_transaction)
                }
                Err(e) => {
                    return Err(invalid_request(&e.to_string()));
                }
            };
        let signature = versioned_transaction.signatures[0].to_string();
        // if self.transaction_store.has_signature(&signature) {
        //     return Ok(signature);
        // }
        let transaction = TransactionData {
            wire_transaction,
            versioned_transaction,
            sent_at,
            retry_count: 0,
            max_retries: 0,
        };
        self.txn_sender.send_transaction(transaction);
        Ok(signature)
    }
}
