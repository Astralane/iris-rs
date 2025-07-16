use async_trait::async_trait;
use std::net::SocketAddr;

pub trait SendTransactionClient: Send + Sync {
    fn send_transaction(&self, txn: Vec<u8>, mev_protect: bool);
    fn send_transaction_batch(&self, wire_transaction: Vec<Vec<u8>>, mev_protect: bool);
}

pub trait ChainStateClient: Send + Sync {
    fn get_slot(&self) -> u64;
    fn confirm_signature_status(&self, signature: &str) -> Option<u64>;
}

#[async_trait]
pub trait BlockListProvider: Send + 'static {
    async fn fetch_blocked_leaders(&self) -> anyhow::Result<Vec<SocketAddr>>;
}
