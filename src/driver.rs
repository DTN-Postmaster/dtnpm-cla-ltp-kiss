//! Background runtime driver coordinating the LTP sans-I/O Engine with network I/O.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpSocket, TcpStream, UdpSocket};
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, error, info, trace, warn};

use dtnpm_core::pacing::LeakyBucket;
use hardy_async::TaskPool;
use hardy_bpa::cla::{Acceptance, ForwardBundleResult, Sink, TransferOutcome};
use hardy_bpv7::bundle::Id;
use hardy_bpv7::eid::NodeId;
use hardy_ltp::engine::{Engine, EngineConfig, Notice};
use hardy_ltp::kiss::{Command, decode_frame, encode_frame};
use hardy_ltp::segment::{Extension, Segment};
use hardy_ltp::types::{EngineId, SessionId};

use crate::cla::engine_id_to_cla_address;
use crate::config::{Config, TransportMode};
use crate::error::{Error, Result};

/// Commands sent to the driver actor from the CLA interface.
pub enum DriverCommand {
    /// Initiate block transmission for a forwarded bundle.
    Forward {
        bundle_id: Id,
        destination_engine: EngineId,
        data: Bytes,
        red_len: u64,
        reply_tx: oneshot::Sender<Result<ForwardBundleResult>>,
    },
    /// Register a mapping between a remote Engine ID, its network address, and BPA NodeId.
    RegisterPeer {
        engine_id: EngineId,
        addr: SocketAddr,
        node_id: Option<NodeId>,
    },
    /// Unregister a remote peer and remove its routing associations.
    UnregisterPeer { engine_id: EngineId },
    /// Configure or dynamically update the RTT and ACK margin for a remote peer engine.
    SetPeerTiming {
        engine_id: EngineId,
        rtt: Duration,
        ack_margin: Duration,
    },
    /// Gracefully shutdown the driver task.
    Shutdown,
}

const MAX_DYNAMIC_UDP_PEERS: usize = 256;
const MAX_GREEN_REASSEMBLIES: usize = 64;
const MAX_GREEN_BUNDLE_SIZE: usize = 16 * 1024 * 1024; // 16 MiB max green bundle
const MAX_TOTAL_GREEN_BYTES: usize = 32 * 1024 * 1024; // 32 MiB global green buffer cap

#[derive(Debug)]
struct GreenReassembly {
    _source: EngineId,
    chunks: BTreeMap<usize, Vec<u8>>,
    stored_bytes: usize,
    total_len: Option<usize>,
    last_update: Instant,
}

impl GreenReassembly {
    fn new(source: EngineId, now: Instant) -> Self {
        Self {
            _source: source,
            chunks: BTreeMap::new(),
            stored_bytes: 0,
            total_len: None,
            last_update: now,
        }
    }

    fn max_end_offset(&self) -> usize {
        self.chunks
            .last_key_value()
            .map_or(0, |(&k, v)| k.saturating_add(v.len()))
    }

    /// Inserts a green slice, coalescing overlapping/adjacent chunks and enforcing global budget.
    ///
    /// Returns `Ok(())` on success or `Err(())` if global buffer cap would be exceeded.
    fn insert(
        &mut self,
        offset: usize,
        data: &[u8],
        now: Instant,
        total_green_bytes: &mut usize,
    ) -> std::result::Result<(), ()> {
        self.last_update = now;
        if data.is_empty() {
            return Ok(());
        }
        let end_offset = offset.checked_add(data.len()).ok_or(())?;

        // Fast path: incoming slice is already completely covered by an existing chunk
        if let Some((&prev_k, prev_v)) = self.chunks.range(..=offset).next_back()
            && prev_k.saturating_add(prev_v.len()) >= end_offset
        {
            return Ok(());
        }

        let mut merged_start = offset;
        let mut merged_end = end_offset;
        let mut overlapping_keys = Vec::new();
        let mut existing_bytes = 0usize;

        if let Some((&prev_k, prev_v)) = self.chunks.range(..offset).next_back() {
            let prev_end = prev_k.saturating_add(prev_v.len());
            if prev_end >= offset {
                merged_start = merged_start.min(prev_k);
                merged_end = merged_end.max(prev_end);
                overlapping_keys.push(prev_k);
                existing_bytes = existing_bytes.saturating_add(prev_v.len());
            }
        }

        for (&k, v) in self.chunks.range(offset..=end_offset) {
            let v_end = k.saturating_add(v.len());
            merged_end = merged_end.max(v_end);
            overlapping_keys.push(k);
            existing_bytes = existing_bytes.saturating_add(v.len());
        }

        let merged_len = merged_end.checked_sub(merged_start).ok_or(())?;
        let net_added = merged_len.saturating_sub(existing_bytes);
        if total_green_bytes.saturating_add(net_added) > MAX_TOTAL_GREEN_BYTES {
            return Err(());
        }

        let new_chunk = if overlapping_keys.is_empty() {
            data.to_vec()
        } else {
            let mut buf = vec![0u8; merged_len];
            for key in overlapping_keys {
                if let Some(old) = self.chunks.remove(&key) {
                    self.stored_bytes = self.stored_bytes.saturating_sub(old.len());
                    *total_green_bytes = total_green_bytes.saturating_sub(old.len());
                    let rel = key - merged_start;
                    buf[rel..rel + old.len()].copy_from_slice(&old);
                }
            }
            let rel_new = offset - merged_start;
            buf[rel_new..rel_new + data.len()].copy_from_slice(data);
            buf
        };

        self.stored_bytes = self.stored_bytes.saturating_add(new_chunk.len());
        *total_green_bytes = total_green_bytes.saturating_add(new_chunk.len());
        self.chunks.insert(merged_start, new_chunk);
        Ok(())
    }

    /// Returns true when `0..total_len` is completely and contiguously covered.
    fn is_complete(&self) -> bool {
        let Some(total) = self.total_len else {
            return false;
        };
        if total == 0 {
            return true;
        }
        let mut cursor = 0usize;
        for (&start, chunk) in &self.chunks {
            if start > cursor {
                return false;
            }
            let end = start.saturating_add(chunk.len());
            if end > cursor {
                cursor = end;
            }
            if cursor >= total {
                return cursor == total;
            }
        }
        cursor == total
    }

    fn take_complete(&mut self, total_green_bytes: &mut usize) -> Option<Vec<u8>> {
        if !self.is_complete() {
            return None;
        }
        let total = self.total_len?;
        let drained = std::mem::take(&mut self.chunks);
        *total_green_bytes = total_green_bytes.saturating_sub(self.stored_bytes);
        self.stored_bytes = 0;

        if total == 0 {
            return Some(Vec::new());
        }
        if drained.len() == 1
            && let Some((start, buf)) = drained.into_iter().next()
            && start == 0
            && buf.len() == total
        {
            return Some(buf);
        }

        // Fallback linear copy (unreachable while coalesced, preserves general chunk invariant)
        None
    }
}

/// The background task running the LTP engine.
pub struct Driver {
    config: Config,
    allow_unauthenticated_udp: bool,
    engine: Engine,
    sink: Arc<dyn Sink>,
    tasks: Arc<TaskPool>,
    cmd_rx: mpsc::Receiver<DriverCommand>,
    start_time: Instant,
    static_peers: HashMap<EngineId, SocketAddr>,
    allow_ip_migration_peers: HashSet<EngineId>,
    dynamic_peers: VecDeque<EngineId>,
    engine_to_addr: HashMap<EngineId, SocketAddr>,
    addr_to_engine: HashMap<SocketAddr, EngineId>,
    engine_to_node: HashMap<EngineId, NodeId>,
    pending_transfers: HashMap<SessionId, Id>,
    green_reassemblies: HashMap<SessionId, GreenReassembly>,
    total_green_bytes: usize,
}

/// Minimum frame length required by standard packet radio TNCs (e.g. Direwolf).
/// Ensures an LTP segment is padded to at least `min_frame_len` bytes using an
/// RFC 5326 trailer extension so that it is accepted by packet radio TNCs (such as Direwolf).
/// If `min_frame_len` is 0 or if the segment is already long enough, no padding is applied.
#[must_use]
pub fn pad_segment_for_tnc(bytes: &[u8], min_frame_len: usize) -> std::borrow::Cow<'_, [u8]> {
    if min_frame_len == 0 || bytes.len() >= min_frame_len {
        return std::borrow::Cow::Borrowed(bytes);
    }

    if let Ok((mut seg, consumed)) = Segment::parse(bytes)
        && consumed == bytes.len()
        && seg.trailer_ext.len() < 15
    {
        let cur_len = bytes.len();
        let pad_len = min_frame_len.saturating_sub(cur_len + 2);
        let pad_zeros = vec![0u8; pad_len];
        seg.trailer_ext.push(Extension {
            tag: 0xFF,
            value: &pad_zeros,
        });
        let padded = seg.encode_to_vec();
        return std::borrow::Cow::Owned(padded);
    }

    std::borrow::Cow::Borrowed(bytes)
}

#[derive(Debug, Clone)]
pub(crate) struct KissPacingConfig {
    pub min_frame_len: usize,
    pub rate_limit_bytes_per_sec: Option<u64>,
    pub burst_capacity_bytes: usize,
    pub half_duplex: bool,
    pub turnaround_mode: crate::config::TurnaroundMode,
    pub turnaround_timeout: Duration,
    pub dwell_time: Duration,
}

fn is_checkpoint_segment(bytes: &[u8]) -> bool {
    if let Some(&first) = bytes.first() {
        matches!(first & 0x0F, 1..=3)
    } else {
        false
    }
}

fn is_report_segment(bytes: &[u8]) -> bool {
    if let Some(&first) = bytes.first() {
        (first & 0x0F) == 8
    } else {
        false
    }
}

struct HalfDuplexPacer {
    enabled: bool,
    mode: crate::config::TurnaroundMode,
    turnaround_timeout: Duration,
    dwell_time: Duration,
    pause_deadline: Option<tokio::time::Instant>,
}

impl HalfDuplexPacer {
    fn new(config: &KissPacingConfig) -> Self {
        Self {
            enabled: config.half_duplex,
            mode: config.turnaround_mode,
            turnaround_timeout: config.turnaround_timeout,
            dwell_time: config.dwell_time,
            pause_deadline: None,
        }
    }

    fn is_paused(&self, now: tokio::time::Instant) -> bool {
        if !self.enabled {
            return false;
        }
        if let Some(dl) = self.pause_deadline {
            now < dl
        } else {
            false
        }
    }

    fn on_checkpoint_sent(&mut self) {
        if !self.enabled {
            return;
        }
        let duration = match self.mode {
            crate::config::TurnaroundMode::WaitForReport => self.turnaround_timeout,
            crate::config::TurnaroundMode::FixedDwell => self.dwell_time,
        };
        self.pause_deadline = Some(tokio::time::Instant::now() + duration);
        debug!(
            "Half-duplex: Checkpoint emitted, pausing transmission queue for {:?}",
            duration
        );
    }

    fn on_report_received(&mut self) {
        if !self.enabled {
            return;
        }
        if self.mode == crate::config::TurnaroundMode::WaitForReport
            && self.pause_deadline.is_some()
        {
            debug!(
                "Half-duplex: Report Segment received, unpausing transmission queue immediately"
            );
            self.pause_deadline = None;
        }
    }

    fn next_deadline(&self) -> Option<tokio::time::Instant> {
        if !self.enabled {
            None
        } else {
            self.pause_deadline
        }
    }
}

impl Driver {
    /// Spawns the driver task based on configuration.
    pub fn spawn(
        config: Config,
        allow_unauthenticated_udp: bool,
        sink: Arc<dyn Sink>,
        cmd_rx: mpsc::Receiver<DriverCommand>,
        addr_tx: oneshot::Sender<std::io::Result<SocketAddr>>,
        tasks: Arc<TaskPool>,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let start_time = Instant::now();
            let engine_cfg = EngineConfig {
                engine_id: config.engine_id,
                max_segment_payload_len: config.max_segment_payload_len,
                checkpoint_rtx_limit: config.checkpoint_rtx_limit,
                report_rtx_limit: config.report_rtx_limit,
                rtt_micros: u64::try_from(config.rtt.as_micros()).unwrap_or(u64::MAX),
                ack_margin_micros: u64::try_from(config.ack_margin.as_micros()).unwrap_or(u64::MAX),
                allowed_client_services: Some(vec![1]),
                ..Default::default()
            };
            let mut engine = Engine::new(engine_cfg);
            let mut static_peers = HashMap::new();
            let mut allow_ip_migration_peers = HashSet::new();
            for peer in &config.peers {
                if peer.allow_ip_migration {
                    allow_ip_migration_peers.insert(peer.engine_id);
                }
                if let Some(addr) = peer.address {
                    static_peers.insert(peer.engine_id, addr);
                }
                if let (Some(rtt), Some(ack_margin)) = (peer.rtt, peer.ack_margin) {
                    let rtt_micros = u64::try_from(rtt.as_micros()).unwrap_or(u64::MAX);
                    let ack_margin_micros =
                        u64::try_from(ack_margin.as_micros()).unwrap_or(u64::MAX);
                    engine.set_peer_timing(peer.engine_id, rtt_micros, ack_margin_micros);
                }
            }

            let mut driver = Self {
                config,
                allow_unauthenticated_udp,
                engine,
                sink,
                tasks,
                cmd_rx,
                start_time,
                static_peers,
                allow_ip_migration_peers,
                dynamic_peers: VecDeque::new(),
                engine_to_addr: HashMap::new(),
                addr_to_engine: HashMap::new(),
                engine_to_node: HashMap::new(),
                pending_transfers: HashMap::new(),
                green_reassemblies: HashMap::new(),
                total_green_bytes: 0,
            };

            if let Err(err) = driver.run(addr_tx).await {
                error!("LTP driver exited with error: {err}");
            }

            // Clean shutdown: notify all remaining pending transfers with Failed outcome
            for (_session, bundle_id) in driver.pending_transfers.drain() {
                let _ = driver
                    .sink
                    .transfer_outcome(&bundle_id, TransferOutcome::Failed)
                    .await;
            }
        })
    }

    fn remove_green_reassembly(&mut self, session: &SessionId) {
        if let Some(removed) = self.green_reassemblies.remove(session) {
            self.total_green_bytes = self.total_green_bytes.saturating_sub(removed.stored_bytes);
        }
    }

    fn prune_expired_green_reassemblies(&mut self, max_age: Duration) {
        let now = Instant::now();
        let mut removed_bytes = 0usize;
        self.green_reassemblies.retain(|_, r| {
            let keep = now.duration_since(r.last_update) < max_age;
            if !keep {
                removed_bytes = removed_bytes.saturating_add(r.stored_bytes);
            }
            keep
        });
        self.total_green_bytes = self.total_green_bytes.saturating_sub(removed_bytes);
    }

    fn now_micros(&self) -> u64 {
        u64::try_from(self.start_time.elapsed().as_micros()).unwrap_or(u64::MAX)
    }

    async fn run(&mut self, addr_tx: oneshot::Sender<std::io::Result<SocketAddr>>) -> Result<()> {
        match self.config.transport.clone() {
            TransportMode::Udp { bind_addr } => self.run_udp(bind_addr, addr_tx).await,
            TransportMode::KissTcp {
                server_addr,
                bind_addr,
                port,
                reconnect_initial_delay,
                reconnect_max_delay,
                min_frame_len,
                rate_limit_bytes_per_sec,
                burst_capacity_bytes,
                half_duplex,
                turnaround_mode,
                turnaround_timeout,
                dwell_time,
            } => {
                let pacing = KissPacingConfig {
                    min_frame_len,
                    rate_limit_bytes_per_sec,
                    burst_capacity_bytes,
                    half_duplex,
                    turnaround_mode,
                    turnaround_timeout,
                    dwell_time,
                };
                self.run_kiss_tcp(
                    server_addr,
                    bind_addr,
                    port,
                    reconnect_initial_delay,
                    reconnect_max_delay,
                    pacing,
                    addr_tx,
                )
                .await
            }
        }
    }

    async fn run_udp(
        &mut self,
        bind_addr: SocketAddr,
        addr_tx: oneshot::Sender<std::io::Result<SocketAddr>>,
    ) -> Result<()> {
        let socket = match UdpSocket::bind(bind_addr).await {
            Ok(s) => Arc::new(s),
            Err(e) => {
                let _ = addr_tx.send(Err(e));
                return Ok(());
            }
        };
        let local_addr = match socket.local_addr() {
            Ok(a) => a,
            Err(e) => {
                let _ = addr_tx.send(Err(e));
                return Ok(());
            }
        };
        let _ = addr_tx.send(Ok(local_addr));
        info!("LTP UDP transport listening on {local_addr}");

        let mut rx_buf = vec![0u8; 65536];
        let cancel = self.tasks.cancel_token().clone();

        loop {
            let now = self.now_micros();
            let next_timer = self.engine.next_timer_deadline();

            // Calculate duration to next deadline
            let timer_future = async {
                match next_timer {
                    Some(deadline) if deadline > now => {
                        tokio::time::sleep(Duration::from_micros(deadline - now)).await;
                    }
                    Some(_) => {
                        // Expired or due immediately
                    }
                    None => {
                        // Indefinite sleep until interrupted by other events
                        std::future::pending::<()>().await;
                    }
                }
            };

            tokio::select! {
                _ = cancel.cancelled() => {
                    debug!("LTP UDP driver loop cancelled");
                    break;
                }
                // 1. Ingress UDP packet
                recv_res = socket.recv_from(&mut rx_buf) => {
                    let (len, peer_addr) = match recv_res {
                        Ok(res) => res,
                        Err(err) => {
                            warn!("LTP UDP recv_from error (transient): {err}");
                            continue;
                        }
                    };

                    if len == 4 && rx_buf[..4] == [0, 0, 0, 0] {
                        // RFC 7122 §3.3 & §3.4: 4-octet keep-alive packet MUST be discarded
                        trace!("discarding 4-octet keep-alive datagram from {peer_addr}");
                        continue;
                    }

                    let now = self.now_micros();

                    // Always peek the LTP session header first so a cached socket address cannot
                    // spoof a different peer's EngineId in Engine::handle_incoming_segment.
                    let Ok(session) = Segment::peek_session(&rx_buf[..len]) else {
                        trace!("discarding malformed UDP datagram from {peer_addr}");
                        continue;
                    };

                    let peer_engine = if session.originator != self.config.engine_id {
                        session.originator
                    } else if let Some(peer) = self.engine.peer_for_session(&session) {
                        peer
                    } else if let Some(&cached) = self.addr_to_engine.get(&peer_addr) {
                        cached
                    } else if let (1, Some(&only_peer)) =
                        (self.engine_to_addr.len(), self.engine_to_addr.keys().next())
                    {
                        only_peer
                    } else {
                        warn!("received packet from unregistered UDP peer {peer_addr}");
                        continue;
                    };

                    if let Some(&static_addr) = self.static_peers.get(&peer_engine) {
                        let allow_migration = self.allow_unauthenticated_udp
                            || self.allow_ip_migration_peers.contains(&peer_engine);
                        if static_addr.ip() != peer_addr.ip() && !allow_migration {
                            warn!(
                                "rejecting UDP segment for static Engine {:?} from mismatched IP {} (expected {})",
                                peer_engine,
                                peer_addr.ip(),
                                static_addr.ip()
                            );
                            continue;
                        }
                        if self.addr_to_engine.len() >= 1024
                            && !self.addr_to_engine.contains_key(&peer_addr)
                        {
                            self.addr_to_engine.clear();
                        }
                        self.addr_to_engine.insert(peer_addr, peer_engine);
                        self.engine_to_addr.insert(peer_engine, peer_addr);
                    } else if !self.allow_unauthenticated_udp {
                        warn!(
                            "rejecting UDP segment from unauthenticated dynamic Engine {:?} at {} (enable --allow-unauthenticated-udp to permit)",
                            peer_engine, peer_addr
                        );
                        continue;
                    } else {
                        let is_new_peer = !self.engine_to_addr.contains_key(&peer_engine);
                        if is_new_peer {
                            if self.engine_to_addr.len() >= 1024 {
                                self.engine_to_addr
                                    .retain(|id, _| self.static_peers.contains_key(id));
                                self.dynamic_peers.clear();
                            }
                            while self.dynamic_peers.len() >= MAX_DYNAMIC_UDP_PEERS {
                                if let Some(evicted_id) = self.dynamic_peers.pop_front() {
                                    if let Some(old_addr) = self.engine_to_addr.remove(&evicted_id)
                                    {
                                        self.addr_to_engine.remove(&old_addr);
                                    }
                                    let cla_addr = engine_id_to_cla_address(evicted_id);
                                    let sink = self.sink.clone();
                                    self.tasks.spawn(async move {
                                        let _ = sink.remove_peer(&cla_addr).await;
                                    });
                                }
                            }
                            self.dynamic_peers.push_back(peer_engine);
                        }
                        if self.addr_to_engine.len() >= 1024
                            && !self.addr_to_engine.contains_key(&peer_addr)
                        {
                            self.addr_to_engine.clear();
                        }
                        self.addr_to_engine.insert(peer_addr, peer_engine);
                        self.engine_to_addr.insert(peer_engine, peer_addr);

                        if is_new_peer {
                            info!(
                                "discovered new dynamic LTP peer Engine {peer_engine:?} at {peer_addr}"
                            );
                            let cla_addr = engine_id_to_cla_address(peer_engine);
                            let sink = self.sink.clone();
                            self.tasks.spawn(async move {
                                let _ = sink.add_peer(cla_addr, &[]).await;
                            });
                        }
                    }

                    if let Err(err) =
                        self.engine
                            .handle_incoming_segment(peer_engine, &rx_buf[..len], now)
                    {
                        trace!("failed to parse LTP segment from {peer_addr}: {err}");
                    }
                }

                // 2. Timer expiration
                _ = timer_future => {
                    let now = self.now_micros();
                    self.engine.handle_timer_tick(now);
                }

                // 3. Command from CLA
                cmd = self.cmd_rx.recv() => {
                    let Some(cmd) = cmd else {
                        break;
                    };
                    if !self.handle_command(cmd).await? {
                        break;
                    }
                }
            }

            // Drain outputs and notices after any event
            self.drain_outputs_udp(&socket).await?;
            self.drain_notices().await?;
        }

        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_kiss_tcp(
        &mut self,
        server_addr: SocketAddr,
        bind_addr: Option<SocketAddr>,
        port: u8,
        reconnect_initial_delay: Duration,
        reconnect_max_delay: Duration,
        pacing: KissPacingConfig,
        addr_tx: oneshot::Sender<std::io::Result<SocketAddr>>,
    ) -> Result<()> {
        // Signal server address immediately so on_register does not block
        let _ = addr_tx.send(Ok(server_addr));

        let mut current_delay = reconnect_initial_delay;
        let connect_timeout = Duration::from_secs(5);
        const MAX_KISS_ACCUMULATOR: usize = 2 * 1024 * 1024; // 2 MiB
        let cancel = self.tasks.cancel_token().clone();

        'reconnect_loop: loop {
            info!("connecting LTP CLA to KISS TCP server at {server_addr}");
            let connect_fut = async {
                if let Some(baddr) = bind_addr {
                    let socket = if baddr.is_ipv4() {
                        TcpSocket::new_v4()?
                    } else {
                        TcpSocket::new_v6()?
                    };
                    socket.bind(baddr)?;
                    socket.connect(server_addr).await
                } else {
                    TcpStream::connect(server_addr).await
                }
            };

            let connect_res = tokio::time::timeout(connect_timeout, connect_fut).await;

            match connect_res {
                Ok(Ok(stream)) => {
                    info!("connected to KISS TCP server at {server_addr}");
                    // Reset backoff delay on successful connection
                    current_delay = reconnect_initial_delay;

                    // Disable Nagle's algorithm for timely transmission of small frames (ACKs/checkpoints)
                    let _ = stream.set_nodelay(true);

                    let (mut reader, mut writer) = stream.into_split();
                    let mut raw_rx_buf = vec![0u8; 4096];
                    let mut kiss_accumulator = Vec::with_capacity(8192);

                    let mut leaky_bucket = pacing
                        .rate_limit_bytes_per_sec
                        .map(|rate| LeakyBucket::new(rate, pacing.burst_capacity_bytes));
                    let mut pacer = HalfDuplexPacer::new(&pacing);

                    // Drain any output segments queued while disconnected obeying pacing and shaper
                    let mut write_err = false;
                    if !pacer.is_paused(tokio::time::Instant::now()) {
                        while let Some((_peer, bytes)) = self.engine.poll_output() {
                            let is_cp = is_checkpoint_segment(&bytes);
                            let payload = pad_segment_for_tnc(&bytes, pacing.min_frame_len);
                            let mut frame = Vec::new();
                            encode_frame(port, Command::Data, &payload, &mut frame);

                            if let Some(ref mut shaper) = leaky_bucket {
                                shaper.acquire(frame.len()).await;
                            }

                            if let Err(e) = writer.write_all(&frame).await {
                                warn!("KISS TCP initial write error to {server_addr}: {e}");
                                write_err = true;
                                break;
                            }

                            if is_cp {
                                pacer.on_checkpoint_sent();
                                break;
                            }
                        }
                    }

                    if !write_err {
                        let disconnect_reason = 'connected_loop: loop {
                            let now = self.now_micros();
                            let next_timer = self.engine.next_timer_deadline();

                            let timer_future = async {
                                match next_timer {
                                    Some(deadline) if deadline > now => {
                                        tokio::time::sleep(Duration::from_micros(deadline - now))
                                            .await;
                                    }
                                    Some(_) => {}
                                    None => {
                                        std::future::pending::<()>().await;
                                    }
                                }
                            };

                            let pacer_deadline = pacer.next_deadline();
                            let pacer_future = async {
                                match pacer_deadline {
                                    Some(dl) => tokio::time::sleep_until(dl).await,
                                    None => std::future::pending::<()>().await,
                                }
                            };

                            tokio::select! {
                                _ = cancel.cancelled() => {
                                    debug!("LTP KISS TCP connected loop cancelled");
                                    break "driver cancelled";
                                }
                                _ = pacer_future => {
                                    debug!("Half-duplex: pacer turnaround deadline elapsed, unpausing");
                                    pacer.pause_deadline = None;
                                }
                                read_res = reader.read(&mut raw_rx_buf) => {
                                    match read_res {
                                        Ok(0) => {
                                            warn!("KISS TCP server at {server_addr} closed connection (EOF)");
                                            break "server closed connection (EOF)";
                                        }
                                        Ok(n) => {
                                            if kiss_accumulator.len() + n > MAX_KISS_ACCUMULATOR {
                                                warn!(
                                                    "KISS accumulator exceeded {MAX_KISS_ACCUMULATOR} bytes, dropping corrupt buffer"
                                                );
                                                kiss_accumulator.clear();
                                            }
                                            kiss_accumulator.extend_from_slice(&raw_rx_buf[..n]);

                                            // Extract all complete KISS frames with linear O(N) cursor traversal
                                            let mut offset = 0;
                                            while let Ok(Some((frame, consumed))) = decode_frame(&kiss_accumulator[offset..]) {
                                                offset += consumed;
                                                if frame.command == Command::Data {
                                                    if is_report_segment(&frame.payload) {
                                                        pacer.on_report_received();
                                                    }
                                                    let now = self.now_micros();
                                                    // Dynamic peer identification on KISS frames (supports shared TNC / multi-station)
                                                    let peer_engine = if let Ok(session) = Segment::peek_session(&frame.payload) {
                                                        if session.originator != self.config.engine_id {
                                                            session.originator
                                                        } else if let Some(peer) = self.engine.peer_for_session(&session) {
                                                            peer
                                                        } else {
                                                            self.engine_to_addr.keys().copied().next().unwrap_or(EngineId(2))
                                                        }
                                                    } else {
                                                        self.engine_to_addr.keys().copied().next().unwrap_or(EngineId(2))
                                                    };
                                                    let _ = self.engine.handle_incoming_segment(peer_engine, &frame.payload, now);
                                                }
                                            }
                                            if offset > 0 {
                                                kiss_accumulator.drain(..offset);
                                            }
                                        }
                                        Err(e) => {
                                            warn!("KISS TCP read error from {server_addr}: {e}");
                                            break "read error";
                                        }
                                    }
                                }

                                _ = timer_future => {
                                    let now = self.now_micros();
                                    self.engine.handle_timer_tick(now);
                                }

                                cmd = self.cmd_rx.recv() => {
                                    let Some(cmd) = cmd else {
                                        break 'reconnect_loop;
                                    };
                                    if !self.handle_command(cmd).await? {
                                        break 'reconnect_loop;
                                    }
                                }
                            }

                            // Drain outputs over KISS obeying half-duplex pacing and leaky-bucket rate limiting
                            if !pacer.is_paused(tokio::time::Instant::now()) {
                                while let Some((_peer, bytes)) = self.engine.poll_output() {
                                    let is_cp = is_checkpoint_segment(&bytes);
                                    let payload = pad_segment_for_tnc(&bytes, pacing.min_frame_len);
                                    let mut frame = Vec::new();
                                    encode_frame(port, Command::Data, &payload, &mut frame);

                                    if let Some(ref mut shaper) = leaky_bucket {
                                        shaper.acquire(frame.len()).await;
                                    }

                                    if let Err(e) = writer.write_all(&frame).await {
                                        warn!("KISS TCP write error to {server_addr}: {e}");
                                        break 'connected_loop "write error";
                                    }

                                    if is_cp {
                                        pacer.on_checkpoint_sent();
                                        break;
                                    }
                                }
                            }

                            self.drain_notices().await?;
                        };
                        debug!(
                            "KISS TCP session ended ({disconnect_reason}); will reconnect with backoff"
                        );
                    }
                }
                Ok(Err(e)) => {
                    warn!(
                        "Failed to connect to KISS TCP server at {server_addr}: {e}; retrying in {current_delay:?}"
                    );
                }
                Err(_) => {
                    warn!(
                        "Connection to KISS TCP server at {server_addr} timed out; retrying in {current_delay:?}"
                    );
                }
            }

            // Disconnected state: backoff sleep while still servicing commands and timers!
            let backoff_deadline = tokio::time::sleep(current_delay);
            tokio::pin!(backoff_deadline);

            // Compute next delay for exponential backoff (multiplier 2x, saturating, capped at reconnect_max_delay)
            current_delay = current_delay.saturating_mul(2).min(reconnect_max_delay);

            loop {
                let now = self.now_micros();
                let next_timer = self.engine.next_timer_deadline();

                let timer_future = async {
                    match next_timer {
                        Some(deadline) if deadline > now => {
                            tokio::time::sleep(Duration::from_micros(deadline - now)).await;
                        }
                        Some(_) => {}
                        None => {
                            std::future::pending::<()>().await;
                        }
                    }
                };

                tokio::select! {
                    _ = cancel.cancelled() => {
                        debug!("LTP KISS TCP backoff loop cancelled");
                        break 'reconnect_loop;
                    }

                    () = &mut backoff_deadline => {
                        // Backoff period elapsed: try reconnecting
                        break;
                    }

                    _ = timer_future => {
                        let now = self.now_micros();
                        self.engine.handle_timer_tick(now);
                        self.drain_notices().await?;
                    }

                    cmd = self.cmd_rx.recv() => {
                        let Some(cmd) = cmd else {
                            break 'reconnect_loop;
                        };
                        if !self.handle_command(cmd).await? {
                            break 'reconnect_loop;
                        }
                    }
                }
            }
        }

        Ok(())
    }

    async fn handle_command(&mut self, cmd: DriverCommand) -> Result<bool> {
        match cmd {
            DriverCommand::Forward {
                bundle_id,
                destination_engine,
                data,
                red_len,
                reply_tx,
            } => {
                if !self.engine_to_addr.contains_key(&destination_engine) {
                    let _ = reply_tx.send(Ok(ForwardBundleResult::NoNeighbour));
                    return Ok(true);
                }
                let now = self.now_micros();
                match self
                    .engine
                    .request_transmission(destination_engine, 1, data, red_len, now)
                {
                    Ok(session_id) => {
                        self.pending_transfers.insert(session_id, bundle_id);
                        let _ = reply_tx.send(Ok(ForwardBundleResult::Accepted));
                    }
                    Err(e) => {
                        let _ = reply_tx.send(Err(Error::Ltp(e)));
                    }
                }
                Ok(true)
            }
            DriverCommand::RegisterPeer {
                engine_id,
                addr,
                node_id,
            } => {
                self.static_peers.insert(engine_id, addr);
                self.dynamic_peers.retain(|&id| id != engine_id);
                self.engine_to_addr.insert(engine_id, addr);
                self.addr_to_engine.insert(addr, engine_id);
                if let Some(node) = node_id {
                    self.engine_to_node.insert(engine_id, node);
                }
                Ok(true)
            }
            DriverCommand::UnregisterPeer { engine_id } => {
                self.static_peers.remove(&engine_id);
                self.allow_ip_migration_peers.remove(&engine_id);
                self.dynamic_peers.retain(|&id| id != engine_id);
                if let Some(addr) = self.engine_to_addr.remove(&engine_id) {
                    self.addr_to_engine.remove(&addr);
                }
                self.engine_to_node.remove(&engine_id);
                Ok(true)
            }
            DriverCommand::SetPeerTiming {
                engine_id,
                rtt,
                ack_margin,
            } => {
                let rtt_micros = u64::try_from(rtt.as_micros()).unwrap_or(u64::MAX);
                let ack_margin_micros = u64::try_from(ack_margin.as_micros()).unwrap_or(u64::MAX);
                self.engine
                    .set_peer_timing(engine_id, rtt_micros, ack_margin_micros);
                Ok(true)
            }
            DriverCommand::Shutdown => {
                for (_, bundle_id) in self.pending_transfers.drain() {
                    let _ = self
                        .sink
                        .transfer_outcome(&bundle_id, TransferOutcome::Failed)
                        .await;
                }
                Ok(false)
            }
        }
    }

    async fn drain_outputs_udp(&mut self, socket: &UdpSocket) -> Result<()> {
        while let Some((peer, bytes)) = self.engine.poll_output() {
            if let Some(&dest_addr) = self.engine_to_addr.get(&peer) {
                if let Err(err) = socket.send_to(&bytes, dest_addr).await {
                    warn!("failed to send LTP datagram to {dest_addr}: {err}");
                }
            } else {
                warn!("no destination socket address configured for Engine {peer:?}");
            }
        }
        Ok(())
    }

    async fn drain_notices(&mut self) -> Result<()> {
        while let Some(notice) = self.engine.poll_notice() {
            match notice {
                Notice::RedPartReceived {
                    session: _,
                    source,
                    client_service,
                    data,
                    eob: _,
                } => {
                    if client_service != 1 {
                        warn!(
                            "ignoring received red-part for unsupported client service {client_service}"
                        );
                        continue;
                    }
                    let peer_node = self.engine_to_node.get(&source).cloned();
                    let peer_addr = engine_id_to_cla_address(source);
                    let sink = self.sink.clone();

                    self.tasks.spawn(async move {
                        debug!(
                            "delivering received bundle ({} bytes) from Engine {:?}",
                            data.len(),
                            source
                        );
                        let mut bundle_stream = Bytes::from(data);
                        match sink
                            .dispatch(peer_node.as_ref(), Some(&peer_addr), &mut bundle_stream)
                            .await
                        {
                            Ok(Acceptance::Accepted) => {}
                            Ok(Acceptance::Refused) => {
                                warn!("bundle from Engine {source:?} refused by BPA");
                            }
                            Err(err) => {
                                error!("dispatch to BPA failed: {err}");
                            }
                        }
                    });
                }
                Notice::TransmissionSessionCompleted { session } => {
                    if let Some(bundle_id) = self.pending_transfers.remove(&session) {
                        debug!("transmission session {session:?} completed for bundle {bundle_id}");
                        let sink = self.sink.clone();
                        self.tasks.spawn(async move {
                            let _ = sink
                                .transfer_outcome(&bundle_id, TransferOutcome::Completed)
                                .await;
                        });
                    }
                }
                Notice::TransmissionSessionCancelled { session, reason } => {
                    if let Some(bundle_id) = self.pending_transfers.remove(&session) {
                        warn!(
                            "transmission session {session:?} cancelled ({reason:?}) for bundle {bundle_id}"
                        );
                        let sink = self.sink.clone();
                        self.tasks.spawn(async move {
                            let _ = sink
                                .transfer_outcome(&bundle_id, TransferOutcome::Failed)
                                .await;
                        });
                    }
                }
                Notice::GreenSegmentArrived {
                    session,
                    source,
                    offset,
                    data,
                    eob,
                } => {
                    let sink = self.sink.clone();
                    let peer_node = self.engine_to_node.get(&source).cloned();
                    let peer_addr = engine_id_to_cla_address(source);

                    let Ok(offset_usize) = usize::try_from(offset) else {
                        warn!(
                            "Green segment offset {} exceeds platform address space; dropping",
                            offset
                        );
                        self.remove_green_reassembly(&session);
                        continue;
                    };

                    if offset_usize == 0 && eob {
                        // Single-segment green block (fast-path)
                        self.remove_green_reassembly(&session);
                        if data.len() > MAX_GREEN_BUNDLE_SIZE {
                            warn!(
                                "Green single-segment bundle size {} exceeds max size {}; dropping",
                                data.len(),
                                MAX_GREEN_BUNDLE_SIZE
                            );
                            continue;
                        }
                        self.tasks.spawn(async move {
                            debug!(
                                "delivering single-segment green bundle ({} bytes) from Engine {:?}",
                                data.len(),
                                source
                            );
                            let mut bundle_stream = Bytes::from(data);
                            let _ = sink
                                .dispatch(peer_node.as_ref(), Some(&peer_addr), &mut bundle_stream)
                                .await;
                        });
                    } else {
                        // Multi-segment green block reassembly
                        let Some(end_offset) = offset_usize.checked_add(data.len()) else {
                            warn!(
                                "integer overflow in Green segment offset {}; dropping",
                                offset
                            );
                            self.remove_green_reassembly(&session);
                            continue;
                        };

                        if end_offset > MAX_GREEN_BUNDLE_SIZE {
                            warn!(
                                "Green segment session {:?} end offset {} exceeds max size {}; dropping",
                                session, end_offset, MAX_GREEN_BUNDLE_SIZE
                            );
                            self.remove_green_reassembly(&session);
                            continue;
                        }

                        if !self.green_reassemblies.contains_key(&session)
                            && self.green_reassemblies.len() >= MAX_GREEN_REASSEMBLIES
                        {
                            // Sweep expired reassemblies before rejecting new sessions
                            self.prune_expired_green_reassemblies(Duration::from_secs(60));
                            if self.green_reassemblies.len() >= MAX_GREEN_REASSEMBLIES {
                                warn!(
                                    "too many concurrent Green reassemblies; dropping new session {:?}",
                                    session
                                );
                                continue;
                            }
                        }

                        let reassembly_result = {
                            let now_inst = Instant::now();
                            let entry = self
                                .green_reassemblies
                                .entry(session)
                                .or_insert_with(|| GreenReassembly::new(source, now_inst));

                            if entry.total_len.is_some_and(|total| end_offset > total)
                                || (eob
                                    && (entry.total_len.is_some_and(|prev| prev != end_offset)
                                        || entry.max_end_offset() > end_offset))
                            {
                                Err("inconsistent Green EOB bounds")
                            } else if entry
                                .insert(offset_usize, &data, now_inst, &mut self.total_green_bytes)
                                .is_err()
                            {
                                Err("global Green reassembly buffer cap (32 MiB) exceeded")
                            } else {
                                if eob {
                                    entry.total_len = Some(end_offset);
                                }
                                Ok(entry.take_complete(&mut self.total_green_bytes))
                            }
                        };

                        match reassembly_result {
                            Err(reason) => {
                                warn!("{reason} for session {:?}; dropping", session);
                                self.remove_green_reassembly(&session);
                                continue;
                            }
                            Ok(Some(complete_data)) => {
                                self.green_reassemblies.remove(&session);
                                self.tasks.spawn(async move {
                                    debug!(
                                        "delivering reassembled green bundle ({} bytes) from Engine {:?}",
                                        complete_data.len(),
                                        source
                                    );
                                    let mut bundle_stream = Bytes::from(complete_data);
                                    let _ = sink
                                        .dispatch(
                                            peer_node.as_ref(),
                                            Some(&peer_addr),
                                            &mut bundle_stream,
                                        )
                                        .await;
                                });
                            }
                            Ok(None) => {}
                        }

                        // Periodic cleanup of stale green reassemblies (older than 30s)
                        if self.green_reassemblies.len() > 128 {
                            self.prune_expired_green_reassemblies(Duration::from_secs(30));
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::TurnaroundMode;

    #[test]
    fn test_is_checkpoint_and_report_segment() {
        // Red Data non-CP (0)
        assert!(!is_checkpoint_segment(&[0x00, 0x11, 0x22]));
        // Red Data Checkpoints (1, 2, 3)
        assert!(is_checkpoint_segment(&[0x01, 0x11, 0x22]));
        assert!(is_checkpoint_segment(&[0x02, 0x11, 0x22]));
        assert!(is_checkpoint_segment(&[0x03, 0x11, 0x22]));
        // Red Data EORP non-CP (4)
        assert!(!is_checkpoint_segment(&[0x04, 0x11, 0x22]));
        // Report Segment (8)
        assert!(is_report_segment(&[0x08, 0x11, 0x22]));
        // Report Ack (9)
        assert!(!is_report_segment(&[0x09, 0x11, 0x22]));
        // Empty buffer
        assert!(!is_checkpoint_segment(&[]));
        assert!(!is_report_segment(&[]));
    }

    #[tokio::test]
    async fn test_leaky_bucket_pacing() {
        let mut bucket = LeakyBucket::new(10_000, 500); // 10,000 B/s, max burst 500 B
        let start = tokio::time::Instant::now();

        // Immediate consumption of initial burst
        bucket.acquire(500).await;
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_millis(50),
            "Burst acquisition should be near instantaneous"
        );

        // Consuming 1000 more bytes at 10,000 B/s requires ~100ms
        bucket.acquire(1000).await;
        let elapsed = start.elapsed();
        assert!(
            elapsed >= Duration::from_millis(80),
            "Paced acquisition should sleep for token replenishment"
        );
    }

    #[tokio::test]
    async fn test_half_duplex_pacer_wait_for_report() {
        let cfg = KissPacingConfig {
            min_frame_len: 0,
            rate_limit_bytes_per_sec: None,
            burst_capacity_bytes: 512,
            half_duplex: true,
            turnaround_mode: TurnaroundMode::WaitForReport,
            turnaround_timeout: Duration::from_millis(300),
            dwell_time: Duration::from_millis(100),
        };
        let mut pacer = HalfDuplexPacer::new(&cfg);

        let now = tokio::time::Instant::now();
        assert!(!pacer.is_paused(now));

        // Checkpoint emitted -> paused
        pacer.on_checkpoint_sent();
        assert!(pacer.is_paused(tokio::time::Instant::now()));

        // Report Segment received -> unpaused immediately
        pacer.on_report_received();
        assert!(!pacer.is_paused(tokio::time::Instant::now()));
    }

    #[tokio::test]
    async fn test_half_duplex_pacer_turnaround_timeout() {
        let cfg = KissPacingConfig {
            min_frame_len: 0,
            rate_limit_bytes_per_sec: None,
            burst_capacity_bytes: 512,
            half_duplex: true,
            turnaround_mode: TurnaroundMode::WaitForReport,
            turnaround_timeout: Duration::from_millis(50),
            dwell_time: Duration::from_millis(100),
        };
        let mut pacer = HalfDuplexPacer::new(&cfg);

        pacer.on_checkpoint_sent();
        assert!(pacer.is_paused(tokio::time::Instant::now()));

        // Wait past turnaround timeout
        tokio::time::sleep(Duration::from_millis(70)).await;
        assert!(
            !pacer.is_paused(tokio::time::Instant::now()),
            "Pacer should unpause when turnaround timeout expires"
        );
    }

    #[tokio::test]
    async fn test_half_duplex_pacer_fixed_dwell() {
        let cfg = KissPacingConfig {
            min_frame_len: 0,
            rate_limit_bytes_per_sec: None,
            burst_capacity_bytes: 512,
            half_duplex: true,
            turnaround_mode: TurnaroundMode::FixedDwell,
            turnaround_timeout: Duration::from_millis(300),
            dwell_time: Duration::from_millis(60),
        };
        let mut pacer = HalfDuplexPacer::new(&cfg);

        pacer.on_checkpoint_sent();
        assert!(pacer.is_paused(tokio::time::Instant::now()));

        // In FixedDwell mode, receiving an RS does NOT cancel the dwell pause
        pacer.on_report_received();
        assert!(pacer.is_paused(tokio::time::Instant::now()));

        // Once dwell_time expires, pacer unpauses
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert!(
            !pacer.is_paused(tokio::time::Instant::now()),
            "Pacer should unpause after dwell time"
        );
    }

    #[test]
    fn test_green_reassembly_out_of_order_overlap_and_gap_check() {
        let now = Instant::now();
        let mut total_bytes = 0usize;
        let mut reassembly = GreenReassembly::new(EngineId(2), now);

        // Out-of-order EOB arrives first at [20..30), gap at [0..20)
        assert!(
            reassembly
                .insert(20, b"0123456789", now, &mut total_bytes)
                .is_ok()
        );
        reassembly.total_len = Some(30);
        assert_eq!(total_bytes, 10);
        assert!(!reassembly.is_complete());

        // Overlapping duplicate at [10..25) -> only 15 total unique bytes in [10..30)
        // [10..25) overlaps [20..30), coalesced chunk is [10..30) = 20 bytes
        assert!(
            reassembly
                .insert(10, b"ABCDEFGHIJ01234", now, &mut total_bytes)
                .is_ok()
        );
        assert_eq!(total_bytes, 20);
        assert_eq!(reassembly.stored_bytes, 20);
        assert!(
            !reassembly.is_complete(),
            "gap [0..10) must prevent completion even after overlapping retransmissions"
        );

        // Re-sending overlapping [10..25) must NOT inflate total_bytes or trigger completion
        assert!(
            reassembly
                .insert(10, b"ABCDEFGHIJ01234", now, &mut total_bytes)
                .is_ok()
        );
        assert_eq!(total_bytes, 20);
        assert!(!reassembly.is_complete());

        // Missing prefix [0..10) arrives -> completes contiguously
        assert!(
            reassembly
                .insert(0, b"0123456789", now, &mut total_bytes)
                .is_ok()
        );
        assert_eq!(total_bytes, 30);
        let assembled = reassembly
            .take_complete(&mut total_bytes)
            .expect("contiguous 0..30 must complete");
        assert_eq!(assembled, b"0123456789ABCDEFGHIJ0123456789");
        assert_eq!(total_bytes, 0);
        assert_eq!(reassembly.stored_bytes, 0);
    }

    #[test]
    fn test_green_reassembly_enforces_32mib_global_cap() {
        let now = Instant::now();
        let mut total_bytes = 0usize;
        let chunk_16m = vec![0xABu8; 16 * 1024 * 1024];

        let mut s1 = GreenReassembly::new(EngineId(2), now);
        assert!(s1.insert(0, &chunk_16m, now, &mut total_bytes).is_ok());
        assert_eq!(total_bytes, 16 * 1024 * 1024);

        let mut s2 = GreenReassembly::new(EngineId(3), now);
        assert!(s2.insert(0, &chunk_16m, now, &mut total_bytes).is_ok());
        assert_eq!(total_bytes, MAX_TOTAL_GREEN_BYTES);

        // Adding 1 more byte across any session must fail against 32 MiB cap
        let mut s3 = GreenReassembly::new(EngineId(4), now);
        assert!(s3.insert(0, b"X", now, &mut total_bytes).is_err());
        assert_eq!(total_bytes, MAX_TOTAL_GREEN_BYTES);
    }
}
