use anyhow::Context;
use async_trait::async_trait;
use leader_scheduler::leader_scheduler::LeaderScheduler;
use smart_rpc_client::rpc_provider::SmartRpcClientProvider;
use solana_sdk::clock::NUM_CONSECUTIVE_LEADER_SLOTS;
use solana_tpu_client_next::leader_updater::LeaderUpdater;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

pub struct LeaderUpdaterImpl {
    leader_tpu_service: LeaderScheduler,
    cancel: CancellationToken,
}

impl LeaderUpdaterImpl {
    pub async fn new(
        provider: Arc<SmartRpcClientProvider>,
        ws_urls: &[String],
    ) -> anyhow::Result<Self> {
        let cancel = CancellationToken::new();
        let leader_tpu_service = LeaderScheduler::new(provider, ws_urls, cancel.clone())
            .await
            .context("cannot create LeaderScheduler")?;
        Ok(Self {
            leader_tpu_service,
            cancel,
        })
    }
}

#[async_trait]
impl LeaderUpdater for LeaderUpdaterImpl {
    fn next_leaders(&mut self, lookahead_leaders: usize) -> Vec<SocketAddr> {
        let lookahead_slots =
            (lookahead_leaders as u64).saturating_mul(NUM_CONSECUTIVE_LEADER_SLOTS);
        self.leader_tpu_service.leader_tpu_sockets(lookahead_slots)
    }

    async fn stop(&mut self) {
        self.cancel.cancel();
        self.leader_tpu_service.join().await;
    }
}
