use crate::types::{generate_random_string, ChainStateClient};
use dashmap::DashMap;
use futures_util::{SinkExt, StreamExt};
use metrics::gauge;
use solana_client::nonblocking::pubsub_client::PubsubClient;
use solana_rpc_client_api::config::{RpcBlockSubscribeConfig, RpcBlockSubscribeFilter};
use solana_rpc_client_api::response::SlotUpdate;
use solana_sdk::signature::Signature;
use solana_transaction_status::TransactionDetails::Signatures;
use solana_transaction_status::UiTransactionEncoding::Base64;
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::task::JoinHandle;
use tokio::time::timeout;
use tracing::{debug, error, info, warn};
use yellowstone_grpc_client::GeyserGrpcClient;
use yellowstone_grpc_proto::geyser::{CommitmentLevel, SubscribeRequestFilterBlocks};
use yellowstone_grpc_proto::prelude::subscribe_update::UpdateOneof;
use yellowstone_grpc_proto::prelude::{SubscribeRequest, SubscribeRequestPing};

const TIMEOUT: Duration = Duration::from_secs(5);

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
type SignatureStore = DashMap<Signature, u64>;

pub struct ChainStateCache {
    slot: Arc<AtomicU64>,
    // signature -> (block_time, slot)
    signature_store: Arc<SignatureStore>,
}

pub fn spawn_chain_state_updater(
    retain_slot_count: u64,
    ws_url: String,
    grpc_url: Option<String>,
    shutdown: Arc<AtomicBool>,
) -> (ChainStateCache, std::thread::JoinHandle<()>) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .thread_name("chain-state-worker")
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let cache = ChainStateCache::new();
    let ws_client = rt
        .block_on(PubsubClient::new(ws_url))
        .expect("Failed to create pubsub client");
    let ws_client = Arc::new(ws_client);
    let async_task = {
        let _guard = rt.enter();
        let mut block_listen_task = if let Some(grpc_url) = grpc_url {
            spawn_grpc_block_listener(cache.signature_store.clone(), retain_slot_count, grpc_url)
        } else {
            spawn_ws_block_listener(
                cache.signature_store.clone(),
                retain_slot_count,
                ws_client.clone(),
            )
        };
        let mut slot_listen_task = spawn_ws_slot_listener(cache.slot.clone(), ws_client);
        async move {
            tokio::select! {
                r = &mut block_listen_task => {
                    error!("block_listen_task exited: {r:?}");
                }
                r = &mut slot_listen_task => {
                    error!("slot_listen_task exited: {r:?}");
                }
            }
        }
    };
    let handle = std::thread::Builder::new()
        .name("chain-state-handler".to_string())
        .spawn(move || {
            let _ = rt.block_on(async_task);
            shutdown.store(true, Ordering::SeqCst);
        })
        .unwrap();
    (cache, handle)
}

impl ChainStateCache {
    pub fn new() -> Self {
        Self {
            slot: Arc::new(Default::default()),
            signature_store: Arc::new(Default::default()),
        }
    }
}

impl ChainStateClient for ChainStateCache {
    fn get_slot(&self) -> u64 {
        self.slot.load(Ordering::Relaxed)
    }

    fn confirm_signature_status(&self, signature: &Signature) -> Option<u64> {
        self.signature_store.get(signature).map(|v| *v)
    }
}
fn spawn_ws_block_listener(
    signature_store: Arc<SignatureStore>,
    retain_slot_count: u64,
    ws_client: Arc<PubsubClient>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let config = Some(RpcBlockSubscribeConfig {
            commitment: Some(solana_commitment_config::CommitmentConfig::confirmed()),
            encoding: Some(Base64),
            transaction_details: Some(Signatures),
            show_rewards: Some(false),
            max_supported_transaction_version: Some(0),
        });
        info!("Subscribing to ws block updates");
        match ws_client
            .block_subscribe(RpcBlockSubscribeFilter::All, config)
            .await
        {
            Ok((mut stream, unsub)) => {
                loop {
                    let block = match timeout(TIMEOUT, stream.next()).await {
                        Ok(Some(block)) => block,
                        Ok(None) => {
                            error!("block updates ended!");
                            break;
                        }
                        Err(_) => {
                            error!("Timeout waiting for block update");
                            break;
                        }
                    };
                    let block_update = block.value;
                    if let Some(block) = block_update.block {
                        let slot = block_update.slot;
                        let _block_time = block.block_time;
                        debug!("Block update: {:?}", slot);
                        if let Some(signatures) = block.signatures {
                            for signature_str in signatures {
                                if let Ok(signature) = Signature::from_str(&signature_str) {
                                    signature_store.insert(signature, slot);
                                }
                            }
                        }
                        // remove old signatures to prevent leak of memory < slot - retain_slot_count
                        signature_store.retain(|_, v| *v > slot - retain_slot_count);
                        gauge!("iris_signature_store_size").set(signature_store.len() as f64);
                    }
                }
                if let Err(e) = timeout(TIMEOUT, unsub()).await {
                    error!("Error unsubscribing from ws block updates: {:?}", e);
                }
            }
            Err(e) => {
                error!("Error subscribing to block updates {:?}", e);
                return;
            }
        }
        warn!("Shutting down ws block listener thread");
    })
}

fn spawn_ws_slot_listener(
    current_slot: Arc<AtomicU64>,
    ws_client: Arc<PubsubClient>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let subscription = ws_client.slot_updates_subscribe().await.unwrap();
        let (mut stream, unsub) = subscription;
        loop {
            let slot_update = match timeout(TIMEOUT, stream.next()).await {
                Ok(Some(update)) => update,
                Ok(None) => {
                    error!("slot updates ended");
                    break;
                }
                Err(_) => {
                    error!("Timeout waiting for slot update");
                    break;
                }
            };
            let slot = match slot_update {
                SlotUpdate::FirstShredReceived { slot, .. } => slot,
                SlotUpdate::Completed { slot, .. } => slot.saturating_add(1),
                _ => continue,
            };
            gauge!("iris_current_slot").set(slot as f64);
            current_slot.store(slot, Ordering::Relaxed);
        }
        error!("Slot stream ended unexpectedly!!");
        if let Err(e) = timeout(TIMEOUT, unsub()).await {
            error!("Error unsubscribing from ws slot updates: {:?}", e);
        }
        warn!("Shutting down ws slot listener thread");
    })
}

fn spawn_grpc_block_listener(
    signature_store: Arc<SignatureStore>,
    retain_slot_count: u64,
    endpoint: String,
) -> JoinHandle<()> {
    let max_retries = 5;
    tokio::spawn(async move {
        let mut connection_retries = 0;
        loop {
            connection_retries += 1;
            if connection_retries > max_retries {
                error!("Max retries reached, shutting down geyser grpc block listener");
                return;
            }

            let client = GeyserGrpcClient::build_from_shared(endpoint.clone());
            if let Err(e) = client {
                error!("Error creating geyser grpc client: {:?}", e);
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            }

            let client = client
                .unwrap()
                .max_encoding_message_size(64 * 1024 * 1024)
                .max_decoding_message_size(64 * 1024 * 1024);

            let mut connection = match client.connect().await {
                Ok(connection) => connection,
                Err(e) => {
                    error!("Error connecting to geyser grpc: {:?}", e);
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    continue;
                }
            };

            let subscription = match connection.subscribe().await {
                Ok(subscription) => subscription,
                Err(e) => {
                    error!("Error subscribing to geyser grpc: {:?}", e);
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    continue;
                }
            };

            let (mut grpc_tx, mut grpc_rx) = subscription;
            info!("Subscribing to grpc block updates..");
            if let Err(e) = grpc_tx.send(block_subscribe_request!()).await {
                error!("Error sending subscription request: {:?}", e);
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            }
            connection_retries = 0;
            loop {
                let update = match timeout(TIMEOUT, grpc_rx.next()).await {
                    Ok(Some(update)) => update,
                    Ok(None) => {
                        error!("grpc block updates ended");
                        break;
                    }
                    Err(_) => {
                        error!("Timeout waiting for grpc block update");
                        break;
                    }
                };
                match update {
                    Ok(message) => match message.update_oneof {
                        Some(UpdateOneof::Block(block)) => {
                            let slot = block.slot;
                            debug!("Block update: {:?}", slot);
                            for transaction in block.transactions {
                                if let Ok(signature) = Signature::try_from(transaction.signature) {
                                    signature_store.insert(signature, slot);
                                }
                            }
                            // remove old signatures to prevent leak of memory < slot - retain_slot_count
                            signature_store.retain(|_, v| *v > slot - retain_slot_count);
                            gauge!("iris_signature_store_size").set(signature_store.len() as f64);
                        }
                        Some(UpdateOneof::Ping(_)) => {
                            if let Err(e) = grpc_tx.send(ping_request!()).await {
                                error!("Error sending ping: {}", e);
                                break;
                            }
                        }
                        Some(UpdateOneof::Pong(_)) => {}
                        _ => {
                            debug!("pong");
                        }
                    },
                    Err(e) => {
                        error!("Error block updates subscription {:?}", e);
                        tokio::time::sleep(Duration::from_secs(2)).await;
                        break;
                    }
                }
            }
        }
    })
}
