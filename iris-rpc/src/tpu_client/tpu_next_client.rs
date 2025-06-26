use anyhow::anyhow;
use metrics::{counter, gauge, histogram};
use solana_sdk::signature::Keypair;
use solana_tpu_client_next::connection_workers_scheduler::{
    BindTarget, ConnectionWorkersSchedulerConfig, Fanout, StakeIdentity,
};
use solana_tpu_client_next::transaction_batch::TransactionBatch;
use solana_tpu_client_next::{ConnectionWorkersScheduler, SendTransactionStats};
use std::sync::{atomic, Arc};
use std::time::Duration;
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::watch;

use crate::tpu_client::anti_mev_broadcast::MevProtectedBroadcaster;
use crate::tpu_client::leader_updater::LeaderUpdaterImpl;
use crate::traits::{BlockListProvider, SendTransactionClient};
use smart_rpc_client::rpc_provider::SmartRpcClientProvider;
use tokio_util::sync::CancellationToken;
use tracing::error;

pub struct TpuClientNextSender {
    _update_certificate_sender: tokio::sync::watch::Sender<Option<StakeIdentity>>,
    sender: tokio::sync::mpsc::Sender<TransactionBatch>,
}

pub const MAX_CONNECTIONS: usize = 60;

impl TpuClientNextSender {
    pub fn spawn_client<T: BlockListProvider>(
        num_threads: usize,
        rpc_provider: Arc<SmartRpcClientProvider>,
        ws_urls: Vec<String>,
        leaders_fanout: usize,
        validator_identity: &Keypair,
        metrics_update_interval_secs: u64,
        worker_channel_size: usize,
        max_reconnect_attempts: usize,
        blocklist_provider: T,
        cancel: CancellationToken,
    ) -> (Self, std::thread::JoinHandle<()>) {
        let (sender, receiver) = tokio::sync::mpsc::channel(128);
        let (update_certificate_sender, update_certificate_receiver) = watch::channel(None);
        let udp_sock =
            std::net::UdpSocket::bind("0.0.0.0:0").expect("cannot bind tpu client endpoint");
        let config = ConnectionWorkersSchedulerConfig {
            bind: BindTarget::Socket(udp_sock),
            stake_identity: Some(StakeIdentity::new(validator_identity)),
            num_connections: MAX_CONNECTIONS,
            skip_check_transaction_age: true,
            worker_channel_size,
            max_reconnect_attempts,
            leaders_fanout: Fanout {
                connect: leaders_fanout + 1,
                send: leaders_fanout,
            },
        };

        let handle = std::thread::Builder::new()
            .name("tpu-next-client-t".to_owned())
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(num_threads)
                    .enable_all()
                    .thread_name("tpu-next-client-rt")
                    .build()
                    .unwrap();
                let res: anyhow::Result<()> = rt.block_on(async move {
                    let leader_updater =
                        Box::new(LeaderUpdaterImpl::new(rpc_provider, &ws_urls).await?);
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
                    let blocklist_updater_handle = tokio::spawn(MevProtectedBroadcaster::run_updater(blocklist_provider));
                    tokio::select! {
                        _ = cancel.cancelled() => Ok(()),
                        _ = blocklist_updater_handle => {
                            Err(anyhow!("MevProtectedBroadcaster task stopped unexpectedly"))
                        }
                        _ = metrics_handle => {
                            Err(anyhow!("Metrics task stopped unexpectedly"))
                        }
                        result = scheduler.run_with_broadcaster::<MevProtectedBroadcaster>(config) =>  match result {
                            Ok(_) => Ok(()),
                            Err(e) => Err(anyhow!(e))
                        },
                    }
                });
                if let Err(e) = res {
                    error!("tpu-next-client-t: {:?}", e);
                }
            })
            .unwrap();

        let this = TpuClientNextSender {
            _update_certificate_sender: update_certificate_sender,
            sender,
        };
        (this, handle)
    }
}

impl SendTransactionClient for TpuClientNextSender {
    fn send_transaction(&self, wire_transaction: Vec<u8>, mev_protect: bool) {
        self.send_transaction_batch(vec![wire_transaction], mev_protect);
    }

    fn send_transaction_batch(&self, mut wire_transactions: Vec<Vec<u8>>, mev_protect: bool) {
        let batch_len = wire_transactions.len() as u64;
        histogram!("iris_tpu_client_next_txn_batch_len").record(batch_len as f64);
        counter!("iris_tx_send_to_tpu_client_next").increment(batch_len);
        wire_transactions.push(vec![mev_protect as u8]);
        let txn_batch = TransactionBatch::new(wire_transactions);
        match self.sender.try_send(txn_batch) {
            Ok(_) => {
                counter!("iris_tx_send_to_tpu_client_next_success").increment(batch_len);
            }
            Err(error) => match error {
                TrySendError::Full(_) => {
                    error!("tpu client next channel full");
                    counter!("iris_tpu_client_next_channel_full").increment(1);
                    counter!("iris_dropped_transaction_counted_since_channel_full")
                        .increment(batch_len);
                }
                TrySendError::Closed(_) => {
                    error!("tpu client next channel closed");
                    counter!("iris_tpu_client_next_channel_closed").increment(1);
                    counter!("iris_dropped_transaction_counted_since_channel_closed")
                        .increment(batch_len);
                }
            },
        }
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
