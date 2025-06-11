use rand::distributions::Alphanumeric;
use rand::Rng;
use solana_sdk::signature::Keypair;
use solana_tpu_client_next::leader_updater::LeaderUpdater;
use solana_tpu_client_next::SendTransactionStats;
use std::sync::Arc;
use tokio::runtime::Handle;
use tokio::time::sleep;

pub trait SendTransactionClient: Send + Sync {
    fn send_transaction(&self, txn: Vec<u8>);
    fn send_transaction_batch(&self, wire_transaction: Vec<Vec<u8>>);
}

pub trait ChainStateClient: Send + Sync {
    fn get_slot(&self) -> u64;
    fn confirm_signature_status(&self, signature: &str) -> Option<u64>;
}

pub fn generate_random_string(len: usize) -> String {
    rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(len)
        .map(char::from)
        .collect()
}
