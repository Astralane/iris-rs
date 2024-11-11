use crate::store::TransactionData;
use crate::tx_sender::SendTransactionClient;
use log::{info, warn};
use solana_client::connection_cache::ConnectionCache;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_connection_cache::nonblocking::client_connection::ClientConnection;
use solana_sdk::transaction::VersionedTransaction;
use solana_tpu_client_next::leader_updater::LeaderUpdater;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::time::timeout;
use tracing::error;

const MAX_RETRIES: u32 = 10;

pub struct ConnectionCacheClient {
    connection_cache: Arc<ConnectionCache>,
    leader_updater: Mutex<Box<dyn LeaderUpdater>>,
    lookahead_slots: u64,
    friendly_rpcs: Vec<Arc<RpcClient>>,
    enable_leader_forwards: bool,
}

impl ConnectionCacheClient {
    pub async fn new(
        connection_cache: Arc<ConnectionCache>,
        leader_updater: Box<dyn LeaderUpdater>,
        lookahead_slots: u64,
        friendly_rpcs: Vec<Arc<RpcClient>>,
        enable_leader_forwards: bool,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            connection_cache,
            leader_updater: Mutex::new(leader_updater),
            lookahead_slots,
            friendly_rpcs,
            enable_leader_forwards,
        })
    }

    pub fn forward_to_friendly_clients(&self, transaction: VersionedTransaction) {
        for rpc in self.friendly_rpcs.iter() {
            let transaction = transaction.clone();
            let rpc = rpc.clone();
            tokio::spawn(async move {
                let Some(legacy) = transaction.into_legacy_transaction() else {
                    return;
                };
                let resp = rpc.send_transaction(&legacy).await;
                if let Err(e) = resp {
                    error!(
                        "Failed to send transaction to friendly rpc: {:?} {:?}",
                        e,
                        rpc.url()
                    );
                }
            });
        }
    }
}

impl SendTransactionClient for ConnectionCacheClient {
    fn send_transaction(&self, txn: TransactionData) {
        info!(
            "sending transaction {:?}",
            txn.versioned_transaction.signatures[0].to_string()
        );
        self.forward_to_friendly_clients(txn.versioned_transaction.clone());
        if !self.enable_leader_forwards {
            return;
        }
        let leaders_lock = self.leader_updater.lock();
        let leaders = leaders_lock
            .unwrap()
            .next_leaders(self.lookahead_slots as usize);
        for leader in leaders {
            let connection_cache = self.connection_cache.clone();
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
                            error!(
                                "Failed to send transaction to leader TRANSPORT_ERROR {:?}: {:?}",
                                leader, e
                            );
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
    }
}
