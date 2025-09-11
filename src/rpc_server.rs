use crate::rpc::IrisRpcServer;
use crate::store::{TransactionContext, TransactionStore};
use crate::utils::{ChainStateClient, SendTransactionClient};
use crate::vendor::solana_rpc::decode_transaction;
use agave_transaction_view::transaction_view::TransactionView;
use jsonrpsee::core::{async_trait, RpcResult};
use jsonrpsee::types::error::INVALID_PARAMS_CODE;
use jsonrpsee::types::ErrorObjectOwned;
use metrics::{counter, gauge, histogram};
use moka::future::{Cache, CacheBuilder};
use moka::policy::EvictionPolicy;
use solana_rpc_client_api::config::RpcSendTransactionConfig;
use solana_sdk::signature::Signature;
use solana_transaction_status::UiTransactionEncoding;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;
use tracing::{error, info};

pub struct IrisRpcServerImpl {
    txn_sender: Arc<dyn SendTransactionClient>,
    retry_cache: Arc<dyn TransactionStore>,
    chain_state: Arc<dyn ChainStateClient>,
    dedup_cache: Cache<Signature, ()>,
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
        dedup_cache_size: usize,
    ) -> Self {
        let client = IrisRpcServerImpl {
            txn_sender,
            retry_cache: store,
            chain_state,
            dedup_cache: CacheBuilder::new(dedup_cache_size as u64)
                .eviction_policy(EvictionPolicy::lru())
                .build(),
            retry_interval,
            max_retries,
        };
        client.spawn_retry_transactions_loop(shutdown);
        client
    }

    fn spawn_retry_transactions_loop(&self, shutdown: Arc<AtomicBool>) {
        let store = self.retry_cache.clone();
        let chain_state = self.chain_state.clone();
        let txn_sender = self.txn_sender.clone();
        let retry_interval = self.retry_interval;

        tokio::spawn(async move {
            loop {
                if shutdown.load(std::sync::atomic::Ordering::Relaxed) {
                    break;
                }

                let transactions_map = store.get_transactions();
                let mut to_remove = vec![];
                let mut to_retry = vec![];
                let mut to_retry_mev_protected = vec![];
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
                        to_remove.push(txn.key().clone());
                    }
                    //check if transaction has been in the store for too long
                    if txn.value().sent_at.elapsed() > Duration::from_secs(60) {
                        to_remove.push(txn.key().clone());
                    }
                    //check if max retries has been reached
                    if txn.retry_count == 0usize {
                        to_remove.push(txn.key().clone());
                    }
                    if txn.retry_count > 0usize {
                        if !txn.mev_protect {
                            to_retry.push(txn.wire_transaction.clone());
                        } else {
                            to_retry_mev_protected.push(txn.wire_transaction.clone());
                        }
                    }
                    txn.retry_count = txn.retry_count.saturating_sub(1);
                }

                gauge!("iris_transactions_removed").increment(to_remove.len() as f64);
                for signature in to_remove {
                    store.remove_transaction(signature);
                }

                if !to_retry.is_empty() {
                    info!("retrying {} tranasctions", to_retry.iter().len());
                }

                if !to_retry_mev_protected.is_empty() {
                    info!(
                        "retrying {} mev protected transactions",
                        to_retry_mev_protected.iter().len()
                    );
                }

                for batch in to_retry.chunks(10).clone() {
                    txn_sender.send_transaction_batch(batch.to_vec(), false);
                }
                for batch in to_retry_mev_protected.chunks(10).clone() {
                    txn_sender.send_transaction_batch(batch.to_vec(), true);
                }

                tokio::time::sleep(retry_interval).await;
            }
        });
    }
}
#[async_trait]
impl IrisRpcServer for IrisRpcServerImpl {
    async fn health(&self) -> String {
        "Ok(1.2)".to_string()
    }

    async fn send_transaction(
        &self,
        txn: String,
        params: RpcSendTransactionConfig,
        mev_protect: Option<bool>,
    ) -> RpcResult<String> {
        info!("Received transaction on rpc connection loop");
        counter!("iris_txn_total_transactions").increment(1);
        let mev_protect = mev_protect.unwrap_or(false);
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
        let wire_transaction = match decode_transaction(txn, binary_encoding) {
            Ok(wire_transaction) => wire_transaction,
            Err(e) => {
                counter!("iris_error", "type" => "cannot_decode_transaction").increment(1);
                error!("cannot decode transaction: {:?}", e);
                return Err(invalid_request(&e.to_string()));
            }
        };
        let tx_view =
            TransactionView::try_new_unsanitized(wire_transaction.as_ref()).map_err(|e| {
                counter!("iris_error", "type" => "cannot_deserialize_transaction").increment(1);
                error!("cannot deserialize transaction: {:?}", e);
                invalid_request("cannot deserialize transaction")
            })?;
        let signature = tx_view.signatures()[0].clone();
        if self.dedup_cache.contains_key(&signature) {
            counter!("iris_error", "type" => "duplicate_transaction").increment(1);
            return Err(invalid_request("duplicate transaction"));
        }
        info!("processing transaction with signature: {signature}");
        let slot = self.chain_state.get_slot();
        let transaction = TransactionContext::new(
            wire_transaction,
            signature,
            slot,
            params.max_retries.unwrap_or(self.max_retries as usize),
            mev_protect,
        );
        // add to store
        self.retry_cache.add_transaction(transaction.clone());
        self.dedup_cache.insert(signature, ()).await;
        self.txn_sender
            .send_transaction(transaction.wire_transaction, mev_protect);
        Ok(signature.to_string())
    }

    async fn send_transaction_batch(
        &self,
        batch: Vec<String>,
        params: RpcSendTransactionConfig,
        mev_protect: Option<bool>,
    ) -> RpcResult<Vec<String>> {
        let mev_protect = mev_protect.unwrap_or(false);
        if batch.len() > 10 {
            counter!("iris_error", "type" => "batch_size_exceeded").increment(1);
            return Err(invalid_request("batch size exceeded"));
        }
        counter!("iris_txn_total_batches").increment(1);
        let mut wired_transactions = Vec::new();
        let mut signatures = Vec::new();
        for txn in batch {
            if self.retry_cache.has_signature(&txn) {
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
            let wire_transaction = match decode_transaction(txn, binary_encoding) {
                Ok(wire_transaction) => wire_transaction,
                Err(e) => {
                    counter!("iris_error", "type" => "cannot_decode_transaction").increment(1);
                    error!("cannot decode transaction: {:?}", e);
                    return Err(invalid_request("cannot decode transaction"));
                }
            };
            let tx_view =
                TransactionView::try_new_unsanitized(wire_transaction.as_ref()).map_err(|e| {
                    counter!("iris_error", "type" => "cannot_deserialize_transaction").increment(1);
                    error!("cannot deserialize transaction: {:?}", e);
                    invalid_request("cannot deserialize transaction")
                })?;

            let signature = tx_view.signatures()[0].clone();
            if self.dedup_cache.contains_key(&signature) {
                counter!("iris_error", "type" => "duplicate_transaction").increment(1);
                return Err(invalid_request("duplicate transaction"));
            }
            let slot = self.chain_state.get_slot();
            let transaction = TransactionContext::new(
                wire_transaction,
                signature,
                slot,
                params.max_retries.unwrap_or(self.max_retries as usize),
                mev_protect,
            );
            // add to store
            self.dedup_cache.insert(signature, ()).await;
            self.retry_cache.add_transaction(transaction.clone());
            wired_transactions.push(transaction.wire_transaction);
            signatures.push(signature.to_string());
        }
        self.txn_sender
            .send_transaction_batch(wired_transactions, mev_protect);
        Ok(signatures)
    }
}
