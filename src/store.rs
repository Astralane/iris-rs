use dashmap::DashMap;
use solana_sdk::transaction::VersionedTransaction;
use std::sync::Arc;
use std::time::Instant;
use tracing::error;

#[derive(Clone, Debug)]
pub struct TransactionData {
    pub wire_transaction: Vec<u8>,
    pub versioned_transaction: VersionedTransaction,
    pub sent_at: Instant,
    pub slot: u64,
    pub retry_count: usize,
    pub mev_protect: bool,
}

impl TransactionData {
    pub fn new(
        wire_transaction: Vec<u8>,
        versioned_transaction: VersionedTransaction,
        slot: u64,
        retry_count: usize,
        mev_protect: bool,
    ) -> Self {
        Self {
            wire_transaction,
            versioned_transaction,
            sent_at: Instant::now(),
            slot,
            retry_count,
            mev_protect,
        }
    }
}

pub trait TransactionStore: Send + Sync {
    fn add_transaction(&self, transaction: TransactionData);
    fn get_signatures(&self) -> Vec<String>;
    fn remove_transaction(&self, signature: String) -> Option<TransactionData>;
    fn get_transactions(&self) -> Arc<DashMap<String, TransactionData>>;
    fn has_signature(&self, signature: &str) -> bool;
}

pub struct TransactionStoreImpl {
    transactions: Arc<DashMap<String, TransactionData>>,
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
    fn add_transaction(&self, transaction: TransactionData) {
        if let Some(signature) = get_signature(&transaction) {
            if self.transactions.contains_key(&signature) {
                return;
            }
            self.transactions.insert(signature.to_string(), transaction);
        } else {
            error!("Transaction has no signatures");
        }
    }
    fn get_signatures(&self) -> Vec<String> {
        let signatures = self
            .transactions
            .iter()
            .map(|t| get_signature(&t).unwrap())
            .collect();
        signatures
    }
    fn remove_transaction(&self, signature: String) -> Option<TransactionData> {
        let transaction = self.transactions.remove(&signature);
        transaction.map_or(None, |t| Some(t.1))
    }
    fn get_transactions(&self) -> Arc<DashMap<String, TransactionData>> {
        self.transactions.clone()
    }
    fn has_signature(&self, signature: &str) -> bool {
        self.transactions.contains_key(signature)
    }
}

pub fn get_signature(transaction: &TransactionData) -> Option<String> {
    transaction
        .versioned_transaction
        .signatures
        .get(0)
        .map(|s| s.to_string())
}
