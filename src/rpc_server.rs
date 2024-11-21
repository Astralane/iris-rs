use crate::rpc::IrisRpcServer;
use crate::store::{TransactionData, TransactionStore};
use crate::utils::{ChainStateClient, SendTransactionClient};
use crate::vendor::solana_rpc::decode_and_deserialize;
use jsonrpsee::core::{async_trait, RpcResult};
use jsonrpsee::types::error::INVALID_PARAMS_CODE;
use jsonrpsee::types::ErrorObjectOwned;
use metrics::{counter, histogram};
use solana_client::rpc_client::SerializableTransaction;
use solana_rpc_client_api::config::RpcSendTransactionConfig;
use solana_sdk::transaction::VersionedTransaction;
use solana_transaction_status::UiTransactionEncoding;
use std::sync::Arc;
use std::time::Duration;
use tracing::info;

pub struct IrisRpcServerImpl {
    txn_sender: Arc<dyn SendTransactionClient>,
    store: Arc<dyn TransactionStore>,
    chain_state: Arc<dyn ChainStateClient>,
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
        chain_state: Arc<dyn ChainStateClient>,
    ) -> Self {
        Self {
            txn_sender,
            store,
            chain_state,
        }
    }

    async fn spawn_retry_transactions_loop(&self, retry_interval: Duration) {
        loop {
            let transactions_map = self.store.get_transactions();
            let mut transactions_to_remove = vec![];
            let mut transactions_to_send = vec![];

            for mut txn in transactions_map.iter_mut() {
                if let Some(slot) = self.chain_state.confirm_signature_status(&txn.key()) {
                    histogram!("iris_txn_slot_latency")
                        .record(slot.saturating_sub(txn.slot) as f64);
                    transactions_to_remove.push(txn.key().clone());
                }
                //check if transaction has been in the store for too long
                if txn.value().sent_at.elapsed() > Duration::from_secs(60) {
                    transactions_to_remove.push(txn.key().clone());
                }
                //check if max retries has been reached
                if txn.value().retry_count >= txn.retry_count {
                    transactions_to_remove.push(txn.key().clone());
                }
                txn.retry_count += 1;
                transactions_to_send.push(txn.wire_transaction.clone());
            }

            for signature in transactions_to_remove {
                self.store.remove_transaction(signature);
            }

            for batches in transactions_to_send.chunks(10) {
                self.txn_sender.send_transaction_batch(batches.to_vec());
            }

            tokio::time::sleep(retry_interval).await;
        }
    }
}
#[async_trait]
impl IrisRpcServer for IrisRpcServerImpl {
    async fn health(&self) -> String {
        "Ok".to_string()
    }

    async fn send_transaction(
        &self,
        txn: String,
        params: RpcSendTransactionConfig,
    ) -> RpcResult<String> {
        if self.store.has_signature(&txn) {
            counter!("iris_error", "type" => "duplicate_transaction").increment(1);
            return Err(invalid_request("duplicate transaction"));
        }
        info!("Received transaction on rpc connection loop");
        counter!("iris_txn_total_transactions").increment(1);
        let encoding = params.encoding.unwrap_or(UiTransactionEncoding::Base58);
        if !params.skip_preflight {
            counter!("iris_error", "type" => "preflight_check").increment(1);
            return Err(invalid_request("running preflight check is not supported"));
        }
        let binary_encoding = encoding.into_binary_encoding().ok_or_else(|| {
            counter!("iris_error", "type" => "invalid_encoding").increment(1);
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
                    counter!("iris_error", "type" => "cannot_decode_transaction").increment(1);
                    return Err(invalid_request(&e.to_string()));
                }
            };
        let signature = versioned_transaction.get_signature().to_string();
        let slot = self.chain_state.get_slot();
        let transaction = TransactionData::new(wire_transaction, versioned_transaction, slot);
        // add to store
        self.store.add_transaction(transaction.clone());
        self.txn_sender
            .send_transaction(transaction.wire_transaction);
        Ok(signature)
    }
}
