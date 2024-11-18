use dashmap::DashMap;
use log::error;
use metrics::{counter, histogram};
use solana_client::pubsub_client::PubsubClient;
use solana_rpc_client_api::config::{RpcBlockSubscribeConfig, RpcBlockSubscribeFilter};
use solana_rpc_client_api::response::SlotUpdate;
use solana_sdk::commitment_config::CommitmentConfig;
use solana_transaction_status::TransactionDetails::Signatures;
use solana_transaction_status::UiTransactionEncoding::Base64;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::thread::JoinHandle;
use std::time::Duration;

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
    pub fn new(ws_url: String, shutdown: Arc<AtomicBool>, retain_slot_count: u64) -> Self {
        let current_slot = Arc::new(AtomicU64::new(0));
        let signature_store = Arc::new(DashMap::new());
        let tracking_store = Arc::new(DashMap::new());

        let mut hdl = Vec::new();
        hdl.push(spawn_block_listener(
            shutdown.clone(),
            signature_store.clone(),
            tracking_store.clone(),
            retain_slot_count,
            ws_url.clone(),
        ));
        hdl.push(spawn_slot_listener(shutdown, current_slot.clone(), ws_url));

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
    shutdown: Arc<AtomicBool>,
    signature_store: Arc<SignatureStore>,
    tracking_store: Arc<SignatureStore>,
    retain_slot_count: u64,
    ws_url: String,
) -> JoinHandle<()> {
    thread::spawn(move || {
        let latency_histogram = histogram!("iris_block_latency");
        let config = Some(RpcBlockSubscribeConfig {
            commitment: Some(CommitmentConfig::confirmed()),
            encoding: Some(Base64),
            transaction_details: Some(Signatures),
            show_rewards: Some(false),
            max_supported_transaction_version: Some(0),
        });
        let Ok(block_subscribe) =
            PubsubClient::block_subscribe(&ws_url, RpcBlockSubscribeFilter::All, config.clone())
        else {
            error!("Error subscribing to block updates");
            counter!("iris_error", "type"=>"block_subscribe_error").increment(1);
            //shutdown critical error
            shutdown.store(true, Ordering::Relaxed);
            return;
        };
        let mut retries = 0;
        loop {
            if shutdown.load(Ordering::Relaxed) {
                break;
            }
            if let Some(block) = block_subscribe.1.iter().next() {
                let block_update = block.value;
                if let Some(block) = block_update.block {
                    let slot = block_update.slot;
                    let block_time = block.block_time;
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
            } else {
                // retry MAX_RETRIES times before shutting down
                thread::sleep(Duration::from_millis(RETRY_INTERVAL));
                let Ok(_block_subscribe) = PubsubClient::block_subscribe(
                    &ws_url,
                    RpcBlockSubscribeFilter::All,
                    config.clone(),
                ) else {
                    error!("Error subscribing to block updates");
                    counter!("iris_error", "type"=>"block_subscribe_error").increment(1);
                    retries += 1;
                    if retries >= MAX_RETRIES {
                        shutdown.store(true, Ordering::Relaxed);
                        return;
                    }
                    continue;
                };
            }
        }
    })
}

fn spawn_slot_listener(
    shutdown: Arc<AtomicBool>,
    current_slot: Arc<AtomicU64>,
    ws_url: String,
) -> JoinHandle<()> {
    thread::spawn(move || {
        match PubsubClient::slot_updates_subscribe(&ws_url, move |slot_update| match slot_update {
            SlotUpdate::FirstShredReceived { slot, .. } => {
                current_slot.store(slot, Ordering::Relaxed);
            }
            SlotUpdate::Completed { slot, .. } => {
                current_slot.store(slot + 1, Ordering::Relaxed);
            }
            _ => {}
        }) {
            Ok(_) => { /* doesn't seem to do anything */ }
            Err(e) => {
                error!("Error subscribing to slot updates: {:?}", e);
                counter!("iris_error", "type"=>"slot_subscribe_error").increment(1);
                //shutdown critical error
                shutdown.store(true, Ordering::Relaxed);
                return;
            }
        }
    })
}
