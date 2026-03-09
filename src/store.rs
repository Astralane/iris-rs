use crate::types::PacketSource;
use dashmap::DashMap;
use solana_sdk::signature::Signature;
use std::sync::Arc;
use std::time::Instant;

#[derive(Clone, Debug)]
pub struct TransactionContext {
    pub wire_transaction: bytes::Bytes,
    pub source: PacketSource,
    pub signature: Signature,
    pub received_ts: Instant,
    pub slot: u64,
    pub retry_count: u8,
    pub mev_protect: bool,
}

#[derive(Clone)]
pub struct TransactionStoreImpl {
    transactions: Arc<DashMap<Signature, TransactionContext>>,
}

impl TransactionStoreImpl {
    pub fn new() -> Self {
        let transaction_store = Self {
            transactions: Arc::new(DashMap::new()),
        };
        transaction_store
    }
}

impl TransactionStoreImpl {
    pub(crate) fn add_transaction(&self, transaction: TransactionContext) {
        self.transactions.insert(transaction.signature, transaction);
    }
    pub(crate) fn remove_transaction(&self, signature: Signature) -> Option<TransactionContext> {
        let transaction = self.transactions.remove(&signature);
        transaction.map( |t| t.1)
    }
    pub(crate) fn get_transactions(&self) -> Arc<DashMap<Signature, TransactionContext>> {
        self.transactions.clone()
    }
    pub(crate) fn get_transaction(
        &self,
        signature: &Signature,
    ) -> Option<dashmap::mapref::one::Ref<'_, Signature, TransactionContext>> {
        self.transactions.get(signature)
    }
}
