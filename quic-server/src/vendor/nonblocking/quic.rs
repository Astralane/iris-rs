use bytes::Bytes;
use crossbeam_channel::{Receiver, Sender, TrySendError};
use smallvec::SmallVec;
use solana_perf::packet::{Meta, PacketBatch, PacketBatchRecycler, PACKETS_PER_BATCH};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::trace;

// A struct to accumulate the bytes making up
// a packet, along with their offsets, and the
// packet metadata. We use this accumulator to avoid
// multiple copies of the Bytes (when building up
// the Packet and then when copying the Packet into a PacketBatch)
#[derive(Clone)]
pub struct PacketAccumulator {
    pub meta: Meta,
    pub chunks: SmallVec<[Bytes; 2]>,
    pub start_time: Instant,
}

impl PacketAccumulator {
    pub(crate) fn new(meta: Meta) -> Self {
        Self {
            meta,
            chunks: SmallVec::default(),
            start_time: Instant::now(),
        }
    }
}

// Holder(s) of the Sender<PacketAccumulator> on the other end should not
// wait for this function to exit
pub fn packet_batch_sender(
    packet_sender: Sender<PacketBatch>,
    packet_receiver: Receiver<PacketAccumulator>,
    exit: Arc<AtomicBool>,
    coalesce: Duration,
) {
    trace!("enter packet_batch_sender");
    let recycler = PacketBatchRecycler::default();
    let mut batch_start_time = Instant::now();
    loop {
        let mut packet_batch =
            PacketBatch::new_with_recycler(&recycler, PACKETS_PER_BATCH, "quic_packet_coalescer");

        loop {
            if exit.load(Ordering::Relaxed) {
                return;
            }
            let elapsed = batch_start_time.elapsed();
            if packet_batch.len() >= PACKETS_PER_BATCH
                || (!packet_batch.is_empty() && elapsed >= coalesce)
            {
                let len = packet_batch.len();

                if let Err(e) = packet_sender.try_send(packet_batch) {
                    trace!("Send error: {}", e);

                    // The downstream channel is disconnected, this error is not recoverable.
                    if matches!(e, TrySendError::Disconnected(_)) {
                        exit.store(true, Ordering::Relaxed);
                        return;
                    }
                } else {
                    trace!("Sent {} packet batch", len);
                }
                break;
            }

            let timeout_res = if !packet_batch.is_empty() {
                // If we get here, elapsed < coalesce (see above if condition)
                packet_receiver.recv_timeout(coalesce - elapsed)
            } else {
                // Small bit of non-idealness here: the holder(s) of the other end
                // of packet_receiver must drop it (without waiting for us to exit)
                // or we have a chance of sleeping here forever
                // and never polling exit. Not a huge deal in practice as the
                // only time this happens is when we tear down the server
                // and at that time the other end does indeed not wait for us
                // to exit here
                packet_receiver
                    .recv()
                    .map_err(|_| crossbeam_channel::RecvTimeoutError::Disconnected)
            };

            if let Ok(packet_accumulator) = timeout_res {
                // Start the timeout from when the packet batch first becomes non-empty
                if packet_batch.is_empty() {
                    batch_start_time = Instant::now();
                }

                unsafe {
                    let new_len = packet_batch.len() + 1;
                    packet_batch.set_len(new_len);
                }

                let i = packet_batch.len() - 1;
                *packet_batch[i].meta_mut() = packet_accumulator.meta;
                let mut offset = 0;
                for chunk in packet_accumulator.chunks {
                    packet_batch[i].buffer_mut()[offset..offset + chunk.len()]
                        .copy_from_slice(&chunk);
                    offset += chunk.len();
                }
            }
        }
    }
}
