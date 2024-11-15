use crate::rpc::IrisRpcServer;
use crate::rpc_forwards::{RpcForwards, RpcForwardsClient};
use crate::store::{TransactionData, TransactionStore};
use crate::transaction_client::SendTransactionClient;
use crate::vendor::solana_rpc::decode_and_deserialize;
use jsonrpsee::core::{async_trait, RpcResult};
use jsonrpsee::types::error::INVALID_PARAMS_CODE;
use jsonrpsee::types::ErrorObjectOwned;
use solana_rpc_client_api::config::RpcSendTransactionConfig;
use solana_sdk::transaction::VersionedTransaction;
use solana_transaction_status::UiTransactionEncoding;
use std::sync::{Arc, RwLock};
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::time::Instant;
use tracing::info;

pub struct Stat {
    pub tx_count: u64,
}

pub struct IrisRpcServerImpl {
    pub txn_sender: Arc<dyn SendTransactionClient>,
    pub store: Arc<dyn TransactionStore>,
    pub forwarder: Arc<dyn RpcForwardsClient>,
    pub stats: AtomicU64
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
        rpc_forwards: Arc<dyn RpcForwardsClient>,
    ) -> Self {
        Self {
            txn_sender,
            store,
            stats: AtomicU64::new(0),
            forwarder: rpc_forwards,
        }
    }
}
#[async_trait]
impl IrisRpcServer for IrisRpcServerImpl {
    async fn health(&self) -> String {
        "Ok".to_string()
    }

    async fn send_bundle(&self, bundle: Vec<String>) -> RpcResult<String> {
        let resp = self.forwarder.forward_to_jito_clients(bundle).await;
        match resp {
            Ok(_) => Ok("Ok".to_string()),
            Err(e) => Err(invalid_request(&e.to_string())),
        }
    }
    async fn send_transaction(
        &self,
        txn: String,
        params: RpcSendTransactionConfig,
    ) -> RpcResult<String> {
        let sent_at = Instant::now();
        info!("transaction count {:?}", self.stats.fetch_add(1, Ordering::SeqCst));
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
        let transaction = TransactionData {
            wire_transaction,
            versioned_transaction,
            sent_at,
            retry_count: 0,
            max_retries: 0,
        };
        self.forwarder.forward_to_known_rpcs(transaction.clone());
        self.txn_sender.send_transaction(transaction);
        Ok(signature)
    }
}
