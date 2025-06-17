use bytes::{BufMut, Bytes, BytesMut};
use crossbeam_channel::{Receiver, Sender, TrySendError};
use smallvec::SmallVec;
use solana_perf::packet::{BytesPacket, BytesPacketBatch, Meta, PacketBatch, PACKETS_PER_BATCH};
use solana_perf::sigverify::PacketError;
use solana_sdk::short_vec::decode_shortu16_len;
use solana_sdk::signature::SIGNATURE_BYTES;
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;
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
    cancel: CancellationToken,
    coalesce: Duration,
) {
    trace!("enter packet_batch_sender");
    let mut batch_start_time = Instant::now();
    let mut total_bytes: usize = 0;

    loop {
        let mut packet_batch = BytesPacketBatch::with_capacity(PACKETS_PER_BATCH);

        loop {
            if cancel.is_cancelled() {
                return;
            }
            let elapsed = batch_start_time.elapsed();
            if packet_batch.len() >= PACKETS_PER_BATCH
                || (!packet_batch.is_empty() && elapsed >= coalesce)
            {
                let len = packet_batch.len();

                if let Err(e) = packet_sender.try_send(packet_batch.into()) {
                    trace!("Send error: {}", e);

                    // The downstream channel is disconnected, this error is not recoverable.
                    if matches!(e, TrySendError::Disconnected(_)) {
                        cancel.cancel();
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

            if let Ok(mut packet_accumulator) = timeout_res {
                // Start the timeout from when the packet batch first becomes non-empty
                if packet_batch.is_empty() {
                    batch_start_time = Instant::now();
                }

                // 86% of transactions/packets come in one chunk. In that case,
                // we can just move the chunk to the `Packet` and no copy is
                // made.
                // 14% of them come in multiple chunks. In that case, we copy
                // them into one `Bytes` buffer. We make a copy once, with
                // intention to not do it again.
                let packet = if packet_accumulator.chunks.len() == 1 {
                    BytesPacket::new(
                        packet_accumulator.chunks.pop().expect("expected one chunk"),
                        packet_accumulator.meta,
                    )
                } else {
                    let size: usize = packet_accumulator.chunks.iter().map(Bytes::len).sum();
                    let mut buf = BytesMut::with_capacity(size);
                    for chunk in packet_accumulator.chunks {
                        buf.put_slice(&chunk);
                    }
                    BytesPacket::new(buf.freeze(), packet_accumulator.meta)
                };

                total_bytes += packet.meta().size;
                packet_batch.push(packet);
            }
        }
    }
}

/// Get the signature of the transaction packet
/// This does a rudimentry verification to make sure the packet at least
/// contains the signature data and it returns the reference to the signature.
pub fn get_signature_from_packet(
    packet: &BytesPacket,
) -> Result<&[u8; SIGNATURE_BYTES], PacketError> {
    let (sig_len_untrusted, sig_start) = packet
        .data(..)
        .and_then(|bytes| decode_shortu16_len(bytes).ok())
        .ok_or(PacketError::InvalidShortVec)?;

    if sig_len_untrusted < 1 {
        return Err(PacketError::InvalidSignatureLen);
    }

    let signature = packet
        .data(sig_start..sig_start.saturating_add(SIGNATURE_BYTES))
        .ok_or(PacketError::InvalidSignatureLen)?;
    let signature = signature
        .try_into()
        .map_err(|_| PacketError::InvalidSignatureLen)?;
    Ok(signature)
}
