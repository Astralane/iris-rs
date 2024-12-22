use crate::utils::{generate_random_string, ChainStateClient};
use dashmap::DashMap;
use futures_util::{SinkExt, StreamExt};
use log::error;
use metrics::gauge;
use solana_client::nonblocking::pubsub_client::PubsubClient;
use solana_rpc_client_api::config::{RpcBlockSubscribeConfig, RpcBlockSubscribeFilter};
use solana_rpc_client_api::response::SlotUpdate;
use solana_sdk::commitment_config::CommitmentConfig;
use solana_sdk::signature::Signature;
use solana_transaction_status::TransactionDetails::Signatures;
use solana_transaction_status::UiTransactionEncoding::Base64;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::runtime::Handle;
use tokio::task::JoinHandle;
use tracing::{debug, info};
use yellowstone_grpc_client::GeyserGrpcClient;
use yellowstone_grpc_proto::geyser::SubscribeRequestFilterBlocks;
use yellowstone_grpc_proto::prelude::subscribe_update::UpdateOneof;
use yellowstone_grpc_proto::prelude::{
    SubscribeRequest, SubscribeRequestPing,
};

const RETRY_INTERVAL: u64 = 1000;
const MAX_RETRIES: usize = 5;

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
    thread_hdls: Vec<JoinHandle<()>>,
}

impl ChainStateWsClient {
    pub fn new(
        runtime: Handle,
        shutdown: Arc<AtomicBool>,
        retain_slot_count: u64,
        ws_client: Arc<PubsubClient>,
        grpc_url: Option<String>,
    ) -> Self {
        let current_slot = Arc::new(AtomicU64::new(0));
        let signature_store = Arc::new(DashMap::new());
        let mut hdl = Vec::new();
        let block_listener_hdl = if let Some(grpc_url) = grpc_url {
            spawn_grpc_block_listener(
                runtime.clone(),
                shutdown.clone(),
                signature_store.clone(),
                retain_slot_count,
                grpc_url,
            )
        } else {
            spawn_ws_block_listener(
                runtime.clone(),
                shutdown.clone(),
                signature_store.clone(),
                retain_slot_count,
                ws_client.clone(),
            )
        };
        hdl.push(block_listener_hdl);
        hdl.push(spawn_ws_slot_listener(
            runtime.clone(),
            shutdown,
            current_slot.clone(),
            ws_client,
        ));

        Self {
            slot: current_slot,
            signature_store,
            thread_hdls: hdl,
        }
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
    runtime: Handle,
    shutdown: Arc<AtomicBool>,
    signature_store: Arc<SignatureStore>,
    retain_slot_count: u64,
    ws_client: Arc<PubsubClient>,
) -> JoinHandle<()> {
    runtime.spawn(async move {
        let config = Some(RpcBlockSubscribeConfig {
            commitment: Some(CommitmentConfig::confirmed()),
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
                while let Some(block) = stream.next().await {
                    debug!("Block update");
                    gauge!("iris_current_block").set(block.value.slot as f64);
                    if shutdown.load(Ordering::Relaxed) {
                        break;
                    }
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
                error!("Block stream ended unexpectedly!!");
                drop(stream);
                unsub().await;
                //critical error
                shutdown.store(true, Ordering::Relaxed);
            }
            Err(e) => {
                error!("Error subscribing to block updates {:?}", e);
                shutdown.store(true, Ordering::Relaxed);
                return;
            }
        }
    })
}

fn spawn_ws_slot_listener(
    runtime: Handle,
    shutdown: Arc<AtomicBool>,
    current_slot: Arc<AtomicU64>,
    ws_client: Arc<PubsubClient>,
) -> JoinHandle<()> {
    runtime.spawn(async move {
        let subscription = ws_client.slot_updates_subscribe().await.unwrap();
        let (mut stream, unsub) = subscription;
        while let Some(slot_update) = stream.next().await {
            if shutdown.load(Ordering::Relaxed) {
                break;
            }
            let slot = match slot_update {
                SlotUpdate::FirstShredReceived { slot, .. } => slot,
                SlotUpdate::Completed { slot, .. } => slot.saturating_add(1),
                _ => continue,
            };
            debug!("Slot update: {}", slot);
            gauge!("iris_current_slot").set(slot as f64);
            current_slot.store(slot, Ordering::SeqCst);
        }
        drop(stream);
        unsub().await;
        shutdown.store(true, Ordering::Relaxed);
    })
}

fn spawn_grpc_block_listener(
    runtime: Handle,
    shutdown: Arc<AtomicBool>,
    signature_store: Arc<SignatureStore>,
    retain_slot_count: u64,
    endpoint: String,
) -> JoinHandle<()> {
    let max_retries = 10;
    runtime.spawn(async move {
        let mut retries = 0;
        loop {
            retries += 1;
            if retries > max_retries {
                error!("Max retries reached, shutting down geyser grpc block listener");
                shutdown.store(true, Ordering::Relaxed);
                return;
            }

            let client = GeyserGrpcClient::build_from_shared(endpoint.clone());
            if let Err(e) = client {
                error!("Error creating geyser grpc client: {:?}", e);
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            }

            let client = client.unwrap()
                .max_encoding_message_size(64 * 1024 * 1024)
                .max_decoding_message_size(64 * 1024 * 1024);

            let connection = client.connect().await;
            if let Err(e) = connection {
                error!("Error connecting to geyser grpc: {:?}", e);
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            }

            let subscription = connection.unwrap().subscribe().await;
            if let Err(e) = subscription {
                error!("Error subscribing to geyser grpc: {:?}", e);
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            }

            let (mut grpc_tx, mut grpc_rx) = subscription.unwrap();
            info!("Subscribing to grpc block updates..");
            if let Err(e) = grpc_tx.send(block_subscribe_request!()).await {
                error!("Error sending subscription request: {:?}", e);
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            }
            'event_loop: while let Some(update) = grpc_rx.next().await {
                if shutdown.load(Ordering::Relaxed) {
                    return;
                }
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
                        tokio::time::sleep(Duration::from_secs(2)).await;
                        break 'event_loop;
                    }
                }
            }
        }
    })
}
