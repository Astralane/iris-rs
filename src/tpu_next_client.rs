use crate::utils::{SendTransactionClient, MEV_PROTECT_FALSE_PREFIX, MEV_PROTECT_TRUE_PREFIX};
use metrics::{counter, gauge};
use solana_sdk::signature::Keypair;
use solana_tpu_client_next::connection_workers_scheduler::WorkersBroadcaster;
use solana_tpu_client_next::leader_updater::LeaderUpdater;
use solana_tpu_client_next::{ClientBuilder, ClientError, SendTransactionStats};
use std::sync::{atomic, Arc};
use std::time::Duration;
use tokio::sync::mpsc::error::TrySendError;
use tokio_util::sync::CancellationToken;
use tracing::{error, warn};

pub struct TpuClientNextSender {
    inner: solana_tpu_client_next::TransactionSender,
}

pub fn spawn_tpu_client_next(
    broadcaster: impl WorkersBroadcaster + 'static,
    leader_updater: Box<dyn LeaderUpdater>,
    leader_fan_out: usize,
    validator_identity: Keypair,
    worker_channel_size: usize,
    max_reconnect_attempts: usize,
    cancel: CancellationToken,
) -> anyhow::Result<(TpuClientNextSender, solana_tpu_client_next::Client)> {
    let udp_sock = std::net::UdpSocket::bind("0.0.0.0:0").expect("cannot bind tpu client endpoint");
    let (sender, client) = ClientBuilder::new(leader_updater)
        .cancel_token(cancel.child_token())
        .bind_socket(udp_sock)
        .identity(Some(&validator_identity))
        .max_reconnect_attempts(1024)
        .worker_channel_size(worker_channel_size)
        .metric_reporter(send_metrics_stats)
        .max_reconnect_attempts(max_reconnect_attempts)
        .leader_send_fanout(leader_fan_out)
        .broadcaster(broadcaster)
        .build()?;
    Ok((TpuClientNextSender { inner: sender }, client))
}

impl SendTransactionClient for TpuClientNextSender {
    fn send_transaction(&self, wire_transaction: bytes::Bytes, mev_protected: bool) {
        self.send_transaction_batch(vec![wire_transaction], mev_protected);
    }

    fn send_transaction_batch(
        &self,
        mut wire_transactions: Vec<bytes::Bytes>,
        mev_protected: bool,
    ) {
        counter!("iris_tx_send_to_tpu_client_next").increment(wire_transactions.len() as u64);
        if wire_transactions.is_empty() {
            return;
        }
        let prefix = if mev_protected {
            MEV_PROTECT_TRUE_PREFIX
        } else {
            MEV_PROTECT_FALSE_PREFIX
        };
        wire_transactions.push(bytes::Bytes::from(prefix));
        match self.inner.try_send_transactions_in_batch(wire_transactions) {
            Ok(_) => {
                counter!("iris_tx_send_to_tpu_client_success").increment(1);
            }
            Err(ClientError::TrySendError(TrySendError::Closed(_))) => {
                //critical error
                error!("cannot send transactions, channel close");
                counter!("iris_tx_try_send_error_channel_closed").increment(1);
            }
            Err(ClientError::FailedToUpdateIdentity) => {
                counter!("iris_tx_failed_to_update_identity").increment(1);
            }
            Err(ClientError::JoinError(_)) => {
                counter!("iris_tx_join_error").increment(1);
            }
            Err(ClientError::ConnectionWorkersSchedulerError(err)) => {
                error!("connection worker scheduler error {err:?}");
                counter!("iris-connection_worker_scheduler_error").increment(1);
            }
            Err(ClientError::TrySendError(TrySendError::Full(_))) => {
                warn!("tpu client channel full");
                counter!("iris_tx_try_send_error_channel_full").increment(1);
            }
            Err(ClientError::SendError(_)) => {
                counter!("iris_tx_send_error_channel_failed").increment(1);
            }
        }
    }
}

pub async fn send_metrics_stats(stats: Arc<SendTransactionStats>, cancel: CancellationToken) {
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    while !cancel.is_cancelled() {
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
