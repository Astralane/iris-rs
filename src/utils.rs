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

pub async fn transaction_retry_loop(
    txn_store: Arc<dyn TransactionStore>,
    sender: Arc<dyn SendTransactionClient>,
    chain_listener: Arc<ChainListener>,
    retry_interval_secs: u32,
    max_retries: usize,
    max_queue_size: usize,
) {
    loop {
        let retry_interval = Duration::from_secs(retry_interval_secs as u64);
        let transaction_map = txn_store.get_transactions();
        let queue_size = transaction_map.len();

        //shed by most retried first
        if queue_size > max_queue_size {
            let mut transactions: Vec<(String, TransactionData)> = transaction_map
                .iter()
                .map(|x| (x.key().to_owned(), x.value().to_owned()))
                .collect();
            transactions.sort_by(|(_, a), (_, b)| a.retry_count.cmp(&b.retry_count));
            let transactions_to_remove = transactions[(max_queue_size + 1)..].to_vec();
            for (signature, _) in transactions_to_remove {
                txn_store.remove_transaction(signature.clone());
                transaction_map.remove(&signature);
            }
        }

        // remove transactions where last send was more than 1 minute ago
        let now = std::time::Instant::now();
        let transactions: Vec<(String, TransactionData)> = transaction_map
            .iter()
            .map(|x| (x.key().to_owned(), x.value().to_owned()))
            .collect();
        let transactions_to_remove: Vec<String> = transactions
            .iter()
            .filter_map(|(signature, data)| {
                if now.duration_since(data.sent_at).as_secs() > 60 {
                    Some(signature.clone())
                } else {
                    None
                }
            })
            .collect();

        for signature in transactions_to_remove {
            txn_store.remove_transaction(signature.clone());
            transaction_map.remove(&signature);
        }

        for mut data in transaction_map.iter_mut() {
            let signature = data.versioned_transaction.get_signature().to_string();
            if data.retry_count >= max_retries || chain_listener.confirm_signature(&signature) {
                txn_store.remove_transaction(signature);
                continue;
            }
            data.retry_count += 1;
            sender.send_transaction(data.clone());
        }

        tokio::time::sleep(retry_interval).await;
    }
}
