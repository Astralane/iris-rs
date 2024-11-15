use crate::store::TransactionData;
use reqwest::Client;
use serde_json::json;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_rpc_client_api::config::RpcSendTransactionConfig;
use solana_transaction_status::UiTransactionEncoding;
use std::sync::Arc;
use tokio::runtime::Handle;
use tracing::error;

pub struct RpcForwards {
    runtime: Handle,
    pub known_rpcs: Vec<Arc<RpcClient>>,
    jito_urls: Vec<String>,
    pub client: Client,
}

impl RpcForwards {
    pub fn new(runtime: Handle, known_rpcs: Vec<Arc<RpcClient>>, jito_urls: Vec<String>) -> Self {
        Self {
            runtime,
            known_rpcs,
            jito_urls,
            client: Client::new(),
        }
    }
    pub fn forward_to_known_rpcs(&self, tx: TransactionData) {
        for rpc in self.known_rpcs.iter() {
            let tx = tx.clone();
            let rpc = rpc.clone();
            self.runtime.spawn(async move {
                let resp = rpc
                    .send_transaction_with_config(
                        &tx.versioned_transaction,
                        RpcSendTransactionConfig {
                            skip_preflight: true,
                            preflight_commitment: None,
                            encoding: Some(UiTransactionEncoding::Base64),
                            max_retries: None,
                            min_context_slot: None,
                        },
                    )
                    .await;
                if let Err(e) = resp {
                    error!("Failed to send transaction to known rpc: {:?}", e);
                }
            });
        }
    }
    pub fn forward_to_jito_clients(&self, encoded_transaction: String) {
        for url in self.jito_urls.iter() {
            let encoded_transaction = encoded_transaction.clone();
            let client = self.client.clone();
            let url = url.clone();
            self.runtime.spawn(async move {
                let body = json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "sendBundle",
                    "params": [[encoded_transaction]]
                });
                let response = client.post(&url).json(&body).send().await;
                if let Ok(response) = response {
                    let status = response.status();
                    if let Ok(body) = response.text().await {
                        if !status.is_success() {
                            error!("failed to send tx: {}", body);
                        }
                    }
                }
            });
        }
    }
}
