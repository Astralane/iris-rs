use crate::store::TransactionData;
use crate::utils::SendTransactionClient;
use log::error;
use metrics::counter;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_rpc_client_api::config::RpcSendTransactionConfig;
use solana_transaction_status::UiTransactionEncoding;
use std::sync::Arc;
use tokio::runtime::Handle;
use tracing::debug;

pub struct SolanaRpcSender {
    rpc_client: Arc<RpcClient>,
    runtime: Handle,
}

impl SolanaRpcSender {
    pub fn new(url: String, runtime: Handle) -> Self {
        let rpc_client = RpcClient::new(url.to_string());
        Self {
            rpc_client: Arc::new(rpc_client),
            runtime,
        }
    }
}
impl SendTransactionClient for SolanaRpcSender {
    fn send_transaction(&self, tx: TransactionData) {
        let url = self.rpc_client.url().to_string();
        let client = self.rpc_client.clone();
        self.runtime.spawn(async move {
            match client
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
                .await
            {
                Ok(_) => {
                    debug!("Transaction sent successfully to {:?}", url);
                    counter!("transactions_sent", "to" => url).increment(1);
                }
                Err(e) => {
                    error!("Failed to send transaction to {:?}: {:?}", url, e);
                    counter!("transaction_send_failure", "to" => url).increment(1);
                }
            }
        });
    }
}
