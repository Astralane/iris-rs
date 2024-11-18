use dashmap::DashMap;
use futures_util::StreamExt;
use log::error;
use metrics::{counter, gauge, histogram};
use solana_client::nonblocking::pubsub_client::PubsubClient;
use solana_rpc_client_api::config::{RpcBlockSubscribeConfig, RpcBlockSubscribeFilter};
use solana_rpc_client_api::response::SlotUpdate;
use solana_sdk::commitment_config::CommitmentConfig;
use solana_transaction_status::TransactionDetails::Signatures;
use solana_transaction_status::UiTransactionEncoding::Base64;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use tokio::runtime::Handle;
use tokio::task::JoinHandle;
use tracing::{debug, info};

const RETRY_INTERVAL: u64 = 1000;
const MAX_RETRIES: usize = 5;

//Signature and slot number the transaction was sent
type SignatureStore = DashMap<String, u64>;

pub struct ChainListener {
    slot: Arc<AtomicU64>,
    // signature -> (block_time, slot)
    signature_store: Arc<SignatureStore>,
    tracking_store: Arc<SignatureStore>,
    thread_hdls: Vec<JoinHandle<()>>,
}

impl ChainListener {
    pub fn new(
        runtime: Handle,
        shutdown: Arc<AtomicBool>,
        retain_slot_count: u64,
        ws_client: Arc<PubsubClient>,
    ) -> Self {
        let current_slot = Arc::new(AtomicU64::new(0));
        let signature_store = Arc::new(DashMap::new());
        let tracking_store = Arc::new(DashMap::new());
        let mut hdl = Vec::new();
        hdl.push(spawn_block_listener(
            runtime.clone(),
            shutdown.clone(),
            signature_store.clone(),
            tracking_store.clone(),
            retain_slot_count,
            ws_client.clone(),
        ));
        hdl.push(spawn_slot_listener(
            runtime.clone(),
            shutdown,
            current_slot.clone(),
            ws_client,
        ));

        Self {
            slot: current_slot,
            signature_store,
            thread_hdls: hdl,
            tracking_store,
        }
    }

    pub fn get_slot(&self) -> u64 {
        self.slot.load(Ordering::Relaxed)
    }

    pub fn track_signature(&self, signature: String) {
        self.tracking_store.insert(signature, self.get_slot());
    }

    pub fn confirm_signature(&self, signature: &str) -> bool {
        self.signature_store.contains_key(signature)
    }
}

fn spawn_block_listener(
    runtime: Handle,
    shutdown: Arc<AtomicBool>,
    signature_store: Arc<SignatureStore>,
    tracking_store: Arc<SignatureStore>,
    retain_slot_count: u64,
    ws_client: Arc<PubsubClient>,
) -> JoinHandle<()> {
    runtime.spawn(async move {
        let latency_histogram = histogram!("iris_block_latency");
        let config = Some(RpcBlockSubscribeConfig {
            commitment: Some(CommitmentConfig::confirmed()),
            encoding: Some(Base64),
            transaction_details: Some(Signatures),
            show_rewards: Some(false),
            max_supported_transaction_version: Some(0),
        });
        info!("Subscribing to block updates");
        let Ok((mut stream, unsub)) = ws_client
            .block_subscribe(RpcBlockSubscribeFilter::All, config)
            .await
        else {
            error!("Error subscribing to block updates");
            shutdown.store(true, Ordering::Relaxed);
            return;
        };

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
                if let Some(transactions) = block.transactions {
                    for transaction in transactions {
                        let signature = transaction
                            .transaction
                            .decode()
                            .map(|t| t.signatures[0].to_string());
                        if let Some(signature) = signature {
                            // remove from tracking signatures
                            if let Some((_, send_slot)) = tracking_store.remove(&signature) {
                                latency_histogram.record(slot.saturating_sub(send_slot) as f64);
                                counter!("iris_transactions_landed").increment(1);
                            }
                            // add to seen signatures
                            signature_store.insert(signature, slot);
                        }
                    }
                }
                // remove old signatures to prevent leak of memory < slot - retain_slot_count
                signature_store.retain(|_, v| *v > slot - retain_slot_count);
                tracking_store.retain(|_, v| *v > slot - retain_slot_count);
            }
        }
        drop(stream);
        unsub().await;
        //critical error
        shutdown.store(true, Ordering::Relaxed);
    })
}

fn spawn_slot_listener(
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
