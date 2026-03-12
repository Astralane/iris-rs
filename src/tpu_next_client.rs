use crate::types::SendTransactionClient;
use anyhow::Context;
use bitflags::bitflags;
use bytes::{BufMut, Bytes, BytesMut};
use metrics::{counter, gauge};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::signature::Keypair;
use solana_tpu_client_next::connection_workers_scheduler::WorkersBroadcaster;
use solana_tpu_client_next::node_address_service::LeaderTpuCacheServiceConfig;
use solana_tpu_client_next::websocket_node_address_service::WebsocketNodeAddressService;
use solana_tpu_client_next::{ClientBuilder, ClientError, SendTransactionStats};
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
    wire_transaction: Bytes,
    mev_protect: bool,
}

impl TpuClientPayload {
    pub fn new(txn: Bytes, mev_protect: bool) -> TpuClientPayload {
        Self {
            wire_transaction: txn,
            mev_protect,
        }
    }

    #[inline]
    pub fn is_mev_protected(&self) -> bool {
        self.mev_protect
    }

    #[inline]
    pub fn wire_transaction(&self) -> Bytes {
        self.wire_transaction.clone() //clone is cheap here
    }

    #[inline]
    pub fn encode(self) -> Bytes {
        let mut flags = BatchFlags::empty();
        if self.mev_protect {
            flags |= BatchFlags::MEV_PROTECTED;
        }
        let mut buf = BytesMut::with_capacity(self.wire_transaction.len() + 1);
        buf.put(self.wire_transaction);
        buf.put_u8(flags.bits());
        buf.freeze()
    }

    #[inline]
    pub fn decode(bytes: Bytes) -> Option<Self> {
        let (flag_byte, _) = bytes.as_ref().split_last()?;
        let flags = BatchFlags::from_bits_truncate(*flag_byte);
        let wire_transaction = bytes.slice(..bytes.len() - 1);
        Some(Self {
            wire_transaction,
            mev_protect: flags.contains(BatchFlags::MEV_PROTECTED),
        })
    }
}

#[derive(Clone)]
pub struct TpuClientNextSender {
    inner: solana_tpu_client_next::TransactionSender,
}

pub fn spawn_tpu_client_next(
    broadcaster: impl WorkersBroadcaster + 'static,
    tpu_client_rt: &Handle,
    rpc: Arc<RpcClient>,
    ws_url: String,
    tpu_cache_config: LeaderTpuCacheServiceConfig,
    leader_fan_out: usize,
    num_connections: usize,
    validator_identity: Keypair,
    worker_channel_size: usize,
    max_reconnect_attempts: usize,
    cancel: CancellationToken,
) -> anyhow::Result<(TpuClientNextSender, solana_tpu_client_next::Client)> {
    let udp_sock =
        std::net::UdpSocket::bind("0.0.0.0:0").context("cannot bind tpu client endpoint")?;

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
            .identity(Some(&validator_identity))
            .worker_channel_size(worker_channel_size)
            .metric_reporter(send_metrics_stats)
            .max_reconnect_attempts(max_reconnect_attempts)
            .leader_send_fanout(leader_fan_out)
            .max_cache_size(num_connections)
            .broadcaster(broadcaster)
            .build()?;
        Ok((TpuClientNextSender { inner: sender }, client))
    })
}

impl SendTransactionClient for TpuClientNextSender {
    fn send_transaction(&self, wire_transaction: TpuClientPayload) {
        self.send_transaction_batch(vec![wire_transaction]);
    }

    fn send_transaction_batch(&self, wire_transactions: Vec<TpuClientPayload>) {
        counter!("iris_tx_send_to_tpu_client_next").increment(wire_transactions.len() as u64);
        if wire_transactions.is_empty() {
            return;
        }

        let batch = wire_transactions
            .into_iter()
            .map(|txn| txn.encode())
            .collect::<Vec<_>>();

        if let Err(e) = self.inner.try_send_transactions_in_batch(batch) {
            record_send_err(e);
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
