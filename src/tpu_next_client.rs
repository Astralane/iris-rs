use crate::store::TransactionData;
use crate::transaction_client::{CreateClient, SendTransactionClient};
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
use tracing::{error, info};

pub struct TpuClientNextSender {
    runtime: Handle,
    sender: tokio::sync::mpsc::Sender<TransactionBatch>,
    cancel: CancellationToken,
    enable_leader_sends: bool,
}

impl CreateClient for TpuClientNextSender {
    fn create_client(
        runtime: Handle,
        leader_info: Box<dyn LeaderUpdater>,
        enable_leader_sends: bool,
        leader_forward_count: u64,
        validator_identity: Keypair,
    ) -> Self {
        spawn_tpu_client_send_txs(
            runtime,
            leader_info,
            leader_forward_count,
            enable_leader_sends,
            validator_identity,
        )
    }
}

fn spawn_tpu_client_send_txs(
    runtime_handle: Handle,
    leader_info: Box<dyn LeaderUpdater>,
    leader_forward_count: u64,
    enable_leader_sends: bool,
    validator_identity: Keypair,
) -> TpuClientNextSender {
    let (sender, receiver) = tokio::sync::mpsc::channel(16);
    let cancel = CancellationToken::new();
    let _handle = runtime_handle.spawn({
        let cancel = cancel.clone();
        async move {
            let config = ConnectionWorkersSchedulerConfig {
                bind: SocketAddr::new(Ipv4Addr::new(0, 0, 0, 0).into(), 0),
                stake_identity: Some(validator_identity),
                // to match MAX_CONNECTIONS from ConnectionCache
                num_connections: 1024,
                skip_check_transaction_age: true,
                worker_channel_size: 128,
                max_reconnect_attempts: 4,
                leaders_fanout: Fanout {
                    connect: leader_forward_count as usize,
                    send: leader_forward_count as usize,
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
        enable_leader_sends,
    }
}

impl SendTransactionClient for TpuClientNextSender {
    fn send_transaction(&self, txn: TransactionData) {
        info!(
            "sending transaction {:?}",
            txn.versioned_transaction.signatures[0].to_string()
        );
        let txn_batch = TransactionBatch::new(vec![txn.wire_transaction]);
        let sender = self.sender.clone();
        self.runtime.spawn(async move {
            if let Err(e) = sender.send(txn_batch).await {
                error!("Failed to send transaction: {:?}", e);
            }
        });
    }
}
