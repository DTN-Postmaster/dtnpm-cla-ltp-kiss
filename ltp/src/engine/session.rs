//! Session representations for sending and receiving LTP blocks (RFC 5326 §8).

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec::Vec;
use core::num::NonZeroU64;

use bytes::Bytes;

use crate::range::RangeSet;
use crate::segment::ReportSegment;
use crate::types::{CancelReason, EngineId, SessionId};

/// A recorded checkpoint issued by the block sender.
#[derive(Debug, Clone)]
pub struct CheckpointRecord {
    /// Checkpoint serial number.
    pub serial: NonZeroU64,
    /// Report serial echoed by this checkpoint.
    pub report_serial: u64,
    /// Block offset of the checkpoint segment data.
    pub offset: u64,
    /// Payload length of the checkpoint segment.
    pub length: u64,
    /// End of red part indicator.
    pub is_eorp: bool,
    /// End of block indicator.
    pub is_eob: bool,
    /// Number of times this checkpoint has been transmitted.
    pub retries: u32,
    /// Monotonic timestamp in microseconds when this checkpoint was initially transmitted.
    pub transmitted_at: u64,
}

/// Sender-side session tracking a transmitted block (RFC 5326 §8.1).
#[derive(Debug)]
pub struct SendSession {
    /// Session identifier.
    pub id: SessionId,
    /// Destination peer engine ID.
    pub peer: EngineId,
    /// Destination client service ID.
    pub client_service: u64,
    /// Retained block data until red part is fully acknowledged.
    pub block: Bytes,
    /// Length of the reliable (red) part in octets.
    pub red_len: u64,
    /// Most recently issued checkpoint serial.
    pub last_cp_serial: u64,
    /// Issued checkpoints awaiting report, keyed by checkpoint serial.
    pub checkpoints: BTreeMap<u64, CheckpointRecord>,
    /// Cumulative union of all received report claims.
    pub claims_received: RangeSet,
    /// Set of report serials already processed for deduplication.
    pub report_serials_seen: BTreeSet<u64>,
    /// True once the EOB segment has been queued/emitted.
    pub eob_sent: bool,
    /// Monotonic timestamp of last session activity in microseconds.
    pub last_activity: u64,
    /// Cancellation reason if this session is currently cancelling.
    pub cancel_reason: Option<CancelReason>,
    /// Number of cancellation segments transmitted.
    pub cancel_retries: u32,
    /// Number of retransmission cycles initiated upon receiving reports.
    pub rxmt_cycles: u32,
}

impl SendSession {
    /// Creates a new sender session.
    #[must_use]
    pub fn new(
        id: SessionId,
        peer: EngineId,
        client_service: u64,
        block: Bytes,
        red_len: u64,
        initial_cp_serial: u64,
        now: u64,
    ) -> Self {
        Self {
            id,
            peer,
            client_service,
            block,
            red_len,
            last_cp_serial: initial_cp_serial,
            checkpoints: BTreeMap::new(),
            claims_received: RangeSet::new(),
            report_serials_seen: BTreeSet::new(),
            eob_sent: false,
            last_activity: now,
            cancel_reason: None,
            cancel_retries: 0,
            rxmt_cycles: 0,
        }
    }

    /// Returns true if the red part has been fully claimed and all block segments emitted.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.eob_sent && self.claims_received.covers(&(0..self.red_len))
    }
}

/// An issued report segment recorded by the receiver awaiting acknowledgment (RA).
#[derive(Debug, Clone)]
pub struct ReportRecord {
    /// Exact report segment body for byte-identical retransmission.
    pub report: ReportSegment,
    /// Number of times this report has been transmitted.
    pub retries: u32,
    /// True once an RA has acknowledged this report.
    pub acked: bool,
    /// Monotonic timestamp in microseconds when this report was initially transmitted.
    pub transmitted_at: u64,
}

/// Receiver-side session tracking an incoming block (RFC 5326 §8.2).
#[derive(Debug)]
pub struct RecvSession {
    /// Session identifier.
    pub id: SessionId,
    /// Sender peer engine ID.
    pub peer: EngineId,
    /// Destination client service ID.
    pub client_service: u64,
    /// Reassembly buffer for the reliable (red) part.
    pub red_buf: Vec<u8>,
    /// Received red extents.
    pub red_received: RangeSet,
    /// Red-part total length (known when EORP arrives).
    pub red_part_len: Option<u64>,
    /// Total block length (known when EOB arrives).
    pub block_len: Option<u64>,
    /// True if the red part ends the block.
    pub red_ends_block: bool,
    /// Most recently issued report serial number.
    pub last_report_serial: u64,
    /// Issued reports keyed by report serial number.
    pub reports: BTreeMap<u64, ReportRecord>,
    /// Checkpoint serials answered -> report serials issued.
    pub cp_serials_answered: BTreeMap<u64, Vec<u64>>,
    /// Union of claims acknowledged by RA.
    pub reported_coverage: RangeSet,
    /// True once the complete red part has been delivered as a notice.
    pub red_delivered: bool,
    /// Monotonic timestamp of last session activity in microseconds.
    pub last_activity: u64,
    /// Out-of-order red segments awaiting contiguous reassembly.
    pub sparse_red: BTreeMap<u64, Bytes>,
    /// Total received bytes in sparse buffer to prevent memory exhaustion.
    pub total_received_bytes: usize,
    /// Minimum received green data offset for Miscolored detection (RFC 5326 §6.21).
    pub min_green_offset: Option<u64>,
    /// Maximum end offset of received red data.
    pub max_red_end: u64,
    /// Cancellation reason if this session is currently cancelling.
    pub cancel_reason: Option<CancelReason>,
    /// Number of cancellation segments transmitted.
    pub cancel_retries: u32,
}

impl RecvSession {
    /// Creates a new receiver session.
    #[must_use]
    pub fn new(
        id: SessionId,
        peer: EngineId,
        client_service: u64,
        initial_rs_serial: u64,
        now: u64,
    ) -> Self {
        Self {
            id,
            peer,
            client_service,
            red_buf: Vec::new(),
            red_received: RangeSet::new(),
            red_part_len: None,
            block_len: None,
            red_ends_block: false,
            last_report_serial: initial_rs_serial,
            reports: BTreeMap::new(),
            cp_serials_answered: BTreeMap::new(),
            reported_coverage: RangeSet::new(),
            red_delivered: false,
            last_activity: now,
            sparse_red: BTreeMap::new(),
            total_received_bytes: 0,
            min_green_offset: None,
            max_red_end: 0,
            cancel_reason: None,
            cancel_retries: 0,
        }
    }

    /// Stores a red data segment into the reassembly buffer with sparse out-of-order storage.
    pub fn insert_red(&mut self, offset: u64, data: &[u8], max_block_size: u64) -> bool {
        let Some(end) = offset.checked_add(data.len() as u64) else {
            return false;
        };
        if end > max_block_size {
            return false;
        }
        if self.red_received.covers(&(offset..end)) {
            return true;
        }

        let Ok(start_idx) = usize::try_from(offset) else {
            return false;
        };
        let Ok(end_idx) = usize::try_from(end) else {
            return false;
        };

        self.max_red_end = self.max_red_end.max(end);

        if data.is_empty() {
            return true;
        }

        if start_idx <= self.red_buf.len() {
            // Contiguous or partially overlapping with existing buffer
            if end_idx > self.red_buf.len() {
                self.red_buf.resize(end_idx, 0);
            }

            // Connect any sparse segments that are now adjacent to red_buf before writing new slice
            while let Some((&sp_offset, _)) = self.sparse_red.first_key_value() {
                let Ok(sp_start) = usize::try_from(sp_offset) else {
                    break;
                };
                if sp_start <= self.red_buf.len() {
                    let Some((_, sp_bytes)) = self.sparse_red.pop_first() else {
                        break;
                    };
                    self.total_received_bytes =
                        self.total_received_bytes.saturating_sub(sp_bytes.len());
                    let sp_end = sp_start.saturating_add(sp_bytes.len());
                    if sp_end > self.red_buf.len() {
                        self.red_buf.resize(sp_end, 0);
                    }
                    self.red_buf[sp_start..sp_end].copy_from_slice(&sp_bytes);
                } else {
                    break;
                }
            }

            self.red_buf[start_idx..end_idx].copy_from_slice(data);
            self.red_received.insert(offset..end);
        } else {
            // Out of order gap: coalesce with overlapping/adjacent sparse entries so sparse_red
            // remains disjoint and total_received_bytes tracks unique buffered bytes without leaking.
            let mut merged_start = offset;
            let mut merged_end = end;
            let mut overlapping_keys = Vec::new();
            let mut existing_bytes = 0usize;

            if let Some((&prev_k, prev_v)) = self.sparse_red.range(..offset).next_back() {
                let prev_end = prev_k.saturating_add(prev_v.len() as u64);
                if prev_end >= offset {
                    merged_start = merged_start.min(prev_k);
                    merged_end = merged_end.max(prev_end);
                    overlapping_keys.push(prev_k);
                    existing_bytes = existing_bytes.saturating_add(prev_v.len());
                }
            }
            for (&k, v) in self.sparse_red.range(offset..=end) {
                let v_end = k.saturating_add(v.len() as u64);
                merged_end = merged_end.max(v_end);
                overlapping_keys.push(k);
                existing_bytes = existing_bytes.saturating_add(v.len());
            }

            let Ok(merged_start_idx) = usize::try_from(merged_start) else {
                return false;
            };
            let Ok(merged_end_idx) = usize::try_from(merged_end) else {
                return false;
            };
            let Some(merged_len) = merged_end_idx.checked_sub(merged_start_idx) else {
                return false;
            };
            let new_bytes = merged_len.saturating_sub(existing_bytes);
            if self.total_received_bytes.saturating_add(new_bytes) > (max_block_size as usize) {
                return false;
            }

            let new_chunk = if overlapping_keys.is_empty() {
                Bytes::copy_from_slice(data)
            } else {
                let mut buf = alloc::vec![0u8; merged_len];
                for key in overlapping_keys {
                    if let Some(old) = self.sparse_red.remove(&key) {
                        self.total_received_bytes =
                            self.total_received_bytes.saturating_sub(old.len());
                        let rel = (key - merged_start) as usize;
                        buf[rel..rel + old.len()].copy_from_slice(&old);
                    }
                }
                let rel_new = (offset - merged_start) as usize;
                buf[rel_new..rel_new + data.len()].copy_from_slice(data);
                Bytes::from(buf)
            };

            self.total_received_bytes = self.total_received_bytes.saturating_add(new_chunk.len());
            self.sparse_red.insert(merged_start, new_chunk);
            self.red_received.insert(offset..end);
        }
        true
    }

    /// Returns true if the complete red part has been received.
    #[must_use]
    pub fn is_red_complete(&self) -> bool {
        if let Some(len) = self.red_part_len {
            self.red_received.covers(&(0..len))
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sparse_red_overlapping_out_of_order_does_not_leak_accounting() {
        let mut sess = RecvSession::new(
            SessionId {
                originator: EngineId(1),
                number: 1,
            },
            EngineId(1),
            1,
            0,
            0,
        );

        // First out-of-order slice [10..20)
        assert!(sess.insert_red(10, b"0123456789", 64));
        assert_eq!(sess.total_received_bytes, 10);
        assert_eq!(sess.sparse_red.len(), 1);

        // Overlapping extension at same offset [10..25)
        assert!(sess.insert_red(10, b"0123456789abcde", 64));
        assert_eq!(sess.total_received_bytes, 15);
        assert_eq!(sess.sparse_red.len(), 1);

        // Overlapping later slice [22..30) coalesces into single disjoint chunk [10..30)
        assert!(sess.insert_red(22, b"cdefghij", 64));
        assert_eq!(sess.total_received_bytes, 20);
        assert_eq!(sess.sparse_red.len(), 1);

        // Another disjoint slice [40..50)
        assert!(sess.insert_red(40, b"KLMNOPQRST", 64));
        assert_eq!(sess.total_received_bytes, 30);
        assert_eq!(sess.sparse_red.len(), 2);

        // Bridge [25..40) connects [10..30) and [40..50)
        assert!(sess.insert_red(25, b"fghij0123456789", 64));
        assert_eq!(sess.total_received_bytes, 40);
        assert_eq!(sess.sparse_red.len(), 1);

        // Fill leading gap [0..10) -> drains entire sparse_red into red_buf
        assert!(sess.insert_red(0, b"ABCDEFGHIJ", 64));
        assert_eq!(sess.total_received_bytes, 0);
        assert!(sess.sparse_red.is_empty());
        assert_eq!(
            &sess.red_buf[..50],
            b"ABCDEFGHIJ0123456789abcdefghij0123456789KLMNOPQRST"
        );
    }
}
