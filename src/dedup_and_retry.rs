use crate::store::{TransactionContext, TransactionStoreImpl};
use crate::tpu_next_client::{TpuClientNextSender, TpuClientPayload};
use crate::types::{ChainStateClient, PacketSource, SendTransactionClient, TransactionPacket};
use agave_transaction_view::transaction_view::TransactionView;
use bytes::Bytes;
use cached::{Cached, TimedCache};
use crossbeam_channel::{Receiver, RecvTimeoutError};
use metrics::{counter, gauge, histogram};
use solana_measure::measure_us;
use solana_rpc_client_api::response::transaction::Signature;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;
use tracing::error;

pub type DedupPacketPayload = (TransactionPacket, Instant, PacketSource);
pub struct DedupAndRetry {
    dedup_t: JoinHandle<()>,
    retry_t: JoinHandle<()>,
}

const TXN_EXPIRY_DURATION: Duration = Duration::from_secs(7);

#[derive(Clone, Debug)]
struct RetryScanEntry {
    wire_transaction: Bytes,
    signature: Signature,
    received_ts: Instant,
    slot: u64,
    retry_count: u16,
    mev_protect: bool,
}

#[derive(Debug)]
struct RetryRemoval {
    signature: Signature,
    landed_slot: Option<u64>,
    original_slot: u64,
}

#[derive(Debug)]
enum RetryScanDecision {
    Remove(RetryRemoval),
    Retry(RetryScanEntry),
}

#[derive(Debug)]
struct RetryTick {
    next_tick: Instant,
    sleep_for: Duration,
    overran: bool,
}

fn retry_budget(max_retry: Option<u16>) -> Option<u16> {
    max_retry.filter(|count| *count > 0)
}

fn snapshot_retry_store(
    store: &dashmap::DashMap<Signature, TransactionContext>,
    entries: &mut Vec<RetryScanEntry>,
) {
    entries.clear();
    for txn in store.iter() {
        let value = txn.value();
        entries.push(RetryScanEntry {
            wire_transaction: value.wire_transaction.clone(),
            signature: *txn.key(),
            received_ts: value.received_ts,
            slot: value.slot,
            retry_count: value.retry_count,
            mev_protect: value.mev_protect,
        });
    }
}

fn classify_retry_entry(
    entry: RetryScanEntry,
    landed_slot: Option<u64>,
    now: Instant,
) -> RetryScanDecision {
    if let Some(slot) = landed_slot {
        return RetryScanDecision::Remove(RetryRemoval {
            signature: entry.signature,
            landed_slot: Some(slot),
            original_slot: entry.slot,
        });
    }

    if now.saturating_duration_since(entry.received_ts) > TXN_EXPIRY_DURATION {
        return RetryScanDecision::Remove(RetryRemoval {
            signature: entry.signature,
            landed_slot: None,
            original_slot: entry.slot,
        });
    }

    if entry.retry_count == 0 {
        return RetryScanDecision::Remove(RetryRemoval {
            signature: entry.signature,
            landed_slot: None,
            original_slot: entry.slot,
        });
    }

    RetryScanDecision::Retry(entry)
}

fn schedule_next_retry_tick(
    previous_tick: Instant,
    now: Instant,
    retry_interval: Duration,
) -> RetryTick {
    let next_tick = previous_tick + retry_interval;
    match next_tick.checked_duration_since(now) {
        Some(sleep_for) => RetryTick {
            next_tick,
            sleep_for,
            overran: false,
        },
        None => RetryTick {
            next_tick: now + retry_interval,
            sleep_for: retry_interval,
            overran: true,
        },
    }
}

impl DedupAndRetry {
    pub fn new(
        tpu_client_next_sender: TpuClientNextSender,
        receiver: Receiver<DedupPacketPayload>,
        chain_state: Arc<dyn ChainStateClient>,
        retry_duration: Duration,
        cancel: CancellationToken,
    ) -> Self {
        let retry_store = TransactionStoreImpl::new();
        let dedup_t = spawn_dedup_loop(
            tpu_client_next_sender.clone(),
            receiver,
            retry_store.clone(),
            chain_state.clone(),
            cancel.clone(),
        );
        let retry_t = spawn_retry_loop(
            tpu_client_next_sender.clone(),
            retry_store.clone(),
            chain_state,
            retry_duration,
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
    retry_cache: TransactionStoreImpl,
    chain_state: Arc<dyn ChainStateClient>,
    cancel: CancellationToken,
) -> JoinHandle<()> {
    std::thread::Builder::new()
        .name("dedup_recv_loop".to_string())
        .spawn(move || {
            const RECV_TIMEOUT: Duration = Duration::from_secs(1);
            let mut dedup_cache: TimedCache<Signature, (PacketSource, Instant, u16)> =
                TimedCache::with_lifespan(Duration::from_secs(5 * 60));
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

                let (view, latency_us) = measure_us!(match TransactionView::try_new_unsanitized(
                    packet.wire_transaction.as_slice(),
                ) {
                    Ok(view) => view,
                    Err(e) => {
                        error!("cannot get transaction view {e:?}");
                        counter!("dedup_state_transaction_view_err").increment(1);
                        continue;
                    }
                });

                histogram!("transaction_view_sanitization_latency").record(latency_us as f64);

                let signature = view.signatures()[0];
                if let Some((first_seen_source, first_seen_ts, seen_count)) =
                    dedup_cache.cache_get_mut(&signature)
                {
                    *seen_count += 1;
                    if *seen_count > 2 {
                        counter!("duplicate_seen_more_than_twice").increment(1);
                        continue;
                    }

                    let elapsed = timestamp.duration_since(*first_seen_ts).as_micros();
                    if first_seen_source != &source {
                        match first_seen_source {
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
                dedup_cache.cache_set(signature, (source, timestamp, 0));
                let wire_transaction = Bytes::from(packet.wire_transaction);
                let mev_protect = packet.mev_protect;
                let retry_count = retry_budget(packet.max_retry);
                tpu_sender
                    .send_transaction(TpuClientPayload::new(wire_transaction.clone(), mev_protect));
                if let Some(retry_count) = retry_count {
                    retry_cache.add_transaction(TransactionContext {
                        wire_transaction,
                        signature,
                        received_ts: timestamp,
                        slot: chain_state.get_slot(),
                        retry_count,
                        mev_protect,
                    });
                }
            }
        })
        .unwrap()
}

fn spawn_retry_loop(
    tpu_sender: TpuClientNextSender,
    retry_store: TransactionStoreImpl,
    chain_state: Arc<dyn ChainStateClient>,
    retry_interval: Duration,
    cancel: CancellationToken,
) -> JoinHandle<()> {
    std::thread::Builder::new()
        .name("dedup_retry_loop".to_string())
        .spawn(move || {
            let store = retry_store.get_transactions();
            let mut to_remove: Vec<Signature> = Vec::new();
            let mut to_retry_entries: Vec<RetryScanEntry> = Vec::new();
            let mut to_retry: Vec<TpuClientPayload> = Vec::new();
            let mut retry_entries: Vec<RetryScanEntry> = Vec::new();
            // Fixed-rate cadence: schedule each pass against an absolute deadline so a slow
            // scan or send burst can't accumulate drift into the configured retry interval.
            let mut next_tick = Instant::now();
            loop {
                if cancel.is_cancelled() {
                    break;
                }

                let scan_start = Instant::now();

                // Snapshot under the DashMap shard locks, then perform confirmation lookups
                // after those locks have been released. This keeps ingest inserts from being
                // blocked by chain-state lookups or by signature-store maintenance.
                snapshot_retry_store(&store, &mut retry_entries);
                gauge!("iris_retry_transactions").set(retry_entries.len() as f64);
                let now = Instant::now();
                for entry in retry_entries.drain(..) {
                    let landed_slot = chain_state.confirm_signature_status(&entry.signature);
                    match classify_retry_entry(entry, landed_slot, now) {
                        RetryScanDecision::Remove(removal) => {
                            if let Some(slot) = removal.landed_slot {
                                counter!("iris_txn_landed").increment(1);
                                histogram!("iris_txn_slot_latency")
                                    .record(slot.saturating_sub(removal.original_slot) as f64);
                            }
                            to_remove.push(removal.signature);
                        }
                        RetryScanDecision::Retry(entry) => {
                            to_retry_entries.push(entry);
                        }
                    }
                }
                histogram!("iris_retry_scan_us").record(scan_start.elapsed().as_micros() as f64);

                counter!("iris_transactions_removed").increment(to_remove.len() as u64);
                for signature in to_remove.drain(..) {
                    retry_store.remove_transaction(signature);
                }

                for entry in to_retry_entries.drain(..) {
                    if let Some(mut txn) = store.get_mut(&entry.signature) {
                        if txn.retry_count > 0 {
                            txn.retry_count -= 1;
                            to_retry.push(TpuClientPayload::new(
                                entry.wire_transaction,
                                entry.mev_protect,
                            ));
                        }
                    }
                }

                if !to_retry.is_empty() {
                    counter!("iris_txn_retried").increment(to_retry.len() as u64);
                    for batch in to_retry.chunks(10) {
                        tpu_sender.send_transaction_batch(batch.to_vec());
                    }
                    to_retry.clear();
                }

                // Sleep only the remainder of the interval; on overrun, resync without spinning.
                let tick = schedule_next_retry_tick(next_tick, Instant::now(), retry_interval);
                if tick.overran {
                    counter!("iris_retry_loop_overrun").increment(1);
                }
                next_tick = tick.next_tick;
                std::thread::sleep(tick.sleep_for);
            }
        })
        .unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_sdk::signature::Signature;

    fn retry_entry(
        signature: Signature,
        now: Instant,
        age: Duration,
        retry_count: u16,
    ) -> RetryScanEntry {
        RetryScanEntry {
            wire_transaction: Bytes::from_static(b"txn"),
            signature,
            received_ts: now - age,
            slot: 10,
            retry_count,
            mev_protect: true,
        }
    }

    #[test]
    fn retry_budget_is_only_enabled_when_positive() {
        assert_eq!(retry_budget(None), None);
        assert_eq!(retry_budget(Some(0)), None);
        assert_eq!(retry_budget(Some(3)), Some(3));
    }

    #[test]
    fn confirmed_retryable_transaction_is_removed_without_resend() {
        let now = Instant::now();
        let signature = Signature::new_unique();
        let decision = classify_retry_entry(
            retry_entry(signature, now, Duration::from_millis(100), 3),
            Some(42),
            now,
        );

        match decision {
            RetryScanDecision::Remove(removal) => {
                assert_eq!(removal.signature, signature);
                assert_eq!(removal.landed_slot, Some(42));
                assert_eq!(removal.original_slot, 10);
            }
            RetryScanDecision::Retry(_) => panic!("confirmed transaction must not be retried"),
        }
    }

    #[test]
    fn exhausted_retry_budget_removes_transaction_without_resend() {
        let now = Instant::now();
        let signature = Signature::new_unique();
        let decision = classify_retry_entry(
            retry_entry(signature, now, Duration::from_millis(100), 0),
            None,
            now,
        );

        match decision {
            RetryScanDecision::Remove(removal) => {
                assert_eq!(removal.signature, signature);
                assert_eq!(removal.landed_slot, None);
            }
            RetryScanDecision::Retry(_) => panic!("exhausted transaction must not be retried"),
        }
    }

    #[test]
    fn live_retryable_transaction_is_retried() {
        let now = Instant::now();
        let signature = Signature::new_unique();
        let decision = classify_retry_entry(
            retry_entry(signature, now, Duration::from_millis(100), 2),
            None,
            now,
        );

        match decision {
            RetryScanDecision::Retry(entry) => {
                assert_eq!(entry.signature, signature);
                assert_eq!(entry.retry_count, 2);
                assert!(entry.mev_protect);
            }
            RetryScanDecision::Remove(_) => panic!("live retryable transaction must be retried"),
        }
    }

    #[test]
    fn retry_tick_sleeps_full_interval_after_overrun() {
        let start = Instant::now();
        let interval = Duration::from_millis(100);
        let tick = schedule_next_retry_tick(start, start + Duration::from_millis(250), interval);

        assert!(tick.overran);
        assert_eq!(tick.sleep_for, interval);
        assert_eq!(tick.next_tick, start + Duration::from_millis(350));
    }

    #[test]
    fn retry_tick_keeps_fixed_rate_when_scan_finishes_before_deadline() {
        let start = Instant::now();
        let interval = Duration::from_millis(100);
        let tick = schedule_next_retry_tick(start, start + Duration::from_millis(30), interval);

        assert!(!tick.overran);
        assert_eq!(tick.sleep_for, Duration::from_millis(70));
        assert_eq!(tick.next_tick, start + interval);
    }
}
