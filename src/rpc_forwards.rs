use crate::jito_sender::send_bundle;
use crate::store::TransactionData;
use jsonrpsee::core::async_trait;
use log::info;
use reqwest::Client;
use serde_json::json;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_rpc_client_api::config::RpcSendTransactionConfig;
use solana_transaction_status::UiTransactionEncoding;
use std::sync::Arc;
use tokio::runtime::Handle;
use tokio::task::JoinSet;
use tracing::error;

pub struct RpcForwards {
    runtime: Handle,
    pub known_rpcs: Vec<Arc<RpcClient>>,
    jito_urls: Vec<String>,
    pub client: Client,
}

#[async_trait]
pub trait RpcForwardsClient: Send + Sync {
    async fn forward_to_known_rpcs(&self, tx: TransactionData);
    async fn forward_to_jito_clients(&self, encoded_transaction: Vec<String>) -> anyhow::Result<String>;
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
}

#[async_trait]
impl RpcForwardsClient for RpcForwards {
    fn forward_to_known_rpcs(&self, tx: TransactionData) {
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
    async fn forward_to_jito_clients(
        &self,
        encoded_transaction: Vec<String>,
    ) -> anyhow::Result<String> {
        let encoded_transaction = encoded_transaction.clone();
        let client = self.client.clone();
        for url in self.jito_urls.iter() {
            let response = send_bundle(&client, &url, encoded_transaction.clone()).await;
            if let Err(e) = response {
                error!("Failed to send transaction to jito: {:?}", e);
            } else {
                info!(
                    "Successfully sent transaction to jito {:}",
                    response.unwrap()
                );
            }
        }
        Ok("hehe".to_string())
    }
}
