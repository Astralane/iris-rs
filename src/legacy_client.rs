use crate::store::TransactionData;
use crate::tpu_next_client::TxnSender;
use crate::Config;
use jsonrpsee::core::async_trait;
use log::info;
use solana_client::connection_cache::ConnectionCache;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_connection_cache::nonblocking::client_connection::ClientConnection;
use solana_sdk::signature::{read_keypair_file, Keypair};
use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;
use tracing::error;

pub struct LegacyClient {
    connection_cache: Arc<ConnectionCache>,
    rpc: Arc<RpcClient>,
}

impl LegacyClient {
    pub fn new(config: Config) -> Self {
        let rpc = Arc::new(RpcClient::new(config.rpc_url.to_owned()));
        let identity_keypair = config
            .identity_keypair_file
            .as_ref()
            .map(|file| read_keypair_file(file).expect("Failed to read identity keypair file"))
            .unwrap_or(Keypair::new());

        let connection_cache = Arc::new(ConnectionCache::new_with_client_options(
            "iris",
            4,
            None, // created if none specified
            Some((&identity_keypair, IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)))),
            None, // not used as far as I can tell
        ));
        LegacyClient {
            connection_cache,
            rpc,
        }
    }
}

#[async_trait]
impl TxnSender for LegacyClient {
    async fn send_transaction(&self, txn: TransactionData) {
        let tpu_quick = "147.28.226.99:8009".parse().unwrap();
        info!(
            "sending transaction {:?}",
            txn.versioned_transaction.signatures[0].to_string()
        );
        let conn = self.connection_cache.get_nonblocking_connection(&tpu_quick);
        let wire_tx = txn.wire_transaction;
        let result = conn.send_data(wire_tx.as_ref()).await;
        if let Err(ref e) = result {
            error!("Failed to send transaction: {:?}", e);
        }
        info!("Transaction sent {:?}", result);
    }
}
