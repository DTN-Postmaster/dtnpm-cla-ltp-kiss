# Security Policy

## Supported Versions

| Version | Supported |
| --- | --- |
| `0.2.x` | Yes |
| `< 0.2.0` | No |

## Threat Model & Security Controls

`dtnpm-cla-ltp-kiss` exposes two operational transport modes over the Licklider Transmission Protocol (`hardy-ltp`, RFC 5326):

1. **LTP over UDP** (IP networks / Ethernet / tunnels)
2. **LTP over KISS-TCP** (amateur radio TNCs such as Direwolf and Mercury)

### UDP Peer Authentication & Anti-Spoofing (`LTP-01`)

UDP provides no transport-layer peer identity guarantee, and LTP headers carry a self-asserted originator Engine ID (`session.originator`). To prevent cross-IP routing hijacks and unauthenticated peer table exhaustion:

- **Default Secure Posture (`allow_unauthenticated_udp = false`)**:
  - Every incoming UDP datagram is peeked for its LTP `SessionId` header before routing lookup so a shared/NAT socket cannot spoof a different `EngineId`.
  - Unknown dynamic UDP peers are rejected by default.
  - Known static/registered peers (`config.peers` or `register_peer`) enforce IP pinning (`static_addr.ip() == peer_addr.ip()`). Same-IP ephemeral port rebinding remains allowed; cross-IP source changes are rejected and logged.
- **Explicit Opt-In Permissive Modes** (use only on pre-secured links or isolated labs):
  - `allow-ip-migration: true` (per-peer in `config.peers`) allows a specific static peer to migrate across IP addresses.
  - `--allow-unauthenticated-udp` / `DTN_LTP_ALLOW_UNAUTHENTICATED_UDP=true` / `LtpCla::with_allow_unauthenticated_udp(true)` permits unauthenticated dynamic UDP peer discovery and cross-IP migration. Dynamic peer registrations are FIFO-capped at `256` entries (`MAX_DYNAMIC_UDP_PEERS`), and static peers are never evicted during table pruning.

### KISS-over-TCP Framing & Extension Tagging (`CLA-01`)

- KISS bridges are assumed to be trusted local or dedicated radio TNC processes.
- Minimum-frame padding for TNCs (`pad_segment_for_tnc`) uses RFC 5326 trailer extension tag `0xFF` (experimental/private-use range) rather than reserved tag `0x00` (RFC 5327 LTP Authentication).

### Resource & Memory Exhaustion Bounds (`LTP-02`, `LTP-03`)

- **Pre-allocation validation**: Inbound data segments for unknown sessions validate `offset + data.len() <= max_block_size` (and `max_recv_sessions`) prior to allocating a `RecvSession` or arming timers.
- **Disjoint reassembly accounting**: Out-of-order red segments (`sparse_red`) and multi-segment green blocks (`GreenReassembly`) coalesce overlapping/adjacent chunks in `BTreeMap` structures rather than zero-filling sparse offsets or overcounting duplicate bytes. Multi-segment green blocks are additionally bounded by a 16 MiB per-bundle limit, 64 concurrent sessions, and a 32 MiB global Green buffer cap (`MAX_TOTAL_GREEN_BYTES`).
- **Timer & cancellation hygiene**: Transitions into sender or receiver cancellation (including RS retransmission exhaustion) purge stale session timers via `timer_table.cancel_session` and record the session in bounded cancellation isolation.
- **Panic freedom**: Production code under `src/` and `ltp/src/` forbids `.unwrap()` and `panic!` (enforced in CI via `-D clippy::unwrap_used -D clippy::panic`).

### BPA Control Plane Credentials (`XC-05`)

- When connecting to a protected Hardy BPA gRPC endpoint, prefer `--token-file <PATH>` (`DTN_HDY_TOKEN_FILE`) or Unix Domain Socket authentication (`--socket` / `DTN_HDY_SOCKET`) over passing `--token` on the command line so secrets are not exposed in process listings.

## Supply-Chain & EU CRA Vulnerability Disclosure (VEX)

- YAML configuration parsing uses `yaml_serde` (`libyaml-rs`), avoiding the unmaintained `serde_yaml` / `unsafe-libyaml` dependency (`RUSTSEC-2024-0449`).
- **VEX (`RUSTSEC-2026-0459` / `CVE-2026-119375`)**: `libsqlite3-sys 0.38.2` appears strictly as an unused transitive dependency of `dtnpm-core -> rusqlite 0.40.2`. Status: **Not Affected** — `dtnpm-cla-ltp-kiss` never creates, opens, or queries SQLite/WAL databases.

## Reporting a Vulnerability

Please **do not** report security vulnerabilities in public GitHub issues.

1. Report privately via GitHub Security Advisories for this repository or contact the maintainers directly through private channels.
2. Include the affected crate/component (`hardy-ltp` or `dtnpm-cla-ltp-kiss`), transport mode (`UDP` or `KISS-TCP`), minimal configuration, and packet/segment bytes or reproduction test if available.
3. We aim to acknowledge reports within 48 hours, provide an initial triage assessment within 5 business days, and coordinate remediation and disclosure within 90 days.
