use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use tracing::info;

const REPORT_INTERVAL: Duration = Duration::from_secs(5);

static QUIC_RECEIVED: AtomicU64 = AtomicU64::new(0);
static QUIC_INVALID: AtomicU64 = AtomicU64::new(0);
static JSON_RPC_RECEIVED: AtomicU64 = AtomicU64::new(0);
static JSON_RPC_INVALID: AtomicU64 = AtomicU64::new(0);
static MEV_PROTECTED_RECEIVED: AtomicU64 = AtomicU64::new(0);
static TPU_ENQUEUED: AtomicU64 = AtomicU64::new(0);
static TPU_DROPPED: AtomicU64 = AtomicU64::new(0);
static MEV_BUFFERED: AtomicU64 = AtomicU64::new(0);
static MEV_RELEASED: AtomicU64 = AtomicU64::new(0);
static MEV_BUFFER_SIZE: AtomicUsize = AtomicUsize::new(0);

#[derive(Clone, Copy, Default)]
struct Snapshot {
    quic_received: u64,
    quic_invalid: u64,
    json_rpc_received: u64,
    json_rpc_invalid: u64,
    mev_protected_received: u64,
    tpu_enqueued: u64,
    tpu_dropped: u64,
    mev_buffered: u64,
    mev_released: u64,
}

impl Snapshot {
    fn load() -> Self {
        Self {
            quic_received: QUIC_RECEIVED.load(Ordering::Relaxed),
            quic_invalid: QUIC_INVALID.load(Ordering::Relaxed),
            json_rpc_received: JSON_RPC_RECEIVED.load(Ordering::Relaxed),
            json_rpc_invalid: JSON_RPC_INVALID.load(Ordering::Relaxed),
            mev_protected_received: MEV_PROTECTED_RECEIVED.load(Ordering::Relaxed),
            tpu_enqueued: TPU_ENQUEUED.load(Ordering::Relaxed),
            tpu_dropped: TPU_DROPPED.load(Ordering::Relaxed),
            mev_buffered: MEV_BUFFERED.load(Ordering::Relaxed),
            mev_released: MEV_RELEASED.load(Ordering::Relaxed),
        }
    }

    fn since(self, previous: Self) -> Self {
        Self {
            quic_received: self.quic_received.saturating_sub(previous.quic_received),
            quic_invalid: self.quic_invalid.saturating_sub(previous.quic_invalid),
            json_rpc_received: self
                .json_rpc_received
                .saturating_sub(previous.json_rpc_received),
            json_rpc_invalid: self
                .json_rpc_invalid
                .saturating_sub(previous.json_rpc_invalid),
            mev_protected_received: self
                .mev_protected_received
                .saturating_sub(previous.mev_protected_received),
            tpu_enqueued: self.tpu_enqueued.saturating_sub(previous.tpu_enqueued),
            tpu_dropped: self.tpu_dropped.saturating_sub(previous.tpu_dropped),
            mev_buffered: self.mev_buffered.saturating_sub(previous.mev_buffered),
            mev_released: self.mev_released.saturating_sub(previous.mev_released),
        }
    }
}

pub fn record_quic_received() {
    QUIC_RECEIVED.fetch_add(1, Ordering::Relaxed);
}

pub fn record_quic_invalid() {
    QUIC_INVALID.fetch_add(1, Ordering::Relaxed);
}

pub fn record_json_rpc_received(mev_protected: bool) {
    JSON_RPC_RECEIVED.fetch_add(1, Ordering::Relaxed);
    record_mev_protected(mev_protected);
}

pub fn record_json_rpc_invalid() {
    JSON_RPC_INVALID.fetch_add(1, Ordering::Relaxed);
}

pub fn record_quic_mev_protected(mev_protected: bool) {
    record_mev_protected(mev_protected);
}

fn record_mev_protected(mev_protected: bool) {
    if mev_protected {
        MEV_PROTECTED_RECEIVED.fetch_add(1, Ordering::Relaxed);
    }
}

pub fn record_tpu_enqueued() {
    TPU_ENQUEUED.fetch_add(1, Ordering::Relaxed);
}

pub fn record_tpu_dropped() {
    TPU_DROPPED.fetch_add(1, Ordering::Relaxed);
}

pub fn record_mev_buffered(count: u64) {
    MEV_BUFFERED.fetch_add(count, Ordering::Relaxed);
}

pub fn record_mev_released(count: u64) {
    MEV_RELEASED.fetch_add(count, Ordering::Relaxed);
}

pub fn set_mev_buffer_size(size: usize) {
    MEV_BUFFER_SIZE.store(size, Ordering::Relaxed);
}

pub async fn report(cancel: CancellationToken) {
    let start = tokio::time::Instant::now() + REPORT_INTERVAL;
    let mut interval = tokio::time::interval_at(start, REPORT_INTERVAL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut previous = Snapshot::default();

    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = interval.tick() => {
                let total = Snapshot::load();
                let recent = total.since(previous);
                previous = total;
                let received = recent.quic_received + recent.json_rpc_received;
                let received_total = total.quic_received + total.json_rpc_received;

                info!(
                    interval_secs = REPORT_INTERVAL.as_secs(),
                    received,
                    quic_received = recent.quic_received,
                    quic_invalid = recent.quic_invalid,
                    json_rpc_received = recent.json_rpc_received,
                    json_rpc_invalid = recent.json_rpc_invalid,
                    mev_protected_received = recent.mev_protected_received,
                    tpu_enqueued = recent.tpu_enqueued,
                    tpu_dropped = recent.tpu_dropped,
                    mev_buffered = recent.mev_buffered,
                    mev_released = recent.mev_released,
                    mev_buffer_size = MEV_BUFFER_SIZE.load(Ordering::Relaxed),
                    received_total,
                    quic_received_total = total.quic_received,
                    quic_invalid_total = total.quic_invalid,
                    json_rpc_received_total = total.json_rpc_received,
                    json_rpc_invalid_total = total.json_rpc_invalid,
                    mev_protected_received_total = total.mev_protected_received,
                    tpu_enqueued_total = total.tpu_enqueued,
                    tpu_dropped_total = total.tpu_dropped,
                    "transaction stats"
                );
            }
        }
    }
}
