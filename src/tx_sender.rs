use crate::store::TransactionData;

pub trait SendTransactionClient: Send + Sync {
    fn send_transaction(&self, txn: TransactionData);
}
