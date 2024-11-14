use crate::store::TransactionData;
use solana_sdk::signature::Keypair;
use solana_tpu_client_next::leader_updater::LeaderUpdater;
use tokio::runtime::Handle;

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
    ) -> Self;
}
