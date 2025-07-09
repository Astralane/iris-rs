use crate::errors::Error;
use crate::leader_cache::LeaderTpuCache;
use futures::StreamExt;
use smart_rpc_client::pubsub::SmartPubsubClient;
use smart_rpc_client::rpc_provider::SmartRpcClientProvider;
use solana_client::client_error::ClientError;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_client::rpc_request::RpcError;
use solana_client::rpc_response::SlotUpdate;
use solana_sdk::commitment_config::CommitmentConfig;
use solana_sdk::pubkey::Pubkey;
use solana_tpu_client::tpu_client::MAX_FANOUT_SLOTS;
use std::collections::VecDeque;
use std::net::SocketAddr;
use std::str::FromStr;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::time::{interval, timeout};
use tokio_util::sync::CancellationToken;
use tracing::{error, warn};

// 48 chosen because it's unlikely that 12 leaders in a row will miss their slots
const MAX_SLOT_SKIP_DISTANCE: u64 = 48;
/// Default number of slots used to build TPU socket fanout set
pub const DEFAULT_FANOUT_SLOTS: u64 = 12;

pub const CLUSTER_NODES_REFRESH_INTERVAL: Duration = Duration::from_secs(5 * 60);
pub const LEADER_CACHE_REFRESH_INTERVAL: Duration = Duration::from_secs(60);
pub const TPU_LEADER_SERVICE_CREATION_TIMEOUT: Duration = Duration::from_secs(20);
pub struct LeaderScheduler {
    leader_tpu_cache: Arc<RwLock<LeaderTpuCache>>,
    recent_leader_slots: RecentLeaderSlots,
    handle: Option<tokio::task::JoinHandle<()>>,
}

impl LeaderScheduler {
    pub async fn new(
        rpc_provider: Arc<SmartRpcClientProvider>,
        ws_urls: &[String],
        cancellation_token: CancellationToken,
    ) -> Result<Self, Error> {
        let rpc_client = rpc_provider.best_rpc();
        let pub_sub_client = SmartPubsubClient::new(&ws_urls).await?;
        let current_slot = rpc_client
            .get_slot_with_commitment(CommitmentConfig::processed())
            .await?;
        let recent_leader_slots = RecentLeaderSlots::new(current_slot);

        let epoch_schedule = rpc_client.get_epoch_schedule().await?;
        let epoch = epoch_schedule.get_epoch(current_slot);
        let slots_in_epoch = epoch_schedule.get_slots_in_epoch(epoch);
        let last_slot_in_epoch = epoch_schedule.get_last_slot_in_epoch(epoch);

        let fanout_leaders =
            Self::fetch_fanout_slot_leaders(&rpc_client, current_slot, slots_in_epoch).await?;

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
            last_slot_in_epoch,
            fanout_leaders,
            leader_node_info,
        )));

        let handle = tokio::spawn(Self::run(
            recent_leader_slots.clone(),
            leader_tpu_cache.clone(),
            rpc_provider.clone(),
            pub_sub_client,
            cancellation_token.clone(),
        ));

        Ok(Self {
            leader_tpu_cache,
            recent_leader_slots,
            handle: Some(handle),
        })
    }

    // When a new epoch is starting, we observe an invalid slot range failure that goes away after a
    // retry. It seems as if the leader schedule is not available, but it should be. The logic
    // below retries the RPC call in case of an invalid slot range error.
    pub async fn fetch_fanout_slot_leaders(
        rpc_client: &RpcClient,
        start_slot: u64,
        slots_in_epoch: u64,
    ) -> Result<Vec<Pubkey>, crate::errors::Error> {
        let retry_interval = Duration::from_secs(1);
        Ok(timeout(TPU_LEADER_SERVICE_CREATION_TIMEOUT, async {
            loop {
                match rpc_client
                    .get_slot_leaders(start_slot, LeaderTpuCache::fanout(slots_in_epoch))
                    .await
                {
                    Ok(leaders) => return Ok(leaders),
                    Err(client_error) => {
                        if is_invalid_slot_range_error(&client_error) {
                            tokio::time::sleep(retry_interval).await;
                            continue;
                        } else {
                            return Err(client_error);
                        }
                    }
                }
            }
        })
        .await
        .map_err(|e| Error::Custom(format!("{:?}", e)))??)
    }

    async fn run(
        recent_leader_slots: RecentLeaderSlots,
        tpu_cache: Arc<RwLock<LeaderTpuCache>>,
        rpc_provider: Arc<SmartRpcClientProvider>,
        pubsub_client: SmartPubsubClient,
        cancellation_token: CancellationToken,
    ) {
        let res = tokio::try_join!(
            Self::run_tpu_cache_refresh(
                &recent_leader_slots,
                tpu_cache,
                rpc_provider,
                cancellation_token.clone()
            ),
            Self::run_slot_listener(&recent_leader_slots, pubsub_client, cancellation_token)
        );
        if let Err(e) = res {
            error!("LeaderScheduler encountered an error: {:?}", e);
        }
    }
    pub fn estimated_current_slot(&self) -> u64 {
        self.recent_leader_slots.estimated_current_slot()
    }

    pub fn leader_tpu_sockets(&self, fanout_slots: u64) -> Vec<SocketAddr> {
        let current_slot = self.recent_leader_slots.estimated_current_slot();
        self.leader_tpu_cache
            .read()
            .unwrap()
            .get_leader_sockets(current_slot, fanout_slots)
    }

    pub async fn join(&mut self) {
        if let Some(handle) = self.handle.take() {
            handle.await.unwrap();
        }
    }

    async fn run_tpu_cache_refresh(
        recent_slots: &RecentLeaderSlots,
        tpu_cache: Arc<RwLock<LeaderTpuCache>>,
        rpc_provider: Arc<SmartRpcClientProvider>,
        cancellation: CancellationToken,
    ) -> Result<(), Error> {
        let mut cluster_nodes_update_interval = interval(CLUSTER_NODES_REFRESH_INTERVAL);
        let mut leader_nodes_update_interval = interval(LEADER_CACHE_REFRESH_INTERVAL);
        loop {
            tokio::select! {
                _ = cancellation.cancelled() => {
                    break;
                }
                _ = cluster_nodes_update_interval.tick() => {
                    let rpc_client = rpc_provider.best_rpc();
                    let maybe_cluster_nodes = rpc_client.get_cluster_nodes().await.ok();
                    if let Some(cluster_nodes) = maybe_cluster_nodes {
                        let mut leader_tpu_cache = tpu_cache.write().unwrap();
                        leader_tpu_cache.update_cluster_nodes(cluster_nodes);
                    }
                }
                _ = leader_nodes_update_interval.tick() => {
                    let rpc_client = rpc_provider.best_rpc();
                    let estimated_current_slot = recent_slots.estimated_current_slot();
                    let ( last_slot, slots_in_epoch, epoch_boundary_slot) = {
                        let leader_tpu_cache = tpu_cache.read().unwrap();
                        leader_tpu_cache.get_slot_info()
                    };
                    let maybe_epoch_schedule = if estimated_current_slot >= epoch_boundary_slot {
                        rpc_client.get_epoch_schedule().await.ok()
                    } else {
                        None
                    };

                    let maybe_slot_leaders = if estimated_current_slot >= last_slot.saturating_sub(MAX_FANOUT_SLOTS){
                        Some(
                            Self::fetch_fanout_slot_leaders(
                                &rpc_client,
                                estimated_current_slot,
                                slots_in_epoch,
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

                    if let Some(epoch_schedule) = maybe_epoch_schedule {
                        let mut leader_tpu_cache = tpu_cache.write().unwrap();
                        leader_tpu_cache.update_epoch_schedule(estimated_current_slot, epoch_schedule);
                    }
                }
            }
        }
        Ok(())
    }

    async fn run_slot_listener(
        recent_leader_slots: &RecentLeaderSlots,
        pubsub_client: SmartPubsubClient,
        cancellation: CancellationToken,
    ) -> Result<(), Error> {
        let (mut stream, unsub) = pubsub_client.slot_updates_subscribe().await?;
        loop {
            let slot_update = tokio::select! {
                 _ = cancellation.cancelled() => {
                    println!("Cancellation requested, exiting slot listener.");
                    break;
                }
                maybe_slot = stream.next() => {
                    match maybe_slot {
                        Some(slot) => slot,
                        None => {
                            warn!("Slot stream ended unexpectedly, exiting slot listener.");
                            break;
                        }
                    }
                }
            };

            let slot = match slot_update {
                SlotUpdate::FirstShredReceived { slot, .. } => slot,
                SlotUpdate::Completed { slot, .. } => slot + 1,
                _ => continue,
            };

            recent_leader_slots.record_slot(slot);
        }
        unsub().await;
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

fn is_invalid_slot_range_error(client_error: &ClientError) -> bool {
    if let solana_rpc_client_api::client_error::ErrorKind::RpcError(RpcError::RpcResponseError {
        code,
        message,
        ..
    }) = client_error.kind()
    {
        return *code == -32602
            && message.contains("Invalid slot range: leader schedule for epoch");
    }
    false
}
