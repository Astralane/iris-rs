use crate::shield::YellowstoneShieldProvider;
use crate::tpu_next_client::TpuClientPayload;
use crate::transaction_stats;
use arc_swap::ArcSwap;
use async_trait::async_trait;
use bytes::Bytes;
use metrics::{counter, gauge};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::pubkey::Pubkey;
use solana_tpu_client_next::connection_workers_scheduler::WorkersBroadcaster;
use solana_tpu_client_next::transaction_batch::TransactionBatch;
use solana_tpu_client_next::workers_cache::{shutdown_worker, WorkersCache, WorkersCacheError};
use solana_tpu_client_next::ConnectionWorkersSchedulerError;
use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

const REFRESH_LIST_DURATION: Duration = Duration::from_secs(10 * 60); // 10 mins

fn has_blocked_send_target(leaders: &[SocketAddr], blocked_leaders: &HashSet<SocketAddr>) -> bool {
    leaders
        .iter()
        .any(|leader| blocked_leaders.contains(leader))
}

pub struct MevProtectedBroadcaster {
    blocked_leaders: Arc<ArcSwap<HashSet<SocketAddr>>>,
    buffered_transactions: Mutex<Vec<Bytes>>,
}

impl MevProtectedBroadcaster {
    pub fn run(
        key: Pubkey,
        rpc: Arc<RpcClient>,
        cancel: CancellationToken,
    ) -> (Self, JoinHandle<()>) {
        let shield = YellowstoneShieldProvider::new(key, rpc);
        let blocked_addrs = Arc::new(ArcSwap::from_pointee(HashSet::new()));
        let refresh_handle = std::thread::Builder::new()
            .name("mev-broadcast-refrest".to_string())
            .spawn({
                let blocked_addrs = blocked_addrs.clone();
                move || {
                    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
                    rt.block_on(async move {
                        let mut interval = tokio::time::interval(REFRESH_LIST_DURATION);
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
                                    blocked_addrs.store(Arc::new(blocked_leaders.into_iter().collect()));
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
                    })
                }
            }).unwrap();
        (
            MevProtectedBroadcaster {
                blocked_leaders: blocked_addrs,
                buffered_transactions: Mutex::new(Vec::with_capacity(512)),
            },
            refresh_handle,
        )
    }

    fn prepare_transactions(
        &self,
        transaction_batch: TransactionBatch,
        is_blocked_leader_slot: bool,
    ) -> Vec<Bytes> {
        let mut buffered = self
            .buffered_transactions
            .lock()
            .expect("MEV-protected transaction buffer lock poisoned");
        let mut ready = Vec::new();
        let mut newly_buffered = 0u64;

        for encoded in transaction_batch {
            let Some(payload) = TpuClientPayload::decode(encoded) else {
                continue;
            };
            let wire_transaction = payload.wire_transaction();
            if is_blocked_leader_slot && payload.is_mev_protected() {
                buffered.push(wire_transaction);
                newly_buffered += 1;
            } else {
                ready.push(wire_transaction);
            }
        }

        if newly_buffered > 0 {
            counter!("iris_mev_protected_buffered").increment(newly_buffered);
            transaction_stats::record_mev_buffered(newly_buffered);
        }

        if !is_blocked_leader_slot && !buffered.is_empty() {
            let released = buffered.len() as u64;
            let mut pending = std::mem::take(&mut *buffered);
            pending.append(&mut ready);
            ready = pending;
            counter!("iris_mev_protected_released").increment(released);
            transaction_stats::record_mev_released(released);
        }

        gauge!("iris_mev_protected_buffer_size").set(buffered.len() as f64);
        transaction_stats::set_mev_buffer_size(buffered.len());
        ready
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
        let blocked_leaders = self.blocked_leaders.load();
        // A protected batch must not be released if any address it will be sent
        // to belongs to a blocked leader.
        let is_blocked_leader_slot = has_blocked_send_target(leaders, &blocked_leaders);
        let batch = self.prepare_transactions(transaction_batch, is_blocked_leader_slot);

        if batch.is_empty() {
            return Ok(());
        }

        let transaction_batch = TransactionBatch::new(batch);

        for new_leader in leaders {
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
                    debug!("Failed to send transactions to {new_leader:?}, worker error: {err}");
                    // If we have failed to send a batch, it will be dropped.
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
pub mod test {
    use super::{has_blocked_send_target, MevProtectedBroadcaster};
    use arc_swap::ArcSwap;
    use bytes::Bytes;
    use solana_tpu_client_next::transaction_batch::TransactionBatch;
    use std::collections::HashSet;
    use std::net::SocketAddr;
    use std::str::FromStr;
    use std::sync::Arc;

    #[tokio::test]
    pub async fn test_fetch_blocked_validators_from_chain() {
        let rpc = Arc::new(solana_client::nonblocking::rpc_client::RpcClient::new(
            "http://rpc:8899".to_string(),
        ));
        let key =
            solana_sdk::pubkey::Pubkey::from_str("4QXuzwHutRGjMHRfpGgZpaC9LEYR2wqVmLJBbPbK1zQo")
                .unwrap();

        let shield = crate::shield::YellowstoneShieldProvider::new(key, rpc);
        let blocked_ips = shield.get_blocked_identities().await.unwrap();

        // println!("Blocked validators ({} total):", blocked_ips.len());
        for addr in &blocked_ips {
            println!("  {addr}");
        }

        assert!(
            !blocked_ips.is_empty(),
            "expected at least one blocked validator"
        );
    }

    #[test]
    pub fn test_blocked_validators_list() {
        let addrs: HashSet<SocketAddr> = ["127.0.0.1:8001", "127.0.0.1:8002", "192.168.1.10:9000"]
            .iter()
            .map(|s| SocketAddr::from_str(s).unwrap())
            .collect();

        let broadcaster = MevProtectedBroadcaster {
            blocked_leaders: Arc::new(ArcSwap::from_pointee(addrs.clone())),
            buffered_transactions: std::sync::Mutex::new(Vec::new()),
        };

        let loaded = broadcaster.blocked_leaders.load();
        let listed: HashSet<SocketAddr> = loaded.iter().copied().collect();

        assert_eq!(listed, addrs);
    }

    #[test]
    fn detects_blocked_target_anywhere_in_send_fanout() {
        let leaders = [
            SocketAddr::from_str("127.0.0.1:8001").unwrap(),
            SocketAddr::from_str("127.0.0.1:8002").unwrap(),
            SocketAddr::from_str("127.0.0.1:8003").unwrap(),
        ];
        let blocked = HashSet::from([leaders[2]]);

        assert!(has_blocked_send_target(&leaders, &blocked));
    }

    #[test]
    fn test_mev_protected_transactions_are_buffered_until_safe() {
        let broadcaster = MevProtectedBroadcaster {
            blocked_leaders: Arc::new(ArcSwap::from_pointee(HashSet::new())),
            buffered_transactions: std::sync::Mutex::new(Vec::new()),
        };
        let ready = broadcaster.prepare_transactions(
            TransactionBatch::new(vec![vec![1, 2, 3, 1], vec![4, 5, 6, 0]]),
            true,
        );
        assert_eq!(ready, vec![Bytes::from_static(&[4, 5, 6])]);
        assert_eq!(broadcaster.buffered_transactions.lock().unwrap().len(), 1);

        let ready =
            broadcaster.prepare_transactions(TransactionBatch::new(vec![vec![1, 2, 3, 1]]), false);
        assert_eq!(
            ready,
            vec![
                Bytes::from_static(&[1, 2, 3]),
                Bytes::from_static(&[1, 2, 3]),
            ]
        );
        assert!(broadcaster.buffered_transactions.lock().unwrap().is_empty());
    }

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
        assert!(decoded);
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
        assert!(!decoded);
        assert_eq!(mev_protect, &Bytes::from_static(&[0]));
        for txn in wire_transactions {
            assert_eq!(txn, &Bytes::from_static(&[0, 128]));
        }
    }
}
