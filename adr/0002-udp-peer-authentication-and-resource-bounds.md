# ADR-0002: UDP Peer Authentication, Anti-Spoofing, and Explicit Opt-In Modes

- **Status**: Accepted
- **Scope**: `src/config.rs`, `src/cla.rs`, `src/driver.rs`, `src/main.rs`

## Context

LTP over UDP is stateless and unauthenticated at the network layer unless paired with RFC 5327 LTP Authentication or IPsec/WireGuard. Previously, `Driver::run_udp`:

1. Short-circuited lookup via `addr_to_engine` and updated `engine_to_addr` dynamically for any received `EngineId`.
2. Allowed an attacker on another IP address to spoof a victim peer's `session.originator` `EngineId`, overwriting `engine_to_addr[victim]` and hijacking outbound Report Segments and subsequent bundle traffic.
3. Auto-registered arbitrary unknown `EngineId`s into the BPA sink (`sink.add_peer`) without bounds, enabling peer table exhaustion.
4. Cleared `engine_to_addr` indiscriminately if growth crossed 1024 entries, risking eviction of static peers.

At the same time, legitimate deployments rely on:
- Same-IP ephemeral port changes (e.g., NAT rebinding or ephemeral test sockets).
- Explicit mobility/pre-secured links where IP migration or zero-config peer discovery is intentional.

## Decisions

1. **Always Peek Session Header First**:
   - `run_udp` inspects `Segment::peek_session` before resolving `peer_engine`, preventing a cached socket address from masking a spoofed `session.originator` in `Engine::handle_incoming_segment`.
2. **Secure-by-Default IP Pinning & Unknown-Peer Rejection (`allow_unauthenticated_udp = false`)**:
   - Configured/registered peers are recorded in `static_peers`.
   - Same-IP port rebinding (`static_addr.ip() == peer_addr.ip()`) is permitted automatically.
   - Cross-IP updates (`static_addr.ip() != peer_addr.ip()`) are rejected with a warning unless either per-peer `PeerEntry.allow_ip_migration == true` or CLA-wide `allow_unauthenticated_udp == true` is explicitly configured.
   - Unknown peers not in `static_peers` are rejected with a warning unless `allow_unauthenticated_udp == true`.
3. **Explicit Opt-In Controls**:
   - Configuration: `PeerEntry.allow_ip_migration: bool` (`allow-ip-migration` in YAML/TOML, default `false`).
   - Library API: `LtpCla::with_allow_unauthenticated_udp(mut self, allow: bool) -> Self` (default `false`).
   - CLI / Env: `--allow-unauthenticated-udp` / `DTN_LTP_ALLOW_UNAUTHENTICATED_UDP`.
4. **Bounded Dynamic Peer Table & Static Preservation**:
   - When `allow_unauthenticated_udp == true`, dynamic unknown peers registered via `sink.add_peer` are tracked in a FIFO queue capped at `256` entries (`MAX_DYNAMIC_UDP_PEERS`), evicting the oldest dynamic peer (`sink.remove_peer`) when exceeded.
   - When `engine_to_addr` reaches 1024 entries, pruning retains all `static_peers` keys and removes only dynamic entries.
5. **Credential File Support (`--token-file`)**:
   - `src/main.rs` adds `--token-file <PATH>` (`DTN_HDY_TOKEN_FILE`) for gRPC BPA Bearer authentication to avoid passing secrets via process arguments.

## Consequences

- Cross-IP spoofing and unauthenticated peer injection are blocked out of the box while same-IP NAT/port rebinding continues to work seamlessly.
- Operators running over pre-secured overlays (WireGuard, IPsec) or isolated test harnesses can explicitly opt in to IP migration or dynamic peer discovery with bounded memory impact.
