use crate::rpc::IrisRpcServer;
use crate::store::{TransactionData, TransactionStore};
use crate::utils::{ChainStateClient, SendTransactionClient};
use crate::vendor::solana_rpc::decode_and_deserialize;
use jsonrpsee::core::{async_trait, RpcResult};
use jsonrpsee::types::error::INVALID_PARAMS_CODE;
use jsonrpsee::types::ErrorObjectOwned;
use metrics::{counter, gauge, histogram};
use solana_client::rpc_client::SerializableTransaction;
use solana_rpc_client_api::config::RpcSendTransactionConfig;
use solana_sdk::transaction::VersionedTransaction;
use solana_transaction_status::UiTransactionEncoding;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;
use tracing::info;

pub struct IrisRpcServerImpl {
    txn_sender: Arc<dyn SendTransactionClient>,
    store: Arc<dyn TransactionStore>,
    chain_state: Arc<dyn ChainStateClient>,
    retry_interval: Duration,
    max_retries: u32,
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
        retry_interval: Duration,
        shutdown: Arc<AtomicBool>,
        max_retries: u32,
    ) -> Self {
        let client = IrisRpcServerImpl {
            txn_sender,
            store,
            chain_state,
            retry_interval,
            max_retries,
        };
        client.spawn_retry_transactions_loop(shutdown);
        client
    }

    fn spawn_retry_transactions_loop(&self, shutdown: Arc<AtomicBool>) {
        let store = self.store.clone();
        let chain_state = self.chain_state.clone();
        let txn_sender = self.txn_sender.clone();
        let retry_interval = self.retry_interval;
        let max_retries = self.max_retries;

        tokio::spawn(async move {
            loop {
                if shutdown.load(std::sync::atomic::Ordering::Relaxed) {
                    break;
                }

                let transactions_map = store.get_transactions();
                let mut transactions_to_remove = vec![];
                let mut transactions_to_send = vec![];
                gauge!("iris_retry_transactions").set(transactions_map.len() as f64);

                for mut txn in transactions_map.iter_mut() {
                    if let Some(slot) = chain_state.confirm_signature_status(&txn.key()) {
                        info!(
                            "Transaction confirmed at slot: {slot} latency {:}",
                            slot.saturating_sub(txn.slot)
                        );
                        counter!("iris_txn_landed").increment(1);
                        histogram!("iris_txn_slot_latency")
                            .record(slot.saturating_sub(txn.slot) as f64);
                        transactions_to_remove.push(txn.key().clone());
                    }
                    //check if transaction has been in the store for too long
                    if txn.value().sent_at.elapsed() > Duration::from_secs(60) {
                        transactions_to_remove.push(txn.key().clone());
                    }
                    //check if max retries has been reached
                    if txn.retry_count >= max_retries as usize {
                        transactions_to_remove.push(txn.key().clone());
                    }
                    txn.retry_count += 1;
                    transactions_to_send.push(txn.wire_transaction.clone());
                }

                gauge!("iris_transactions_removed").increment(transactions_to_remove.len() as f64);
                for signature in transactions_to_remove {
                    store.remove_transaction(signature);
                }

                for batches in transactions_to_send.chunks(10) {
                    txn_sender.send_transaction_batch(batches.to_vec());
                }

                tokio::time::sleep(retry_interval).await;
            }
        });
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
        info!("Received transaction on rpc connection loop");
        if self.store.has_signature(&txn) {
            counter!("iris_error", "type" => "duplicate_transaction").increment(1);
            return Err(invalid_request("duplicate transaction"));
        }
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

    async fn send_transaction_batch(
        &self,
        batch: Vec<String>,
        params: RpcSendTransactionConfig,
    ) -> RpcResult<Vec<String>> {
        if batch.len() > 10 {
            counter!("iris_error", "type" => "batch_size_exceeded").increment(1);
            return Err(invalid_request("batch size exceeded"));
        }
        counter!("iris_txn_total_batches").increment(1);
        let mut wired_transactions = Vec::new();
        let mut signatures = Vec::new();
        for txn in batch {
            if self.store.has_signature(&txn) {
                counter!("iris_error", "type" => "duplicate_transaction_in_batch").increment(1);
                return Err(invalid_request("duplicate transaction"));
            }
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
            wired_transactions.push(transaction.wire_transaction);
            signatures.push(signature);
        }
        self.txn_sender.send_transaction_batch(wired_transactions);
        Ok(signatures)
    }
}
