use crate::store::TransactionData;
use crate::tx_sender::SendTransactionClient;
use crate::Config;
use jsonrpsee::core::async_trait;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::signature::{read_keypair_file, Signer};
use solana_tpu_client_next::connection_workers_scheduler::{
    ConnectionWorkersSchedulerConfig, Fanout,
};
use solana_tpu_client_next::leader_updater::create_leader_updater;
use solana_tpu_client_next::transaction_batch::TransactionBatch;
use solana_tpu_client_next::ConnectionWorkersScheduler;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{error, info};

pub struct TpuClientNextSender {
    conn_handle: tokio::task::JoinHandle<()>,
    tx_recv_handle: tokio::task::JoinHandle<()>,
    cancel: CancellationToken,
    txn_batch_sender: mpsc::Sender<TransactionBatch>,
    pub transaction_sender: mpsc::Sender<TransactionData>,
}

impl TpuClientNextSender {
    pub async fn new(config: Config) -> Self {
        let cancel = CancellationToken::new();
        let Config {
            ws_url,
            rpc_url,
            num_connections,
            skip_check_transaction_age,
            worker_channel_size,
            max_reconnect_attempts,
            identity_keypair_file,
            lookahead_slots,
            bind,
            ..
        } = config;
        let rpc = RpcClient::new(rpc_url.to_owned());
        let leader_updater_res =
            create_leader_updater(Arc::new(rpc), ws_url.to_owned(), None).await;

        if let Err(e) = leader_updater_res {
            error!("Failed to create leader updater: {:?}", e);
            panic!("Failed to create leader updater");
        }
        let leader_updater = leader_updater_res.unwrap();

        //read identity keypair from file
        let stake_identity = identity_keypair_file
            .as_ref()
            .map(|file| read_keypair_file(file).expect("Failed to read identity keypair file"));

        info!(
            "Identity keypair: {:?}",
            stake_identity.as_ref().map(|k| k.pubkey().to_string())
        );

        let (transaction_sender, transaction_receiver) = mpsc::channel(1000);
        let (txn_batch_sender, txn_batch_receiver) = mpsc::channel(1000);
        let config = ConnectionWorkersSchedulerConfig {
            bind,
            num_connections,
            skip_check_transaction_age,
            worker_channel_size,
            max_reconnect_attempts,
            stake_identity,
            leaders_fanout: Fanout {
                send: lookahead_slots as usize,
                connect: lookahead_slots as usize,
            },
        };
        let cancel_cl = cancel.clone();
        let conn_handle = tokio::spawn(async move {
            let result = ConnectionWorkersScheduler::run(
                config,
                leader_updater,
                txn_batch_receiver,
                cancel_cl.clone(),
            )
            .await;
            if let Err(e) = result {
                error!("Connection workers scheduler failed: {:?}", e);
                cancel_cl.cancel();
            }
        });

        let cancel_cl = cancel.clone();
        let txn_batch_sender_cl = txn_batch_sender.clone();
        let tx_recv_handle = tokio::spawn(async move {
            Self::transaction_aggregation_loop(
                transaction_receiver,
                txn_batch_sender_cl,
                cancel_cl,
            )
            .await;
        });

        Self {
            conn_handle,
            tx_recv_handle,
            cancel,
            transaction_sender,
            txn_batch_sender,
        }
    }

    pub async fn shutdown(self) {
        self.cancel.cancel();
        let _ = self.conn_handle.await;
        let _ = self.tx_recv_handle.await;
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
                info!("finish aggregation loop");
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
        }
    }
}

impl SendTransactionClient for TpuClientNextSender {
    fn send_transaction(&self, txn: TransactionData) {
        info!(
            "sending transaction {:?}",
            txn.versioned_transaction.signatures[0].to_string()
        );
        let resp = self.transaction_sender.blocking_send(txn);
        if let Err(e) = resp {
            error!("Failed to send transaction: {:?}", e);
        }
    }
}
