use crate::errors::Error;
use futures::future::BoxFuture;
use futures::StreamExt;
use solana_client::client_error::reqwest::Url;
use solana_client::nonblocking::pubsub_client::PubsubClient;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_client::rpc_response::SlotUpdate;
use solana_commitment_config::CommitmentConfig;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::runtime;
use tokio::sync::mpsc::Receiver;
use tokio::time::timeout;
use tokio_retry::Retry;
use tokio_util::sync::CancellationToken;
use tracing::error;

pub struct RpcProvider {
    url_to_slot: Arc<Vec<(Url, AtomicU64)>>,
    url_to_rpc: HashMap<Url, Arc<RpcClient>>,
    handles: Vec<tokio::task::JoinHandle<()>>,
}

impl RpcProvider {
    const DISCONNECT_WEBSOCKET_TIMEOUT: Duration = Duration::from_secs(30);
    const RPC_TIMEOUT: Duration = Duration::from_secs(15);
    const WS_RETRY_INTERVAL: Duration = Duration::from_secs(5);
    pub async fn new(
        urls: Vec<(Url, Url)>,
        cancel: CancellationToken,
    ) -> Result<(Self, Receiver<u64>), Error> {
        let url_to_rpc: HashMap<_, _> = urls
            .iter()
            .map(|(url, _)| {
                (
                    url.clone(),
                    Arc::new(RpcClient::new_with_timeout_and_commitment(
                        url.to_string(),
                        Self::RPC_TIMEOUT,
                        CommitmentConfig::processed(),
                    )),
                )
            })
            .collect();

        let url_to_slot = Arc::new(
            urls.iter()
                .map(|(url, _)| (url.clone(), AtomicU64::new(0)))
                .collect::<Vec<_>>(),
        );

        let (sender, receiver) = tokio::sync::mpsc::channel(128); // Create a channel with a buffer size of 100

        let ws_futures = url_to_slot
            .iter()
            .map(|(pubsub, _)| pubsub)
            .map(|url| PubsubClient::new(url.as_str()))
            .collect::<Vec<_>>();

        let ws_clients = futures::future::join_all(ws_futures)
            .await
            .into_iter()
            .collect::<Result<Vec<_>, _>>()?;

        let handles = ws_clients
            .into_iter()
            .enumerate()
            .map(|(i, pub_client)| {
                let cancel_clone = cancel.clone();
                let slot_sender = sender.clone();
                let url_to_slot = url_to_slot.clone();
                tokio::spawn(Self::run_subscription(
                    url_to_slot,
                    i,
                    pub_client,
                    cancel_clone,
                    slot_sender,
                ))
            })
            .collect::<Vec<_>>();
        Ok((
            Self {
                url_to_slot,
                url_to_rpc,
                handles,
            },
            receiver,
        ))
    }

    pub async fn run(
        urls_to_slot: Arc<Vec<(Url, AtomicU64)>>,
        cancel: CancellationToken,
        sender: tokio::sync::mpsc::Sender<u64>,
    ) -> Result<(), Error> {
        Ok(())
    }

    pub async fn run_subscription(
        url_to_slot: Arc<Vec<(Url, AtomicU64)>>,
        index: usize,
        pub_client: PubsubClient,
        cancel: CancellationToken,
        slot_sender: tokio::sync::mpsc::Sender<u64>,
    ) {
        loop {
            let (mut subscription, unsubscribe) = tokio::select! {
                _ = cancel.cancelled() => {
                    break;
                },
                res = pub_client.slot_updates_subscribe() => {
                    match res {
                        Ok(sub) => sub,
                        Err(e) => {
                            error!("Error subscribing to slot updates: {:?}", e);
                            tokio::time::sleep(Self::WS_RETRY_INTERVAL).await;
                            continue;
                        }
                    }
                }
            };

            loop {
                let slot_update = tokio::select! {
                    _ = cancel.cancelled() => {
                        break;
                    },
                    slot_update = timeout(Self::DISCONNECT_WEBSOCKET_TIMEOUT, subscription.next()) => {
                        match slot_update {
                            Ok(Some(update)) => update,
                            Ok(None) => break,
                            Err(e) => {
                                error!("Error receiving slot update: {:?}", e);
                                break;
                            }
                        }
                    }
                };

                let slot = match slot_update {
                    SlotUpdate::FirstShredReceived { slot, .. } => slot,
                    SlotUpdate::Completed { slot, .. } => slot + 1,
                    _ => {
                        continue;
                    }
                };

                url_to_slot[index]
                    .1
                    .store(slot, std::sync::atomic::Ordering::SeqCst);

                if let Err(e) = slot_sender.try_send(slot) {
                    error!("Failed to send completed slot update: {:?}", e);
                }
            }
            unsubscribe().await;
        }
    }

    pub fn get_url_with_best_slot(&self) -> (Url, u64) {
        let highest = self
            .url_to_slot
            .iter()
            .max_by(|lhs, rhs| {
                lhs.1
                    .load(Ordering::Relaxed)
                    .cmp(&rhs.1.load(Ordering::Relaxed))
            })
            .expect("No URLs available");
        (
            highest.0.clone(),
            highest.1.load(std::sync::atomic::Ordering::Relaxed),
        )
    }

    pub fn get_rpc_client(&self) -> Arc<RpcClient> {
        let (url, _) = self.get_url_with_best_slot();
        self.url_to_rpc
            .get(&url)
            .cloned()
            .expect("No RPC client found for the best URL")
    }
}
