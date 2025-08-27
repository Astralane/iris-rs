use crate::broadcaster::MevProtectedBroadcaster;
use crate::utils::{SendTransactionClient, MEV_PROTECT_FALSE_PREFIX, MEV_PROTECT_TRUE_PREFIX};
use futures_util::future::{Join, TryJoin};
use metrics::{counter, gauge};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::Keypair;
use solana_tpu_client_next::connection_workers_scheduler::{
    BindTarget, ConnectionWorkersSchedulerConfig, Fanout, StakeIdentity,
};
use solana_tpu_client_next::leader_updater::LeaderUpdater;
use solana_tpu_client_next::transaction_batch::TransactionBatch;
use solana_tpu_client_next::{ConnectionWorkersScheduler, SendTransactionStats};
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::{atomic, Arc};
use std::time::Duration;
use tokio::runtime::Handle;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tracing::error;

pub struct TpuClientNextSender {
    sender: tokio::sync::mpsc::Sender<TransactionBatch>,
}

pub fn spawn_tpu_client_send_txs(
    leader_updater: Box<dyn LeaderUpdater>,
    leader_forward_count: u64,
    validator_identity: Keypair,
    rpc: Arc<RpcClient>,
    blocklist_key: Pubkey,
    metrics_update_interval_secs: u64,
    worker_channel_size: usize,
    max_reconnect_attempts: usize,
    cancel: CancellationToken,
) -> (
    TpuClientNextSender,
    TryJoin<tokio::task::JoinHandle<()>, tokio::task::JoinHandle<()>>,
) {
    let (sender, receiver) = tokio::sync::mpsc::channel(16);
    let (_update_certificate_sender, update_certificate_receiver) = watch::channel(None);
    let udp_sock = std::net::UdpSocket::bind("0.0.0.0:0").expect("cannot bind tpu client endpoint");
    let broadcaster_task = crate::broadcaster::run(blocklist_key, rpc);
    let tpu_scheduler_task = tokio::spawn({
        async move {
            let config = ConnectionWorkersSchedulerConfig {
                bind: BindTarget::Socket(udp_sock),
                stake_identity: Some(StakeIdentity::new(&validator_identity)),
                // to match MAX_CONNECTIONS from ConnectionCache
                num_connections: 1024,
                skip_check_transaction_age: true,
                worker_channel_size,
                max_reconnect_attempts,
                leaders_fanout: Fanout {
                    connect: leader_forward_count as usize + 1,
                    send: leader_forward_count as usize,
                },
            };
            let scheduler = ConnectionWorkersScheduler::new(
                leader_updater,
                receiver,
                update_certificate_receiver,
                cancel.clone(),
            );
            let metrics_handle = tokio::spawn(send_metrics_stats(
                scheduler.get_stats().clone(),
                metrics_update_interval_secs,
            ));
            let _ = scheduler
                .run_with_broadcaster::<MevProtectedBroadcaster>(config)
                .await;
        }
    });
    let tasks = futures_util::future::try_join(tpu_scheduler_task, broadcaster_task);
    (TpuClientNextSender { sender }, tasks)
}

impl SendTransactionClient for TpuClientNextSender {
    fn send_transaction(&self, wire_transaction: Vec<u8>, mev_protected: bool) {
        self.send_transaction_batch(vec![wire_transaction], mev_protected);
    }

    fn send_transaction_batch(&self, mut wire_transactions: Vec<Vec<u8>>, mev_protected: bool) {
        counter!("iris_tx_send_to_tpu_client_next").increment(wire_transactions.len() as u64);
        if wire_transactions.is_empty() {
            return;
        }
        let prefix = if mev_protected {
            MEV_PROTECT_TRUE_PREFIX
        } else {
            MEV_PROTECT_FALSE_PREFIX
        };
        wire_transactions.push(prefix.to_vec());
        let txn_batch = TransactionBatch::new(wire_transactions);
        let sender = self.sender.clone();
        tokio::spawn(async move {
            if let Err(e) = sender.send(txn_batch).await {
                error!("Failed to send transaction: {:?}", e);
                counter!("iris_error", "type" => "cannot_send_local").increment(1);
            }
        });
    }
}

pub async fn send_metrics_stats(
    stats: Arc<SendTransactionStats>,
    metrics_update_interval_secs: u64,
) {
    let mut tick = tokio::time::interval(Duration::from_secs(metrics_update_interval_secs));
    loop {
        tick.tick().await;
        gauge!("iris_tpu_client_next_successfully_sent")
            .set(stats.successfully_sent.load(atomic::Ordering::Relaxed) as f64);
        gauge!("iris_tpu_client_next_connect_error_cids_exhausted").set(
            stats
                .connect_error_cids_exhausted
                .load(atomic::Ordering::Relaxed) as f64,
        );
        gauge!("iris_tpu_client_next_connect_error_invalid_remote_address").set(
            stats
                .connect_error_invalid_remote_address
                .load(atomic::Ordering::Relaxed) as f64,
        );
        gauge!("iris_tpu_client_next_connect_error_other")
            .set(stats.connect_error_other.load(atomic::Ordering::Relaxed) as f64);
        gauge!("iris_tpu_client_next_connection_error_application_closed").set(
            stats
                .connection_error_application_closed
                .load(atomic::Ordering::Relaxed) as f64,
        );
        gauge!("iris_tpu_client_next_connection_error_cids_exhausted").set(
            stats
                .connection_error_cids_exhausted
                .load(atomic::Ordering::Relaxed) as f64,
        );
        gauge!("iris_tpu_client_next_connection_error_connection_closed").set(
            stats
                .connection_error_connection_closed
                .load(atomic::Ordering::Relaxed) as f64,
        );
        gauge!("iris_tpu_client_next_connection_error_locally_closed").set(
            stats
                .connection_error_locally_closed
                .load(atomic::Ordering::Relaxed) as f64,
        );
        gauge!("iris_tpu_client_next_connection_error_reset")
            .set(stats.connection_error_reset.load(atomic::Ordering::Relaxed) as f64);
        gauge!("iris_tpu_client_next_connection_error_timed_out").set(
            stats
                .connection_error_timed_out
                .load(atomic::Ordering::Relaxed) as f64,
        );
        gauge!("iris_tpu_client_next_connection_error_transport_error").set(
            stats
                .connection_error_transport_error
                .load(atomic::Ordering::Relaxed) as f64,
        );
        gauge!("iris_tpu_client_next_connection_error_version_mismatch").set(
            stats
                .connection_error_version_mismatch
                .load(atomic::Ordering::Relaxed) as f64,
        );
        gauge!("iris_tpu_client_next_write_error_closed_stream").set(
            stats
                .write_error_closed_stream
                .load(atomic::Ordering::Relaxed) as f64,
        );
        gauge!("iris_tpu_client_next_write_error_connection_lost").set(
            stats
                .write_error_connection_lost
                .load(atomic::Ordering::Relaxed) as f64,
        );
        gauge!("iris_tpu_client_next_write_error_stopped")
            .set(stats.write_error_stopped.load(atomic::Ordering::Relaxed) as f64);
        gauge!("iris_tpu_client_next_write_error_zero_rtt_rejected").set(
            stats
                .write_error_zero_rtt_rejected
                .load(atomic::Ordering::Relaxed) as f64,
        );
    }
}
