use crate::store::TransactionData;
use crate::tpu_next_client::TxnSender;
use crate::Config;
use anyhow::anyhow;
use jsonrpsee::core::async_trait;
use log::{info, warn};
use solana_client::connection_cache::ConnectionCache;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_connection_cache::nonblocking::client_connection::ClientConnection;
use solana_sdk::signature::{read_keypair_file, Keypair};
use solana_tpu_client_next::leader_updater::{create_leader_updater, LeaderUpdater};
use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::timeout;
use tracing::error;

pub struct LegacyClient {
    txn_sender: mpsc::Sender<TransactionData>,
    handle: tokio::task::JoinHandle<()>,
}

impl LegacyClient {
    pub async fn new(config: Config) -> anyhow::Result<Self> {
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
        let leader_updater = create_leader_updater(rpc.clone(), config.ws_url.to_owned(), None)
            .await
            .map_err(|e| anyhow!(e))?;

        let (txn_sender, txn_receiver) = mpsc::channel(16);

        let handle = tokio::spawn(Self::run(
            connection_cache.clone(),
            txn_receiver,
            leader_updater,
            config.lookahead_slots,
        ));

        Ok(Self { txn_sender, handle })
    }

    pub async fn run(
        connection_cache: Arc<ConnectionCache>,
        mut txn_receiver: mpsc::Receiver<TransactionData>,
        leader_updater: Box<dyn LeaderUpdater>,
        lookahead_slots: u64,
    ) {
        let MAX_RETRIES = 10;
        loop {
            if let Some(txn) = txn_receiver.recv().await {
                let leaders = leader_updater.next_leaders(lookahead_slots);
                for leader in leaders {
                    let connection_cache = connection_cache.clone();
                    let wire_transaction = txn.wire_transaction.clone();
                    tokio::spawn(async move {
                        for _ in 0..MAX_RETRIES {
                            let conn = connection_cache.get_nonblocking_connection(&leader);
                            if let Ok(e) = timeout(
                                Duration::from_millis(500),
                                conn.send_data(&wire_transaction),
                            )
                                .await
                            {
                                if let Err(e) = e {
                                    error!("Failed to send transaction to leader TRANSPORT_ERROR {:?}: {:?}", leader, e);
                                } else {
                                    info!("Successfully sent transaction to leader: {:?}", leader);
                                    break;
                                }
                            } else {
                                warn!(
                                    "Failed to send transaction to leader TIMEOUT: {:?}:",
                                    leader
                                );
                            }
                        }
                    });
                }
            } else {
                warn!("NO SENDERS`: shutting down.");
                break;
            }
        }
    }
}

#[async_trait]
impl TxnSender for LegacyClient {
    async fn send_transaction(&self, txn: TransactionData) {
        info!(
            "sending transaction {:?}",
            txn.versioned_transaction.signatures[0].to_string()
        );
        let r = self.txn_sender.send(txn).await;
        if let Err(e) = r {
            error!("Failed to send transaction over channel: {:?}", e);
        }
    }
}
