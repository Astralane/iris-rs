use crate::shield::YellowstoneShieldProvider;
use arc_swap::ArcSwap;
use async_trait::async_trait;
use once_cell::sync::Lazy;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::pubkey::Pubkey;
use solana_tpu_client_next::connection_workers_scheduler::WorkersBroadcaster;
use solana_tpu_client_next::transaction_batch::TransactionBatch;
use solana_tpu_client_next::workers_cache::{shutdown_worker, WorkersCache, WorkersCacheError};
use solana_tpu_client_next::ConnectionWorkersSchedulerError;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

static BLOCKED_LEADERS: Lazy<ArcSwap<Vec<SocketAddr>>> =
    Lazy::new(|| ArcSwap::from_pointee(vec![]));

const TIMEOUT: Duration = Duration::from_secs(5);

const REFRESH_LIST_DURATION: std::time::Duration = std::time::Duration::from_secs(1 * 60 * 60); // 1 hour
pub struct MevProtectedBroadcaster;
pub fn run(
    key: Pubkey,
    rpc: Arc<RpcClient>,
    cancel: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    let shield = YellowstoneShieldProvider::new(key, rpc);
    let mut interval = tokio::time::interval(REFRESH_LIST_DURATION);
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    warn!("Cancel signal received, exiting");
                    return;
                }
                _ = interval.tick() => {
                    debug!("Refreshing blocked leaders list");
                }
            }
            match timeout(TIMEOUT, shield.get_blocked_ips()).await {
                Err(_) => {
                    warn!("Timeout fetching blocked leaders");
                    continue;
                }
                Ok(Err(e)) => {
                    warn!("Failed to fetch blocked leaders {:?}", e);
                }
                Ok(Ok(blocked_leaders)) => {
                    BLOCKED_LEADERS.store(Arc::new(blocked_leaders));
                    info!("Updated blocked leaders: {:?}", BLOCKED_LEADERS.load());
                }
            }
        }
    })
}

#[async_trait]
impl WorkersBroadcaster for MevProtectedBroadcaster {
    async fn send_to_workers(
        workers: &mut WorkersCache,
        leaders: &[SocketAddr],
        transaction_batch: TransactionBatch,
    ) -> Result<(), ConnectionWorkersSchedulerError> {
        // the last element value shows if this transaction requires MEV protection,
        // the actual transactions are everything, but the last element
        // not the best way but done due to limitation on tpu-client-next.
        let tx_batch = transaction_batch.clone().into_iter();
        let Some((prefix_bytes, wire_transactions)) = tx_batch.as_slice().split_last() else {
            // nothing in the slice, nothing to send
            return Ok(());
        };
        // convert the bytes to a boolean
        let mev_protect = matches!(prefix_bytes.first(), Some(1));
        let transaction_batch = TransactionBatch::new(wire_transactions.to_vec());
        let blocked_leaders = BLOCKED_LEADERS.load().clone();

        //if the current or next leader is in the blocklist don't send the transactions
        if mev_protect {
            if let Some(leader) = leaders.first() {
                if blocked_leaders.contains(leader) {
                    return Ok(());
                }
            }
        }

        for (_, new_leader) in leaders.iter().enumerate() {
            if !workers.contains(new_leader) {
                warn!("No existing worker for {new_leader:?}, skip sending to this leader.");
                continue;
            }

            let send_res =
                workers.try_send_transactions_to_address(new_leader, transaction_batch.clone());

            match send_res {
                Ok(()) => (),
                Err(WorkersCacheError::ShutdownError) => {
                    debug!("Connection to {new_leader} was closed, worker cache shutdown");
                }
                Err(WorkersCacheError::ReceiverDropped) => {
                    // Remove the worker from the cache if the peer has disconnected.
                    if let Some(pop_worker) = workers.pop(*new_leader) {
                        shutdown_worker(pop_worker)
                    }
                }
                Err(err) => {
                    warn!("Connection to {new_leader} was closed, worker error: {err}");
                    // If we have failed to send a batch, it will be dropped.
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
pub mod test {
    use bytes::Bytes;
    use solana_tpu_client_next::transaction_batch::TransactionBatch;

    #[test]
    pub fn test_mev_protect_serialization_deserialization_case_true() {
        let test_transaction = [0u8, 128].to_vec();
        let wire_transaction: Vec<Vec<u8>> = vec![test_transaction, vec![true as u8]];
        let txn_batch = TransactionBatch::new(wire_transaction);
        let batch_iter = txn_batch.into_iter();

        let Some((mev_protect, wire_transactions)) = batch_iter.as_slice().split_last() else {
            // nothing in the slice, nothing to send
            panic!("cannot get back last elemenet")
        };
        let decoded = mev_protect.first().map(|b| *b == 1).unwrap_or(false);
        assert_eq!(true, decoded);
        assert_eq!(mev_protect, &Bytes::from_static(&[1]));
        for txn in wire_transactions {
            assert_eq!(txn, &Bytes::from_static(&[0, 128]));
        }
    }

    #[test]
    pub fn test_mev_protect_serialization_deserialization_case_false() {
        let test_transaction = [0u8, 128].to_vec();
        let wire_transaction: Vec<Vec<u8>> = vec![test_transaction, vec![false as u8]];
        let txn_batch = TransactionBatch::new(wire_transaction);
        let batch_iter = txn_batch.into_iter();

        let Some((mev_protect, wire_transactions)) = batch_iter.as_slice().split_last() else {
            // nothing in the slice, nothing to send
            panic!("cannot get back last elemenet")
        };
        let decoded = mev_protect.first().map(|b| *b == 1).unwrap_or(false);
        assert_eq!(false, decoded);
        assert_eq!(mev_protect, &Bytes::from_static(&[0]));
        for txn in wire_transactions {
            assert_eq!(txn, &Bytes::from_static(&[0, 128]));
        }
    }
}
