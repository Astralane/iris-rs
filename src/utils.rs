use crate::chain_listener::ChainListener;
use crate::store::{TransactionData, TransactionStore};
use solana_client::rpc_client::SerializableTransaction;
use solana_sdk::signature::Keypair;
use solana_tpu_client_next::leader_updater::LeaderUpdater;
use std::sync::Arc;
use std::time::Duration;
use tokio::runtime::Handle;

pub const LOCAL_RPC_URL: &str = "local";

pub trait SendTransactionClient: Send + Sync {
    fn send_transaction(&self, txn: TransactionData);
}

pub trait CreateClient: SendTransactionClient {
    fn create_client(
        maybe_runtime: Handle,
        leader_updater: Box<dyn LeaderUpdater>,
        enable_leader_sends: bool,
        leader_forward_count: u64,
        validator_identity: Keypair,
        txn_store: Arc<dyn TransactionStore>,
    ) -> Self;
}