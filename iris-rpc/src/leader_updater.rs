use solana_sdk::clock::NUM_CONSECUTIVE_LEADER_SLOTS;
use solana_tpu_client_next::leader_updater::LeaderUpdater;
use std::net::SocketAddr;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use async_trait::async_trait;
use tokio_util::sync::CancellationToken;
use leader_scheduler::leader_scheduler::LeaderScheduler;

pub struct LeaderUpdaterImpl {
    leader_tpu_service: LeaderScheduler,
    stop: CancellationToken,
}

impl LeaderUpdaterImpl {
}

#[async_trait]
impl LeaderUpdater for LeaderUpdaterImpl {
    fn next_leaders(&mut self, lookahead_leaders: usize) -> Vec<SocketAddr> {
        let lookahead_slots =
            (lookahead_leaders as u64).saturating_mul(NUM_CONSECUTIVE_LEADER_SLOTS);
        self.leader_tpu_service.leader_tpu_sockets(lookahead_slots)
    }

    async fn stop(&mut self) {
        self.stop.cancel();
        self.leader_tpu_service.join().await;
    }
}
