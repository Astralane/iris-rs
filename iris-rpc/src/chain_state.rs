use crate::utils::{generate_random_string, ChainStateClient};
use anyhow::Context;
use dashmap::DashMap;
use futures_util::{SinkExt, StreamExt};
use log::error;
use metrics::gauge;
use smart_rpc_client::pubsub::SmartPubsubClient;
use solana_rpc_client_api::config::{RpcBlockSubscribeConfig, RpcBlockSubscribeFilter};
use solana_rpc_client_api::response::SlotUpdate;
use solana_sdk::commitment_config::CommitmentConfig;
use solana_sdk::signature::Signature;
use solana_transaction_status::TransactionDetails::Signatures;
use solana_transaction_status::UiTransactionEncoding::Base64;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::runtime;
use tokio::task::JoinHandle;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};
use yellowstone_grpc_client::GeyserGrpcClient;
use yellowstone_grpc_proto::geyser::{CommitmentLevel, SubscribeRequestFilterBlocks};
use yellowstone_grpc_proto::prelude::subscribe_update::UpdateOneof;
use yellowstone_grpc_proto::prelude::{SubscribeRequest, SubscribeRequestPing};

const RETRY_INTERVAL: u64 = 1000;
const TIMEOUT_DURATION: Duration = Duration::from_secs(20);

macro_rules! ping_request {
    () => {
        SubscribeRequest {
            ping: Some(SubscribeRequestPing { id: 1 }),
            ..Default::default()
        }
    };
}

macro_rules! block_subscribe_request {
    () => {
        SubscribeRequest {
            blocks: HashMap::from_iter(vec![(
                generate_random_string(20).to_string(),
                SubscribeRequestFilterBlocks {
                    account_include: vec![],
                    include_transactions: Some(true),
                    include_accounts: Some(false),
                    include_entries: Some(false),
                },
            )]),
            commitment: Some(CommitmentLevel::Confirmed as i32),
            ..Default::default()
        }
    };
}

//Signature and slot number the transaction was confirmed
type SignatureStore = DashMap<String, u64>;

pub struct ChainStateWsClient {
    slot: Arc<AtomicU64>,
    // signature -> (block_time, slot)
    signature_store: Arc<SignatureStore>,
}

impl ChainStateWsClient {
    pub fn spawn_new(
        cancel: CancellationToken,
        retain_slot_count: u64,
        ws_urls: Vec<String>,
        grpc_url: Option<String>,
    ) -> (Self, std::thread::JoinHandle<()>) {
        let current_slot = Arc::new(AtomicU64::new(0));
        let signature_store = Arc::new(DashMap::new());

        let handle = std::thread::Builder::new()
            .name("chain-updater-t".to_string())
            .spawn({
                let current_slot = current_slot.clone();
                let signature_store = signature_store.clone();
                let cancel = cancel.clone();
                move || {
                    let rt = runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .unwrap();
                    let block_listener_hdl = if let Some(grpc_url) = grpc_url {
                        spawn_grpc_block_listener(
                            rt.handle().clone(),
                            cancel.clone(),
                            signature_store.clone(),
                            retain_slot_count,
                            grpc_url,
                        )
                    } else {
                        spawn_ws_block_listener(
                            rt.handle().clone(),
                            cancel.clone(),
                            signature_store.clone(),
                            retain_slot_count,
                            ws_urls.clone(),
                        )
                    };

                    let slot_listener_hdl = spawn_ws_slot_listener(
                        rt.handle().clone(),
                        cancel.clone(),
                        current_slot.clone(),
                        ws_urls,
                    );

                    let res = rt.block_on(futures::future::try_join(
                        async { block_listener_hdl.await? },
                        async { slot_listener_hdl.await? },
                    ));
                    if let Err(err) = &res {
                        cancel.cancel();
                        panic!("unexpected error in chain-updater-t: {}", err.to_string());
                    }
                }
            })
            .unwrap();

        while current_slot.load(Ordering::Relaxed) == 0 && !cancel.is_cancelled() {
            info!("waiting for first slot update for startup");
            std::thread::sleep(Duration::from_millis(RETRY_INTERVAL));
        }

        let this = Self {
            slot: current_slot,
            signature_store,
        };

        (this, handle)
    }
}

impl ChainStateClient for ChainStateWsClient {
    fn get_slot(&self) -> u64 {
        self.slot.load(Ordering::Relaxed)
    }

    fn confirm_signature_status(&self, signature: &str) -> Option<u64> {
        self.signature_store.get(signature).map(|v| *v)
    }
}
fn spawn_ws_block_listener(
    handle: tokio::runtime::Handle,
    cancel: CancellationToken,
    signature_store: Arc<SignatureStore>,
    retain_slot_count: u64,
    ws_url: Vec<String>,
) -> JoinHandle<anyhow::Result<()>> {
    handle.spawn(async move {
        let ws_client = SmartPubsubClient::new(&ws_url)
            .await
            .context("error creating ws_client")?;
        let config = Some(RpcBlockSubscribeConfig {
            commitment: Some(CommitmentConfig::confirmed()),
            encoding: Some(Base64),
            transaction_details: Some(Signatures),
            show_rewards: Some(false),
            max_supported_transaction_version: Some(0),
        });
        info!("Subscribing to ws block updates");
        let subscription = ws_client
            .block_subscribe(RpcBlockSubscribeFilter::All, config)
            .await?;
        let (mut stream, unsub) = subscription;
        loop {
            let block = tokio::select! {
                block = tokio::time::timeout(TIMEOUT_DURATION, stream.next()) => match block {
                    Ok(Some(block)) => block,
                    Ok(None) => return Err(anyhow::Error::msg("block stream closed unexpectedly")),
                    Err(_) => return Err(anyhow::Error::msg("block stream timed out")),
                },
                _ = cancel.cancelled() => {
                   break;
                },
            };
            debug!("Block update");
            gauge!("iris_current_block").set(block.value.slot as f64);

            let block_update = block.value;
            if let Some(block) = block_update.block {
                let slot = block_update.slot;
                let _block_time = block.block_time;
                debug!("Block update: {:?}", slot);
                if let Some(signatures) = block.signatures {
                    for signature in signatures {
                        signature_store.insert(signature, slot);
                    }
                }
                // remove old signatures to prevent leak of memory < slot - retain_slot_count
                signature_store.retain(|_, v| *v > slot - retain_slot_count);
                gauge!("iris_signature_store_size").set(signature_store.len() as f64);
            }
        }
        unsub().await;
        Ok(())
    })
}

fn spawn_ws_slot_listener(
    handle: tokio::runtime::Handle,
    cancel: CancellationToken,
    current_slot: Arc<AtomicU64>,
    ws_urls: Vec<String>,
) -> JoinHandle<anyhow::Result<()>> {
    handle.spawn(async move {
        let ws_client = SmartPubsubClient::new(&ws_urls)
            .await
            .context("cannot connect to ws rpc node")?;
        let (mut stream, unsub) = ws_client.slot_updates_subscribe().await?;
        loop {
            let slot_update = tokio::select! {
                update = tokio::time::timeout(TIMEOUT_DURATION, stream.next()) => match update {
                    Ok(Some(update)) => update,
                    Ok(None) => return Err(anyhow::Error::msg("block stream closed unexpectedly")),
                    Err(_) => {
                        error!("ws slot stream timed out");
                        return Err(anyhow::Error::msg("block stream timed out"))
                    },
                },
                _ = cancel.cancelled() => {
                    break;
                },
            };
            debug!("Slot update: {:?}", slot_update);
            let slot = match slot_update {
                SlotUpdate::FirstShredReceived { slot, .. } => slot,
                SlotUpdate::Completed { slot, .. } => slot.saturating_add(1),
                _ => continue,
            };
            debug!("Slot update: {}", slot);
            gauge!("iris_current_slot").set(slot as f64);
            current_slot.store(slot, Ordering::SeqCst);
        }
        unsub().await;
        Ok(())
    })
}

fn spawn_grpc_block_listener(
    handle: tokio::runtime::Handle,
    cancel: CancellationToken,
    signature_store: Arc<SignatureStore>,
    retain_slot_count: u64,
    endpoint: String,
) -> JoinHandle<anyhow::Result<()>> {
    let max_retries = 10;
    const RETRY_INTERVAL: Duration = Duration::from_secs(1);
    const TIMEOUT_INTERVAL: Duration = Duration::from_secs(20);
    handle.spawn(async move {
        let mut conn_retries = 0;
        loop {
            conn_retries += 1;
            if conn_retries > max_retries {
                error!("Max retries reached, shutting down geyser grpc block listener");
                return Err(anyhow::Error::msg(
                    "Max retries reached, shutting down geyser grpc block listener",
                ));
            }

            let client = GeyserGrpcClient::build_from_shared(endpoint.clone());
            if let Err(e) = client {
                error!("Error creating geyser grpc client: {:?}", e);
                tokio::time::sleep(RETRY_INTERVAL).await;
                continue;
            }

            let client = client
                .unwrap()
                .max_encoding_message_size(64 * 1024 * 1024)
                .max_decoding_message_size(64 * 1024 * 1024);

            let connection = client.connect().await;
            if let Err(e) = connection {
                error!("Error connecting to geyser grpc: {:?}", e);
                tokio::time::sleep(RETRY_INTERVAL).await;
                continue;
            }

            let subscription = connection.unwrap().subscribe().await;
            if let Err(e) = subscription {
                error!("Error subscribing to geyser grpc: {:?}", e);
                tokio::time::sleep(RETRY_INTERVAL).await;
                continue;
            }

            let (mut grpc_tx, mut grpc_rx) = subscription.unwrap();
            info!("Subscribing to grpc block updates..");
            if let Err(e) = grpc_tx.send(block_subscribe_request!()).await {
                error!("Error sending subscription request: {:?}", e);
                tokio::time::sleep(RETRY_INTERVAL).await;
                continue;
            }
            //connection established successfully, reset retries for connection
            conn_retries = 0;
            'event_loop: loop {
                let update = tokio::select! {
                    maybe_update = timeout(TIMEOUT_INTERVAL, grpc_rx.next()) => match maybe_update {
                        Ok(Some(update)) => update,
                        _ => {
                            //try reconnecting again`
                             break 'event_loop;
                        }
                    },
                    _ = cancel.cancelled() => {
                        return Ok(())
                    }
                };
                match update {
                    Ok(message) => match message.update_oneof {
                        Some(UpdateOneof::Block(block)) => {
                            let slot = block.slot;
                            debug!("Block update: {:?}", slot);
                            for transaction in block.transactions {
                                let signature = Signature::try_from(transaction.signature)
                                    .expect("Invalid signature");
                                signature_store.insert(signature.to_string(), slot);
                            }
                            // remove old signatures to prevent leak of memory < slot - retain_slot_count
                            signature_store.retain(|_, v| *v > slot - retain_slot_count);
                            gauge!("iris_signature_store_size").set(signature_store.len() as f64);
                        }
                        Some(UpdateOneof::Ping(_)) => {
                            if let Err(e) = grpc_tx.send(ping_request!()).await {
                                error!("Error sending ping: {}", e);
                                break 'event_loop;
                            }
                        }
                        Some(UpdateOneof::Pong(_)) => {}
                        _ => {
                            debug!("Unknown message type");
                        }
                    },
                    Err(e) => {
                        error!("Error block updates subscription {:?}", e);
                        break 'event_loop;
                    }
                }
            }
        }
    })
}
