use crate::shield::YellowstoneShieldProvider;
use crate::tpu_next_client::TpuClientPayload;
use arc_swap::ArcSwap;
use async_trait::async_trait;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::pubkey::Pubkey;
use solana_tpu_client_next::connection_workers_scheduler::WorkersBroadcaster;
use solana_tpu_client_next::transaction_batch::TransactionBatch;
use solana_tpu_client_next::workers_cache::{shutdown_worker, WorkersCache, WorkersCacheError};
use solana_tpu_client_next::ConnectionWorkersSchedulerError;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::task::JoinHandle;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

const REFRESH_LIST_DURATION: Duration = Duration::from_secs(10 * 60); // 10 mins

pub struct MevProtectedBroadcaster(Arc<ArcSwap<Vec<SocketAddr>>>);
impl MevProtectedBroadcaster {
    pub fn run(
        key: Pubkey,
        rpc: Arc<RpcClient>,
        cancel: CancellationToken,
    ) -> (Self, JoinHandle<()>) {
        let shield = YellowstoneShieldProvider::new(key, rpc);
        let mut interval = tokio::time::interval(REFRESH_LIST_DURATION);
        let blocked_addrs = Arc::new(ArcSwap::from_pointee(vec![]));
        let refresh_handle = tokio::spawn({
            let blocked_addrs = blocked_addrs.clone();
            async move {
                const TIMEOUT: Duration = Duration::from_secs(10);
                loop {
                    tokio::select! {
                        _ = cancel.cancelled() => {
                            warn!("Cancel signal received, exiting");
                            break;
                        }
                        _ = interval.tick() => {
                            match timeout(TIMEOUT, shield.get_blocked_ips()).await {
                                Ok(Ok(blocked_leaders)) => {
                                    blocked_addrs.store(Arc::new(blocked_leaders));
                                    info!("Updated blocked leaders: {:?}", blocked_addrs.load());
                                }
                                Ok(Err(e)) => {
                                    warn!("Failed to fetch blocked leaders {:?}", e);
                                }
                                Err(_) => {
                                    warn!("Timeout fetching blocked leaders");
                                    continue;
                                }
                            }
                        }
                    }
                }
                info!("Exiting blocked leaders refresh task");
            }
        });
        (MevProtectedBroadcaster(blocked_addrs), refresh_handle)
    }
}

#[async_trait]
impl WorkersBroadcaster for MevProtectedBroadcaster {
    async fn send_to_workers(
        &self,
        workers: &mut WorkersCache,
        leaders: &[SocketAddr],
        transaction_batch: TransactionBatch,
    ) -> Result<(), ConnectionWorkersSchedulerError> {
        let blocked_leaders = self.0.load();
        let is_blocked_leader_slot = leaders.first().is_some_and(|l| blocked_leaders.contains(l));

        let batch = if is_blocked_leader_slot {
            transaction_batch
                .into_iter()
                .filter_map(TpuClientPayload::decode)
                .into_iter()
                .filter(|payload| !payload.is_mev_protected())
                .map(|payload| payload.wire_transaction())
                .collect()
        } else {
            transaction_batch
                .into_iter()
                .filter_map(TpuClientPayload::decode)
                .map(|payload| payload.wire_transaction())
                .collect()
        };

        let transaction_batch = TransactionBatch::new(batch);

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
