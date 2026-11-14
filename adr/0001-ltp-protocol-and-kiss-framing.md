# ADR-0001: Sans-I/O LTP Protocol Engine, KISS Framing, and Resource Bounds

- **Status**: Accepted
- **Scope**: `ltp/` (`hardy-ltp`), `src/driver.rs`, `src/cla.rs`

## Context

`dtnpm-cla-ltp-kiss` bridges Hardy's Bundle Protocol Agent (`hardy-bpa`) to constrained RF/radio links (VHF/UHF packet radio via KISS TNCs such as Direwolf and Mercury) as well as UDP/IP links. Because protocol state machines, retransmission timers, and sparse out-of-order reassembly must remain deterministic, fuzzable, and resilient against malformed or adversarial segments, the protocol core requires strong isolation from async I/O and strict memory/timer bounds.

## Decisions

1. **Sans-I/O `no_std` Protocol Core (`hardy-ltp`)**:
   - All RFC 5326 segment codecs, checkpoint/report state machines, Karn RTT/RTO estimation, and session timers live in `hardy-ltp` (`#![no_std]` with `alloc`), decoupled from Tokio sockets and BPA sinks.
2. **KISS Framing & Half-Duplex Radio Pacing**:
   - Outbound segments over KISS-TCP use KISS Port 0 Data frames (`0xC0`/`0xDB` SLIP escaping) and filter non-Port-0 frames cleanly.
   - Optional leaky-bucket pacing and half-duplex turnaround (`WaitForReport` and `FixedDwell`) prevent local TNC buffer bloat and transmit/receive collisions.
   - Minimum-frame TNC padding (`pad_segment_for_tnc`) appends a trailer extension using RFC 5326 experimental/private tag `0xFF` rather than tag `0x00` (which RFC 5327 reserves for LTP Authentication).
3. **Closed-Session Checkpoint Replies**:
   - Retransmitted checkpoints arriving for already-delivered closed sessions reply with `upper_bound = cp_end.max(1)` so zero-length checkpoints still produce a valid RFC 5326 Report Segment (`upper_bound > lower_bound`, non-zero claim length).
4. **Memory & Timer Boundedness (`LTP-02`, `LTP-03`)**:
   - Incoming data segments for non-existent sessions validate `offset + data.len() <= max_block_size` and `recv_sessions.len() < max_recv_sessions` **before** inserting a `RecvSession` or scheduling inactivity timers.
   - Sparse out-of-order red chunks (`sparse_red`) and multi-segment green reassemblies (`GreenReassembly`) store disjoint coalesced `BTreeMap` intervals rather than resizing a contiguous sparse zero-filled `Vec<u8>`.
   - Multi-segment green reassemblies verify contiguous coverage across `0..total_len` before dispatching and enforce a 32 MiB global buffer cap (`MAX_TOTAL_GREEN_BYTES`) across all active sessions.
   - Sender/receiver cancel transitions and RS retransmission limit timeouts call `timer_table.cancel_session(session_id)` and `record_cancelled(session_id)` so orphaned timers and late segments cannot revive dead sessions.

## Consequences

- `hardy-ltp` compiles in `#![no_std]` targets without async runtime dependencies and is directly fuzz-tested.
- Overlapping or sparse out-of-order segments cannot amplify heap allocations or miscount buffered byte limits.
- Unregistering a peer removes both `ClaAddress::Private` and `ClaAddress::Tcp` routing entries in `LtpCla`, preventing stale address reuse.
