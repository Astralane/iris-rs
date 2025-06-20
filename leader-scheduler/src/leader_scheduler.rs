use crate::errors::Error;
use crate::rpc_provider::RpcProvider;
use std::collections::VecDeque;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::sync::mpsc::Receiver;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

// 48 chosen because it's unlikely that 12 leaders in a row will miss their slots
const MAX_SLOT_SKIP_DISTANCE: u64 = 48;

pub struct LeaderScheduler {
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
        let current_slot = rpc_provider.get_rpc_client().get_slot().await?;
        let recent_leader_slots = RecentLeaderSlots::new(current_slot);
        tokio::spawn(Self::run_slot_listener(
            recent_leader_slots.clone(),
            slot_receiver,
            cancellation_token,
        ));

        Ok(Self {
            rpc_provider,
            recent_leader_slots,
        })
    }

    pub async fn run_slot_listener(
        recent_leader_slots: RecentLeaderSlots,
        mut receiver: Receiver<u64>,
        cancellation: CancellationToken,
    ) {
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
            recent_leader_slots.record_slot(slot);
        }
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
