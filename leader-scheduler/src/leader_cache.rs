use solana_client::rpc_response::RpcContactInfo;
use solana_sdk::clock::NUM_CONSECUTIVE_LEADER_SLOTS;
use solana_sdk::epoch_schedule::EpochSchedule;
use solana_sdk::pubkey::Pubkey;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::str::FromStr;
use tracing::warn;

/// Maximum number of slots to fan out TPU sockets for a leader
const MAX_FANOUT_SLOTS: u64 = 100;

pub struct LeaderTpuCache {
    pub(crate) first_slot: u64,
    // contains leaders for first_slot to first_slot + MAX_FANOUT_SLOTS
    pub(crate) leaders: Vec<Pubkey>,
    // Maps leader pubkey to its TPU-QUIC socket address
    pub(crate) leader_tpu_map: HashMap<Pubkey, SocketAddr>,
    pub(crate) slots_in_epoch: u64,
    //slot where the epoch ends
    pub(crate) last_slot_in_epoch: u64,
}

impl LeaderTpuCache {
    pub fn new(
        first_slot: u64,
        slots_in_epoch: u64,
        epoch_boundary_slot: u64,
        leaders: Vec<Pubkey>,
        cluster_nodes: Vec<RpcContactInfo>,
    ) -> Self {
        let leader_tpu_map = Self::extract_cluster_tpu_sockets(cluster_nodes);
        Self {
            first_slot,
            leaders,
            leader_tpu_map,
            slots_in_epoch,
            last_slot_in_epoch: epoch_boundary_slot,
        }
    }

    // returns the last slot in the cache, and the last slot in the epoch
    pub fn get_slot_info(&self) -> (u64, u64, u64) {
        (
            self.last_slot(),
            self.slots_in_epoch,
            self.last_slot_in_epoch,
        )
    }

    pub(crate) fn extract_cluster_tpu_sockets(
        cluster_nodes: Vec<RpcContactInfo>,
    ) -> HashMap<Pubkey, SocketAddr> {
        cluster_nodes
            .into_iter()
            .filter_map(|node| {
                if let Some(tpu_addr) = node.tpu_quic {
                    if let Ok(pubkey) = Pubkey::from_str(&node.pubkey) {
                        return Some((pubkey, tpu_addr));
                    }
                }
                None
            })
            .collect()
    }

    fn get_slot_leader(&self, slot: u64) -> Option<&Pubkey> {
        if slot < self.first_slot {
            return None;
        }
        let index = (slot - self.first_slot) as usize;
        self.leaders.get(index)
    }

    fn last_slot(&self) -> u64 {
        self.first_slot + self.leaders.len().saturating_sub(1) as u64
    }

    // Get the TPU sockets for the current leader and upcoming leaders according to fanout size.
    pub(crate) fn get_leader_sockets(
        &self,
        estimated_current_slot: u64,
        fanout_slots: u64,
    ) -> Vec<SocketAddr> {
        let mut leader_sockets = Vec::new();
        for leader_slot in (estimated_current_slot..estimated_current_slot + fanout_slots)
            .step_by(NUM_CONSECUTIVE_LEADER_SLOTS as usize)
        {
            if let Some(leader) = self.get_slot_leader(leader_slot) {
                if let Some(socket) = self.leader_tpu_map.get(leader) {
                    leader_sockets.push(*socket);
                } else {
                    warn!("TPU not found for leader: {}", leader);
                }
            } else {
                // not found in the local leader cache, this can happen if the local tpu cache has not been refreshed
                warn!(
                    "Leader not known for slot {}; cache holds slots [{},{}]",
                    leader_slot,
                    self.first_slot,
                    self.last_slot()
                );
            }
        }
        leader_sockets
    }

    pub fn fanout(slots_in_epoch: u64) -> u64 {
        (2 * MAX_FANOUT_SLOTS).min(slots_in_epoch)
    }

    pub fn update_cluster_nodes(&mut self, cluster_nodes: Vec<RpcContactInfo>) {
        self.leader_tpu_map = Self::extract_cluster_tpu_sockets(cluster_nodes);
    }

    pub fn update_leader_cache(&mut self, first_slot: u64, leaders: Vec<Pubkey>) {
        self.first_slot = first_slot;
        self.leaders = leaders;
    }

    pub fn update_epoch_schedule(&mut self, current_slot: u64, epoch_info: EpochSchedule) {
        self.slots_in_epoch = epoch_info.get_slots_in_epoch(current_slot);
        self.last_slot_in_epoch = epoch_info.get_last_slot_in_epoch(current_slot);
    }
}
