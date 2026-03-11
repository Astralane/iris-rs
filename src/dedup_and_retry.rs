use crate::store::{TransactionContext, TransactionStoreImpl};
use crate::tpu_next_client::{TpuClientNextSender, TpuClientPayload};
use crate::types::{ChainStateClient, PacketSource, SendTransactionClient, TransactionPacket};
use agave_transaction_view::transaction_view::TransactionView;
use bytes::Bytes;
use crossbeam_channel::{Receiver, RecvTimeoutError};
use metrics::{counter, gauge, histogram};
use std::collections::HashSet;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;
use tracing::{error, info};

pub type DedupPacketPayload = (TransactionPacket, Instant, PacketSource);
pub struct DedupAndRetry {
    dedup_t: JoinHandle<()>,
    retry_t: JoinHandle<()>,
}
impl DedupAndRetry {
    pub fn new(
        tpu_client_next_sender: TpuClientNextSender,
        receiver: Receiver<DedupPacketPayload>,
        chain_state: Arc<dyn ChainStateClient>,
        cancel: CancellationToken,
    ) -> Self {
        let store = TransactionStoreImpl::new();
        let dedup_t = spawn_dedup_loop(
            tpu_client_next_sender.clone(),
            receiver,
            store.clone(),
            chain_state.clone(),
            cancel.clone(),
        );
        let retry_t = spawn_retry_loop(
            tpu_client_next_sender.clone(),
            store.clone(),
            chain_state,
            cancel,
        );
        Self { dedup_t, retry_t }
    }

    pub(crate) fn join(self) -> std::thread::Result<()> {
        self.dedup_t.join()?;
        self.retry_t.join()
    }
}

fn spawn_dedup_loop(
    tpu_sender: TpuClientNextSender,
    packet_receiver: Receiver<DedupPacketPayload>,
    dedup_store: TransactionStoreImpl,
    chain_state: Arc<dyn ChainStateClient>,
    cancel: CancellationToken,
) -> JoinHandle<()> {
    std::thread::Builder::new()
        .name("dedup_recv_loop".to_string())
        .spawn(move || {
            const RECV_TIMEOUT: Duration = Duration::from_secs(1);
            loop {
                if cancel.is_cancelled() {
                    break;
                }
                let (packet, timestamp, source) = match packet_receiver.recv_timeout(RECV_TIMEOUT) {
                    Ok(packet) => packet,
                    Err(RecvTimeoutError::Timeout) => {
                        continue;
                    }
                    Err(RecvTimeoutError::Disconnected) => {
                        break;
                    }
                };

                let view = match TransactionView::try_new_unsanitized(
                    packet.wire_transaction.as_slice(),
                ) {
                    Ok(view) => view,
                    Err(e) => {
                        error!("cannot get transaction view {e:?}");
                        counter!("dedup_state_transaction_view_err").increment(1);
                        continue;
                    }
                };

                let signature = view.signatures()[0];
                if let Some(old) = dedup_store.get_transaction(&signature) {
                    let elapsed = old.received_ts.elapsed().as_micros();
                    if old.source != source {
                        match old.source {
                            PacketSource::Quic => {
                                counter!("transaction_quic_won_count").increment(1);
                                histogram!("transaction_quic_won").record(elapsed as f64)
                            }
                            PacketSource::JsonRpc => {
                                counter!("transaction_json_won_count").increment(1);
                                histogram!("transaction_json_rpc_won").record(elapsed as f64)
                            }
                        }
                        counter!("source_comparable_txns").increment(1);
                    }
                    counter!("duplicate_signature").increment(1);
                    continue;
                }
                let wire_transaction = Bytes::from(packet.wire_transaction);
                dedup_store.add_transaction(TransactionContext {
                    wire_transaction: wire_transaction.clone(),
                    source,
                    signature,
                    received_ts: timestamp,
                    slot: chain_state.get_slot(),
                    retry_count: packet.max_retry.unwrap_or(0),
                    mev_protect: packet.mev_protect,
                });
                tpu_sender
                    .send_transaction(TpuClientPayload::new(wire_transaction, packet.mev_protect))
            }
        })
        .unwrap()
}

fn spawn_retry_loop(
    tpu_sender: TpuClientNextSender,
    store: TransactionStoreImpl,
    chain_state: Arc<dyn ChainStateClient>,
    cancel: CancellationToken,
) -> JoinHandle<()> {
    const TXN_EXPIRY_DURATION: Duration = Duration::from_secs(120);
    std::thread::Builder::new()
        .name("dedup_retry_loop".to_string())
        .spawn(move || {
            let mut to_remove = HashSet::new();
            let mut to_retry = vec![];
            loop {
                if cancel.is_cancelled() {
                    break;
                }
                let transactions_map = store.get_transactions();
                gauge!("iris_retry_transactions").set(transactions_map.len() as f64);

                for mut txn in transactions_map.iter_mut() {
                    if let Some(slot) = chain_state.confirm_signature_status(&txn.key()) {
                        info!(
                            "Transaction confirmed at slot: {slot} latency {:}",
                            slot.saturating_sub(txn.slot)
                        );
                        counter!("iris_txn_landed").increment(1);
                        histogram!("iris_txn_slot_latency")
                            .record(slot.saturating_sub(txn.slot) as f64);
                        to_remove.insert(txn.key().clone());
                    }
                    //check if transaction has been in the store for too long
                    if txn.value().received_ts.elapsed() > TXN_EXPIRY_DURATION {
                        to_remove.insert(txn.key().clone());
                    }
                    //check if max retries has been reached
                    if txn.retry_count == 0 {
                        to_remove.insert(txn.key().clone());
                    }
                    if txn.retry_count > 0 {
                        to_retry.push(TpuClientPayload::new(
                            txn.wire_transaction.clone(),
                            txn.mev_protect,
                        ));
                    }
                    txn.retry_count = txn.retry_count.saturating_sub(1);
                }

                gauge!("iris_transactions_removed").increment(to_remove.len() as f64);
                for signature in to_remove.drain() {
                    store.remove_transaction(signature);
                }

                if !to_retry.is_empty() {
                    info!("retrying {} tranasctions", to_retry.iter().len());
                }

                for batch in to_retry.chunks(10).clone() {
                    tpu_sender.send_transaction_batch(batch.to_vec());
                }
                to_retry.clear();

                std::thread::sleep(Duration::from_millis(600));
            }
        })
        .unwrap()
}
