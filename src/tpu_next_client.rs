use anyhow::Context;
use bitflags::bitflags;
use bytes::Bytes;
use metrics::{counter, gauge};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::signature::Keypair;
use solana_tpu_client_next::connection_workers_scheduler::WorkersBroadcaster;
use solana_tpu_client_next::node_address_service::LeaderTpuCacheServiceConfig;
use solana_tpu_client_next::websocket_node_address_service::WebsocketNodeAddressService;
use solana_tpu_client_next::{ClientBuilder, ClientError, SendTransactionStats};
use std::num::NonZeroUsize;
use std::sync::{atomic, Arc};
use std::time::Duration;
use tokio::runtime::Handle;
use tokio::sync::mpsc::error::TrySendError;
use tokio_util::sync::CancellationToken;
use tracing::{error, warn};

bitflags! {
    #[derive(Debug, Clone, Copy)]
    pub struct BatchFlags: u8 {
        const MEV_PROTECTED = 0b00000001;
    }
}

#[derive(Clone)]
pub struct TpuClientPayload {
    encoded: Bytes,
}

impl TpuClientPayload {
    pub fn new(mut wire_transaction: Vec<u8>, mev_protect: bool) -> Self {
        let flags = if mev_protect {
            BatchFlags::MEV_PROTECTED
        } else {
            BatchFlags::empty()
        };
        wire_transaction.push(flags.bits());
        Self {
            encoded: Bytes::from(wire_transaction),
        }
    }

    #[inline]
    pub fn is_mev_protected(&self) -> bool {
        self.encoded.last().is_some_and(|byte| {
            BatchFlags::from_bits_truncate(*byte).contains(BatchFlags::MEV_PROTECTED)
        })
    }

    #[inline]
    pub fn wire_transaction(&self) -> Bytes {
        self.encoded.slice(..self.encoded.len() - 1)
    }

    #[inline]
    fn into_encoded(self) -> Bytes {
        self.encoded
    }

    #[inline]
    pub fn decode(encoded: Bytes) -> Option<Self> {
        encoded.last()?;
        Some(Self { encoded })
    }
}

#[derive(Clone)]
pub struct TpuClientNextSender {
    inner: solana_tpu_client_next::TransactionSender,
}

#[allow(clippy::too_many_arguments)]
pub fn spawn_tpu_client_next(
    broadcaster: impl WorkersBroadcaster + 'static,
    tpu_client_rt: &Handle,
    rpc: Arc<RpcClient>,
    ws_url: String,
    tpu_cache_config: LeaderTpuCacheServiceConfig,
    leader_fan_out: usize,
    num_connections: usize,
    validator_identity: Keypair,
    sender_channel_size: usize,
    worker_channel_size: usize,
    max_reconnect_attempts: usize,
    cancel: CancellationToken,
) -> anyhow::Result<(TpuClientNextSender, solana_tpu_client_next::Client)> {
    let udp_sock =
        std::net::UdpSocket::bind("0.0.0.0:0").context("cannot bind tpu client endpoint")?;
    let max_cache_size =
        NonZeroUsize::new(num_connections).context("num_connections must be greater than zero")?;

    // Both WebsocketNodeAddressService::run and ClientBuilder::build spawn tasks
    // internally via tokio::spawn. Wrapping both in a single block_on provides
    // the async context they need without a separate enter() guard (which would
    // conflict with block_on's own context setup).
    tpu_client_rt.block_on(async {
        let leader_updater =
            WebsocketNodeAddressService::run(rpc.clone(), ws_url, tpu_cache_config, cancel.clone())
                .await
                .context("cannot create leader updater")?;

        let (sender, client) = ClientBuilder::new(Box::new(leader_updater))
            .runtime_handle(tpu_client_rt.clone())
            .cancel_token(cancel.child_token())
            .bind_socket(udp_sock)
            .identity(&validator_identity)
            .sender_channel_size(sender_channel_size)
            .worker_channel_size(worker_channel_size)
            .metric_reporter(send_metrics_stats)
            .max_reconnect_attempts(max_reconnect_attempts)
            .leader_send_fanout(leader_fan_out)
            .max_cache_size(max_cache_size)
            .broadcaster(broadcaster)
            .build()?;
        Ok((TpuClientNextSender { inner: sender }, client))
    })
}

impl TpuClientNextSender {
    pub fn send_transaction(&self, transaction: TpuClientPayload) {
        counter!("iris_tx_send_to_tpu_client_next").increment(1);
        let batch = vec![transaction.into_encoded()];

        if let Err(error) = self.inner.try_send_transactions_in_batch(batch) {
            record_send_err(error);
        } else {
            counter!("iris_tx_send_to_tpu_client_success").increment(1);
        }
    }
}

fn record_send_err(err: ClientError) {
    match err {
        ClientError::TrySendError(TrySendError::Closed(_)) => {
            error!("cannot send transactions, channel closed");
            counter!("iris_tx_try_send_error_channel_closed").increment(1);
        }
        ClientError::TrySendError(TrySendError::Full(_)) => {
            warn!("tpu client channel full");
            counter!("iris_tx_try_send_error_channel_full").increment(1);
        }
        ClientError::ConnectionWorkersSchedulerError(err) => {
            error!("connection worker scheduler error {err:?}");
            counter!("iris_connection_worker_scheduler_error").increment(1);
        }
        ClientError::FailedToUpdateIdentity => {
            counter!("iris_tx_failed_to_update_identity").increment(1);
        }
        ClientError::JoinError(_) => {
            counter!("iris_tx_join_error").increment(1);
        }
        ClientError::SendError(_) => {
            counter!("iris_tx_send_error_channel_failed").increment(1);
        }
    }
}

async fn send_metrics_stats(stats: Arc<SendTransactionStats>, cancel: CancellationToken) {
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
        gauge!("iris_tpu_client_next_transport_congestion_events").set(
            stats
                .transport_congestion_events
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
