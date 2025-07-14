use crate::utils::{SendTransactionClient};
use metrics::counter;
use solana_sdk::signature::Keypair;
use solana_tpu_client_next::connection_workers_scheduler::{
    BindTarget, ConnectionWorkersSchedulerConfig, Fanout, StakeIdentity,
};
use solana_tpu_client_next::leader_updater::LeaderUpdater;
use solana_tpu_client_next::transaction_batch::TransactionBatch;
use solana_tpu_client_next::ConnectionWorkersScheduler;
use std::net::{Ipv4Addr, SocketAddr};
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
    cancel: CancellationToken,
) -> (TpuClientNextSender, tokio::task::JoinHandle<()>) {
    let (sender, receiver) = tokio::sync::mpsc::channel(16);
    let (_update_certificate_sender, update_certificate_receiver) = watch::channel(None);
    let udp_sock = std::net::UdpSocket::bind("0.0.0.0:0").expect("cannot bind tpu client endpoint");
    let task = tokio::spawn({
        async move {
            let config = ConnectionWorkersSchedulerConfig {
                bind: BindTarget::Socket(udp_sock),
                stake_identity: Some(StakeIdentity::new(&validator_identity)),
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
            let scheduler = ConnectionWorkersScheduler::new(
                leader_updater,
                receiver,
                update_certificate_receiver,
                cancel.clone(),
            );
            let stats = scheduler.get_stats();
            let _ = scheduler.run(config).await;
        }
    });
    (TpuClientNextSender { sender }, task)
}

impl SendTransactionClient for TpuClientNextSender {
    fn send_transaction(&self, wire_transaction: Vec<u8>) {
        self.send_transaction_batch(vec![wire_transaction]);
    }

    fn send_transaction_batch(&self, wire_transactions: Vec<Vec<u8>>) {
        counter!("iris_tx_send_to_tpu_client_next").increment(wire_transactions.len() as u64);
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
