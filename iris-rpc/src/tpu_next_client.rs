use crate::utils::{CreateClient, SendTransactionClient};
use metrics::counter;
use solana_sdk::signature::Keypair;
use solana_tpu_client_next::connection_workers_scheduler::{
    ConnectionWorkersSchedulerConfig, Fanout,
};
use solana_tpu_client_next::leader_updater::LeaderUpdater;
use solana_tpu_client_next::transaction_batch::TransactionBatch;
use solana_tpu_client_next::ConnectionWorkersScheduler;
use std::net::{Ipv4Addr, SocketAddr};
use tokio::runtime::Handle;
use tokio_util::sync::CancellationToken;
use tracing::{error, span, Instrument, Level};

pub struct TpuClientNextSender {
    runtime: Handle,
    sender: tokio::sync::mpsc::Sender<TransactionBatch>,
    cancel: CancellationToken,
}

impl CreateClient for TpuClientNextSender {
    fn create_client(
        runtime: Handle,
        leader_info: Box<dyn LeaderUpdater>,
        leader_forward_count: u64,
        validator_identity: Keypair,
    ) -> Self {
        spawn_tpu_client_send_txs(
            runtime,
            leader_info,
            leader_forward_count,
            validator_identity,
        )
    }
}

fn spawn_tpu_client_send_txs(
    runtime_handle: Handle,
    leader_info: Box<dyn LeaderUpdater>,
    leader_forward_count: u64,
    validator_identity: Keypair,
) -> TpuClientNextSender {
    let (sender, receiver) = tokio::sync::mpsc::channel(128);
    let cancel = CancellationToken::new();
    let _handle = runtime_handle.spawn({
        let cancel = cancel.clone();
        async move {
            let config = ConnectionWorkersSchedulerConfig {
                bind: SocketAddr::new(Ipv4Addr::new(0, 0, 0, 0).into(), 0),
                stake_identity: Some(validator_identity),
                // Cache size of 128 covers all nodes above the P90 slot count threshold,
                // which together account for ~75% of total slots in the epoch.
                num_connections: 128,
                skip_check_transaction_age: true,
                worker_channel_size: 2,
                max_reconnect_attempts: 4,
                // Send to the next leader only, but verify that connections exist
                // for the leaders of the next `4 * NUM_CONSECUTIVE_SLOTS`.
                leaders_fanout: Fanout {
                    send: leader_forward_count as usize,
                    connect: leader_forward_count as usize,
                },
            };
            
            let _scheduler = tokio::spawn(ConnectionWorkersScheduler::run(
                config,
                leader_info,
                receiver,
                cancel.clone(),
            ));
        }
    });
    TpuClientNextSender {
        runtime: runtime_handle,
        sender,
        cancel,
    }
}

impl SendTransactionClient for TpuClientNextSender {
    fn send_transaction(&self, wire_transaction: Vec<u8>) {
        self.send_transaction_batch(vec![wire_transaction]);
    }

    fn send_transaction_batch(&self, wire_transactions: Vec<Vec<u8>>) {
        counter!("iris_tx_send_to_tpu_client_next").increment(wire_transactions.len() as u64);
        let txn_batch = TransactionBatch::new(wire_transactions);
        if let Err(e) = self.sender.try_send(txn_batch) {
            error!(
                "Failed to send transaction batch to tpu client next: {:?}",
                e
            );
            counter!("iris_tx_send_to_tpu_client_next_error").increment(1);
        }
    }
}
