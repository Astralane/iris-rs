use crate::utils::SendTransactionClient;
use anyhow::anyhow;
use metrics::{counter, gauge, histogram};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::signature::Keypair;
use solana_tpu_client_next::connection_workers_scheduler::{
    BindTarget, ConnectionWorkersSchedulerConfig, Fanout, StakeIdentity,
};
use solana_tpu_client_next::leader_updater::create_leader_updater;
use solana_tpu_client_next::transaction_batch::TransactionBatch;
use solana_tpu_client_next::{ConnectionWorkersScheduler, SendTransactionStats};
use std::sync::{atomic, Arc};
use std::time::Duration;
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::watch;

use tokio_util::sync::CancellationToken;
use tracing::error;

pub struct TpuClientNextSender {
    update_certificate_sender: tokio::sync::watch::Sender<Option<StakeIdentity>>,
    sender: tokio::sync::mpsc::Sender<TransactionBatch>,
}

pub const MAX_CONNECTIONS: usize = 60;

impl TpuClientNextSender {
    pub(crate) fn spawn_client(
        num_threads: usize,
        rpc: Arc<RpcClient>,
        ws_url: String,
        leader_forward_count: usize,
        validator_identity: &Keypair,
        metrics_update_interval_secs: u64,
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
            // experimentally found parameter values
            worker_channel_size: 64,
            max_reconnect_attempts: 4,
            leaders_fanout: Fanout {
                connect: leader_forward_count,
                send: leader_forward_count,
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
                    let leader_updater = create_leader_updater(rpc, ws_url, None)
                        .await
                        .map_err(|e| anyhow!(e))?;
                    let scheduler = ConnectionWorkersScheduler::new(
                        leader_updater,
                        receiver,
                        update_certificate_receiver,
                        cancel.clone(),
                    );
                    tokio::spawn(send_metrics_stats(
                        scheduler.get_stats().clone(),
                        metrics_update_interval_secs,
                    ));
                    tokio::select! {
                        _ = cancel.cancelled() => Ok(()),
                        result = scheduler.run(config) =>  match result {
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
            update_certificate_sender,
            sender,
        };
        (this, handle)
    }
}

impl SendTransactionClient for TpuClientNextSender {
    fn send_transaction(&self, wire_transaction: Vec<u8>) {
        self.send_transaction_batch(vec![wire_transaction]);
    }

    fn send_transaction_batch(&self, wire_transactions: Vec<Vec<u8>>) {
        let batch_len = wire_transactions.len() as u64;
        histogram!("iris_tpu_client_next_txn_batch_len").record(batch_len as f64);
        counter!("iris_tx_send_to_tpu_client_next").increment(batch_len);
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
