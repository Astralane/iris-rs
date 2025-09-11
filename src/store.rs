use dashmap::DashMap;
use solana_sdk::signature::Signature;
use std::sync::Arc;
use std::time::Instant;
use tracing::error;

#[derive(Clone, Debug)]
pub struct TransactionContext {
    pub wire_transaction: bytes::Bytes,
    pub signature: Signature,
    pub sent_at: Instant,
    pub slot: u64,
    pub retry_count: usize,
    pub mev_protect: bool,
}

impl TransactionContext {
    pub fn new(
        wire_transaction: bytes::Bytes,
        signature: Signature,
        slot: u64,
        retry_count: usize,
        mev_protect: bool,
    ) -> Self {
        Self {
            wire_transaction,
            signature,
            sent_at: Instant::now(),
            slot,
            retry_count,
            mev_protect,
        }
    }
}

pub trait TransactionStore: Send + Sync {
    fn add_transaction(&self, transaction: TransactionContext);
    fn remove_transaction(&self, signature: String) -> Option<TransactionContext>;
    fn get_transactions(&self) -> Arc<DashMap<String, TransactionContext>>;
    fn has_signature(&self, signature: &str) -> bool;
}

pub struct TransactionStoreImpl {
    transactions: Arc<DashMap<String, TransactionContext>>,
}

impl TransactionStoreImpl {
    pub fn new() -> Self {
        let transaction_store = Self {
            transactions: Arc::new(DashMap::new()),
        };
        transaction_store
    }
}

impl TransactionStore for TransactionStoreImpl {
    fn add_transaction(&self, transaction: TransactionContext) {
        if let Some(signature) = get_signature(&transaction) {
            if self.transactions.contains_key(&signature) {
                return;
            }
            self.transactions.insert(signature.to_string(), transaction);
        } else {
            error!("Transaction has no signatures");
        }
    }
    fn remove_transaction(&self, signature: String) -> Option<TransactionContext> {
        let transaction = self.transactions.remove(&signature);
        transaction.map_or(None, |t| Some(t.1))
    }
    fn get_transactions(&self) -> Arc<DashMap<String, TransactionContext>> {
        self.transactions.clone()
    }
    fn has_signature(&self, signature: &str) -> bool {
        self.transactions.contains_key(signature)
    }
}

pub fn get_signature(transaction: &TransactionContext) -> Option<String> {
    transaction.signature.to_string().parse().ok()
}
