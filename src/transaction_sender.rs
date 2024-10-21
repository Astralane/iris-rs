use crate::store::TransactionData;
use jsonrpsee::core::async_trait;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::signature::Keypair;
use solana_tpu_client_next::connection_workers_scheduler::ConnectionWorkersSchedulerConfig;
use solana_tpu_client_next::leader_updater::create_leader_updater;
use solana_tpu_client_next::transaction_batch::TransactionBatch;
use solana_tpu_client_next::ConnectionWorkersScheduler;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::error;
use tracing_subscriber::fmt::time::FormatTime;

#[async_trait]
pub trait TxnSender: Send + Sync {
    async fn send_transaction(&self, txn: TransactionData);
}

pub struct TpuClientNextSender {
    conn_handle: tokio::task::JoinHandle<()>,
    tx_recv_handle: tokio::task::JoinHandle<()>,
    cancel: CancellationToken,
    pub transaction_sender: mpsc::Sender<TransactionData>,
}

impl TpuClientNextSender {
    pub async fn new(rpc: RpcClient, websocket_url: String, identity: Option<Keypair>) -> Self {
        let cancel = CancellationToken::new();
        let leader_updater = create_leader_updater(Arc::new(rpc), websocket_url, None)
            .await
            .expect("Leader updates was successfully created");
        let (transaction_sender, transaction_receiver) = mpsc::channel(1000);
        let (txn_batch_sender, txn_batch_receiver) = mpsc::channel(1000);
        let bind: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let config = ConnectionWorkersSchedulerConfig {
            bind,
            num_connections: 50,
            skip_check_transaction_age: true,
            worker_channel_size: 50,
            max_reconnect_attempts: 10,
            stake_identity: identity,
            lookahead_slots: 20,
        };
        let cancel_ = cancel.clone();
        let conn_handle = tokio::spawn(async move {
            let _ = ConnectionWorkersScheduler::run(
                config,
                leader_updater,
                txn_batch_receiver,
                cancel_,
            );
        });

        let cancel_ = cancel.clone();
        let tx_recv_handle = tokio::spawn(async move {
            Self::transaction_aggregation_loop(transaction_receiver, txn_batch_sender, cancel_)
                .await;
        });

        Self {
            conn_handle,
            tx_recv_handle,
            cancel,
            transaction_sender,
        }
    }

    pub async fn transaction_aggregation_loop(
        mut recv_tx: mpsc::Receiver<TransactionData>,
        send_tx_batch: mpsc::Sender<TransactionBatch>,
        cancel: CancellationToken,
    ) {
        // Setup sending txs
        let limit = 10;
        let mut buffer = Vec::with_capacity(limit);
        loop {
            if cancel.is_cancelled() {
                return;
            }
            //fill buffer with transactions
            let _ = recv_tx.recv_many(&mut buffer, limit).await;
            //send transactions to TPU
            let wired_transactions = buffer
                .iter()
                .map(|txn| txn.wire_transaction.clone())
                .collect();
            let batch = TransactionBatch::new(wired_transactions);
            let resp = send_tx_batch.send(batch).await;
            if let Err(e) = resp {
                error!("Failed to send transactions batch: {:?}", e);
            }
            buffer.clear();
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}

#[async_trait]
impl TxnSender for TpuClientNextSender {
    async fn send_transaction(&self, txn: TransactionData) {
        let resp = self.transaction_sender.send(txn).await;
        if let Err(e) = resp {
            error!("Failed to send transaction: {:?}", e);
        }
    }
}
