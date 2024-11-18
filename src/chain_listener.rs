use dashmap::DashMap;
use log::error;
use metrics::counter;
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

type SignatureStore = DashMap<String, (i64, u64)>;
pub struct ChainListener {
    slot: Arc<AtomicU64>,
    // signature -> (block_time, slot)
    signature_store: Arc<SignatureStore>,
    block_listener: thread::JoinHandle<()>,
    slot_listener: thread::JoinHandle<()>,
}

impl ChainListener {
    pub fn new(ws_url: String, shutdown: Arc<AtomicBool>) -> Self {
        let current_slot = Arc::new(AtomicU64::new(0));
        let signature_store = Arc::new(DashMap::new());

        let h1 = spawn_block_listener(shutdown.clone(), signature_store.clone(), ws_url.clone());
        let h2 = spawn_slot_listener(shutdown, current_slot.clone(), ws_url);

        Self {
            slot: current_slot,
            signature_store,
            block_listener: h1,
            slot_listener: h2,
        }
    }

    pub fn get_slot(&self) -> u64 {
        self.slot.load(Ordering::Relaxed)
    }

    pub fn confirm_transaction(&self, signature: String) -> bool {
        self.signature_store.contains_key(&signature)
    }
}

fn spawn_block_listener(
    shutdown: Arc<AtomicBool>,
    signature_store: Arc<SignatureStore>,
    ws_url: String,
) -> JoinHandle<()> {
    thread::spawn(move || {
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
            counter!("iris_errors_count", "type"=>"block_subscribe_error").increment(1);
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
                            if let (Some(signature), Some(timestamp)) = (signature, block_time) {
                                signature_store.insert(signature, (timestamp, slot));
                            }
                        }
                    }
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
                    counter!("iris_errors_count", "type"=>"block_subscribe_error").increment(1);
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
                counter!("iris_errors_count", "type"=>"slot_subscribe_error").increment(1);
                //shutdown critical error
                shutdown.store(true, Ordering::Relaxed);
                return;
            }
        }
    })
}
