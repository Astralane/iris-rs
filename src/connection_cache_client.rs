use crate::store::{TransactionData, TransactionStore};
use crate::utils::{CreateClient, SendTransactionClient};
use log::{info, warn};
use metrics::counter;
use solana_client::connection_cache::ConnectionCache;
use solana_client::rpc_client::SerializableTransaction;
use solana_connection_cache::nonblocking::client_connection::ClientConnection;
use solana_sdk::signature::Keypair;
use solana_tpu_client_next::leader_updater::LeaderUpdater;
use std::net::{IpAddr, Ipv4Addr};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::runtime::Handle;
use tokio::time::timeout;
use tracing::error;

const MAX_RETRIES: u32 = 10;

pub struct ConnectionCacheClient {
    runtime: Handle,
    connection_cache: Arc<ConnectionCache>,
    leader_updater: Mutex<Box<dyn LeaderUpdater>>,
    lookahead_slots: u64,
    enable_leader_forwards: bool,
    txn_store: Arc<dyn TransactionStore>,
}

impl SendTransactionClient for ConnectionCacheClient {
    fn send_transaction(&self, txn: TransactionData) {
        let signature = txn.versioned_transaction.get_signature().to_string();
        info!("sending transaction {:?}", signature);
        counter!("iris_tx_send_to_connection_cache").increment(1);
        if !self.enable_leader_forwards {
            return;
        }
        if self.txn_store.has_signature(&signature) {
            counter!("iris_duplicate_transaction_count").increment(1);
            return;
        }
        self.txn_store.add_transaction(txn.clone());
        let leaders_lock = self.leader_updater.lock();
        let leaders = leaders_lock
            .unwrap()
            .next_leaders(self.lookahead_slots as usize);
        for leader in leaders {
            let connection_cache = self.connection_cache.clone();
            let wire_transaction = txn.wire_transaction.clone();
            self.runtime.spawn(async move {
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
                            counter!("iris_error", "type" => "cannot_send_to_leader").increment(1);
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

impl CreateClient for ConnectionCacheClient {
    fn create_client(
        runtime: Handle,
        leader_updater: Box<dyn LeaderUpdater>,
        enable_leader_forwards: bool,
        lookahead_slots: u64,
        validator_identity: Keypair,
        txn_store: Arc<dyn TransactionStore>,
    ) -> Self {
        let connection_cache = Arc::new(ConnectionCache::new_with_client_options(
            "iris",
            24,
            None, // created if none specified
            Some((&validator_identity, IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)))),
            None, // not used as far as I can tell
        ));
        Self {
            runtime,
            connection_cache,
            leader_updater: Mutex::new(leader_updater),
            lookahead_slots,
            enable_leader_forwards,
            txn_store,
        }
    }
}
