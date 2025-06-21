use crate::errors::Error;
use crate::leader_cache::LeaderTpuCache;
use crate::rpc_provider::RpcProvider;
use solana_sdk::pubkey::Pubkey;
use solana_tpu_client::tpu_client::MAX_FANOUT_SLOTS;
use std::collections::VecDeque;
use std::str::FromStr;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::sync::mpsc::Receiver;
use tokio::time::{interval, timeout};
use tokio_util::sync::CancellationToken;
use tracing::warn;

// 48 chosen because it's unlikely that 12 leaders in a row will miss their slots
const MAX_SLOT_SKIP_DISTANCE: u64 = 48;
/// Default number of slots used to build TPU socket fanout set
pub const DEFAULT_FANOUT_SLOTS: u64 = 12;

pub const CLUSTER_NODES_REFRESH_INTERVAL: Duration = Duration::from_secs(5 * 60);
pub const LEADER_CACHE_REFRESH_INTERVAL: Duration = Duration::from_secs(60);

#[derive(Clone)]
pub struct LeaderScheduler {
    leader_tpu_cache: Arc<RwLock<LeaderTpuCache>>,
    rpc_provider: Arc<RpcProvider>,
    recent_leader_slots: RecentLeaderSlots,
}

impl LeaderScheduler {
    const SLOT_RECEIVER_TIMEOUT: Duration = Duration::from_secs(10);
    pub async fn new(
        rpc_provider: Arc<RpcProvider>,
        slot_receiver: Receiver<u64>,
        cancellation_token: CancellationToken,
    ) -> Result<Self, Error> {
        let rpc_client = rpc_provider.get_rpc_client();
        let current_slot = rpc_client.get_slot().await?;
        let recent_leader_slots = RecentLeaderSlots::new(current_slot);

        let slots_in_epoch = rpc_client.get_epoch_info().await?.slots_in_epoch;

        let fanout_leaders = rpc_client
            .get_slot_leaders(current_slot, LeaderTpuCache::fanout(slots_in_epoch))
            .await?;

        let leader_node_info = {
            let cluster_nodes = rpc_client.get_cluster_nodes().await?;
            cluster_nodes
                .into_iter()
                .filter(|node| fanout_leaders.contains(&Pubkey::from_str(&node.pubkey).unwrap()))
                .collect::<Vec<_>>()
        };

        let leader_tpu_cache = Arc::new(RwLock::new(LeaderTpuCache::new(
            current_slot,
            slots_in_epoch,
            fanout_leaders,
            leader_node_info,
        )));

        tokio::spawn(Self::run(
            recent_leader_slots.clone(),
            leader_tpu_cache.clone(),
            rpc_provider.clone(),
            slot_receiver,
            cancellation_token.clone(),
        ));

        Ok(Self {
            leader_tpu_cache,
            rpc_provider,
            recent_leader_slots,
        })
    }

    async fn run(
        recent_leader_slots: RecentLeaderSlots,
        tpu_cache: Arc<RwLock<LeaderTpuCache>>,
        rpc_provider: Arc<RpcProvider>,
        slot_receiver: Receiver<u64>,
        cancellation_token: CancellationToken,
    ) -> Result<(), Error> {
        tokio::try_join!(
            Self::run_tpu_cache_refresh(
                &recent_leader_slots,
                tpu_cache,
                rpc_provider,
                cancellation_token.clone()
            ),
            Self::run_slot_listener(&recent_leader_slots, slot_receiver, cancellation_token)
        )?;
        Ok(())
    }
    pub fn estimated_current_slot(&self) -> u64 {
        self.recent_leader_slots.estimated_current_slot()
    }

    async fn run_tpu_cache_refresh(
        recent_slots: &RecentLeaderSlots,
        tpu_cache: Arc<RwLock<LeaderTpuCache>>,
        rpc_provider: Arc<RpcProvider>,
        cancellation: CancellationToken,
    ) -> Result<(), Error> {
        let mut cluster_nodes_interval = interval(CLUSTER_NODES_REFRESH_INTERVAL);
        let mut leader_nodes_interval = interval(LEADER_CACHE_REFRESH_INTERVAL);
        loop {
            tokio::select! {
                _ = cancellation.cancelled() => {
                    break;
                }
                _ = cluster_nodes_interval.tick() => {
                    let rpc_client = rpc_provider.get_rpc_client();
                    match rpc_client.get_cluster_nodes().await {
                        Ok(cluster_nodes) => {
                            let mut tpu_cache = tpu_cache.write().unwrap();
                            tpu_cache.update_cluster_nodes(cluster_nodes);
                        }
                        Err(e) => {
                            warn!("Failed to refresh cluster nodes: {:?}", e);
                        }
                    }
                }
                _ = leader_nodes_interval.tick() => {
                    let rpc_client = rpc_provider.get_rpc_client();
                    let estimated_current_slot = recent_slots.estimated_current_slot();
                    let ( last_slot, last_slot_in_epoch, slots_in_epoch) = {
                        let leader_tpu_cache = tpu_cache.read().unwrap();
                        leader_tpu_cache.get_slot_info()
                    }
                    let maybe_slot_leaders = if estimated_current_slot >= last_slot.saturating_sub(MAX_FANOUT_SLOTS){
                        Some(
                            rpc_client.get_slot_leaders(
                                estimated_current_slot,
                                LeaderTpuCache::fanout(slots_in_epoch)
                            ).await.ok()
                        )
                    } else {
                        None
                    }
                    .flatten();

                    if let Some(slot_leaders) = maybe_slot_leaders {
                        let mut leader_tpu_cache = tpu_cache.write().unwrap();
                        leader_tpu_cache.update_leader_cache(
                            estimated_current_slot,
                            slot_leaders,
                        );
                    }

                }
            }
        }
        Ok(())
    }

    async fn run_slot_listener(
        recent_leader_slots: &RecentLeaderSlots,
        mut receiver: Receiver<u64>,
        cancellation: CancellationToken,
    ) -> Result<(), Error> {
        let mut dedup = lru_cache::LruCache::new(128);
        loop {
            let slot = tokio::select! {
                 _ = cancellation.cancelled() => {
                    println!("Cancellation requested, exiting slot listener.");
                    break;
                }
                maybe_slot = timeout(Self::SLOT_RECEIVER_TIMEOUT, receiver.recv()) => {
                    match maybe_slot {
                        Ok(Some(slot)) => slot,
                        Ok(None) => {
                            println!("Receiver closed, exiting slot listener.");
                            break;
                        }
                        Err(_) => {
                            println!("Timeout waiting for slot, retrying...");
                            continue;
                        }
                    }
                },
            };
            if !dedup.contains_key(&slot) {
                dedup.insert(slot, ());
                recent_leader_slots.record_slot(slot);
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub(crate) struct RecentLeaderSlots(Arc<RwLock<VecDeque<u64>>>);
impl RecentLeaderSlots {
    pub(crate) fn new(current_slot: u64) -> Self {
        let mut recent_slots = VecDeque::new();
        recent_slots.push_back(current_slot);
        Self(Arc::new(RwLock::new(recent_slots)))
    }

    pub(crate) fn record_slot(&self, current_slot: u64) {
        let mut recent_slots = self.0.write().unwrap();
        recent_slots.push_back(current_slot);
        // 12 recent slots should be large enough to avoid a misbehaving
        // validator from affecting the median recent slot
        while recent_slots.len() > 12 {
            recent_slots.pop_front();
        }
    }

    // Estimate the current slot from recent slot notifications.
    pub(crate) fn estimated_current_slot(&self) -> u64 {
        let mut recent_slots: Vec<u64> = self.0.read().unwrap().iter().cloned().collect();
        assert!(!recent_slots.is_empty());
        recent_slots.sort_unstable();

        // Validators can broadcast invalid blocks that are far in the future
        // so check if the current slot is in line with the recent progression.
        let max_index = recent_slots.len() - 1;
        let median_index = max_index / 2;
        let median_recent_slot = recent_slots[median_index];
        let expected_current_slot = median_recent_slot + (max_index - median_index) as u64;
        let max_reasonable_current_slot = expected_current_slot + MAX_SLOT_SKIP_DISTANCE;

        // Return the highest slot that doesn't exceed what we believe is a
        // reasonable slot.
        recent_slots
            .into_iter()
            .rev()
            .find(|slot| *slot <= max_reasonable_current_slot)
            .unwrap()
    }
}
