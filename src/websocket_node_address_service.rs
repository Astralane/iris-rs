use anyhow::Context;
use async_trait::async_trait;
use futures_util::{Stream, StreamExt};
use solana_client::nonblocking::{pubsub_client::PubsubClient, rpc_client::RpcClient};
use solana_rpc_client_api::response::SlotUpdate;
use solana_tpu_client_next::{
    leader_updater::LeaderUpdater,
    node_address_service::{LeaderTpuCacheServiceConfig, NodeAddressService, SlotEvent},
};
use std::{net::SocketAddr, sync::Arc, time::Duration};
use thiserror::Error;
use tokio::{
    sync::mpsc::{self, error::SendTimeoutError, Sender},
    task::JoinHandle,
};
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

const WEBSOCKET_STALL_TIMEOUT: Duration = Duration::from_secs(30);
const SLOT_EVENT_SEND_TIMEOUT: Duration = Duration::from_millis(100);

pub struct FailFastWebsocketNodeAddressService {
    service: NodeAddressService,
    websocket_task: Option<JoinHandle<()>>,
}

impl FailFastWebsocketNodeAddressService {
    pub async fn run(
        rpc_client: Arc<RpcClient>,
        websocket_url: String,
        config: LeaderTpuCacheServiceConfig,
        cancel: CancellationToken,
    ) -> anyhow::Result<Self> {
        let (slot_events, websocket_task) =
            websocket_slot_event_stream(websocket_url, cancel.clone());
        let service = NodeAddressService::run(rpc_client, slot_events, config, cancel)
            .await
            .context("cannot start node address service")?;

        Ok(Self {
            service,
            websocket_task: Some(websocket_task),
        })
    }

    async fn shutdown(&mut self) -> anyhow::Result<()> {
        self.service.shutdown().await?;
        if let Some(handle) = self.websocket_task.take() {
            handle.await.context("websocket task panicked")?;
        }
        Ok(())
    }
}

#[async_trait]
impl LeaderUpdater for FailFastWebsocketNodeAddressService {
    fn next_leaders(&mut self, lookahead_leaders: usize) -> Vec<SocketAddr> {
        self.service.next_leaders(lookahead_leaders)
    }

    async fn stop(&mut self) {
        if let Err(error) = self.shutdown().await {
            error!(?error, "failed to shut down websocket node address service");
        }
    }
}

fn websocket_slot_event_stream(
    websocket_url: String,
    cancel: CancellationToken,
) -> (ReceiverStream<SlotEvent>, JoinHandle<()>) {
    let (sender, receiver) = mpsc::channel(256);
    let task = tokio::spawn(async move {
        if let Err(error) = run_websocket_subscription(websocket_url, sender, cancel).await {
            error!(
                ?error,
                timeout_seconds = WEBSOCKET_STALL_TIMEOUT.as_secs(),
                "slot-update websocket failed; terminating Iris"
            );
            std::process::abort();
        }
    });
    (ReceiverStream::new(receiver), task)
}

async fn run_websocket_subscription(
    websocket_url: String,
    sender: Sender<SlotEvent>,
    cancel: CancellationToken,
) -> anyhow::Result<()> {
    let pubsub_client = PubsubClient::new(websocket_url)
        .await
        .context("cannot connect to slot-update websocket")?;
    let (mut notifications, unsubscribe) = pubsub_client
        .slot_updates_subscribe()
        .await
        .context("cannot subscribe to slot updates")?;

    loop {
        let Some(update) =
            next_notification(&mut notifications, &cancel, WEBSOCKET_STALL_TIMEOUT).await?
        else {
            break;
        };

        // Every raw websocket notification resets the watchdog. Only the slot
        // updates needed for leader tracking are forwarded to the cache.
        let Some(event) = map_websocket_update_to_slot_event(update) else {
            continue;
        };

        match sender.send_timeout(event, SLOT_EVENT_SEND_TIMEOUT).await {
            Ok(()) => {}
            Err(SendTimeoutError::Closed(_)) if cancel.is_cancelled() => break,
            Err(SendTimeoutError::Closed(_)) => {
                anyhow::bail!("slot-event consumer unexpectedly stopped")
            }
            Err(SendTimeoutError::Timeout(_)) => {
                warn!("slot-event consumer is not keeping up");
            }
        }
    }

    // The notification stream borrows the client and must be dropped first.
    drop(notifications);
    unsubscribe().await;
    pubsub_client
        .shutdown()
        .await
        .context("cannot shut down slot-update websocket")?;
    info!("slot-update websocket stopped");
    Ok(())
}

#[derive(Debug, Error, PartialEq, Eq)]
enum NotificationError {
    #[error("slot-update websocket subscription ended")]
    Closed,
    #[error("slot-update websocket received no events for {0:?}")]
    Stalled(Duration),
}

async fn next_notification<S, T>(
    notifications: &mut S,
    cancel: &CancellationToken,
    stall_timeout: Duration,
) -> Result<Option<T>, NotificationError>
where
    S: Stream<Item = T> + Unpin,
{
    tokio::select! {
        _ = cancel.cancelled() => Ok(None),
        result = tokio::time::timeout(stall_timeout, notifications.next()) => {
            match result {
                Ok(Some(notification)) => Ok(Some(notification)),
                Ok(None) => Err(NotificationError::Closed),
                Err(_) => Err(NotificationError::Stalled(stall_timeout)),
            }
        }
    }
}

fn map_websocket_update_to_slot_event(update: SlotUpdate) -> Option<SlotEvent> {
    match update {
        SlotUpdate::FirstShredReceived { slot, .. } => Some(SlotEvent::Start(slot)),
        SlotUpdate::Completed { slot, .. } => Some(SlotEvent::End(slot)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::stream;

    #[tokio::test]
    async fn notification_is_returned_before_deadline() {
        let mut notifications = stream::iter([42]);
        let cancel = CancellationToken::new();

        let result =
            next_notification(&mut notifications, &cancel, Duration::from_millis(20)).await;

        assert_eq!(result, Ok(Some(42)));
    }

    #[tokio::test]
    async fn cancellation_is_not_a_failure() {
        let mut notifications = stream::pending::<()>();
        let cancel = CancellationToken::new();
        cancel.cancel();

        let result =
            next_notification(&mut notifications, &cancel, Duration::from_millis(20)).await;

        assert_eq!(result, Ok(None));
    }

    #[tokio::test]
    async fn closed_subscription_is_a_failure() {
        let mut notifications = stream::empty::<()>();
        let cancel = CancellationToken::new();

        let result =
            next_notification(&mut notifications, &cancel, Duration::from_millis(20)).await;

        assert_eq!(result, Err(NotificationError::Closed));
    }

    #[tokio::test]
    async fn silent_subscription_stalls() {
        let mut notifications = stream::pending::<()>();
        let cancel = CancellationToken::new();
        let timeout = Duration::from_millis(10);

        let result = next_notification(&mut notifications, &cancel, timeout).await;

        assert_eq!(result, Err(NotificationError::Stalled(timeout)));
    }
}
