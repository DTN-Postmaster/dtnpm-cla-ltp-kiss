# Agent Guide — `dtnpm-cla-ltp-kiss`

## 🎯 Mission
Provide the LTP (Licklider Transmission Protocol - RFC 5326 / RFC 7122) and KISS Convergence Layer Adapter (CLA) for the DTN Postmaster ecosystem, including the protocol state engine (`hardy-ltp`), encapsulation for radio TNCs (Direwolf, AX.25 packet radio), and the standalone gRPC daemon.

---

## 🛡️ Security Frameworks and Requirements

### 1. Cyber Resilience Act (CRA) & Secure-by-Default
- **Default UDP Peer Authentication**:
  - By default, UDP datagrams are only accepted if they originate from addresses registered in `static_peers`.
  - The dynamic `engine_id -> SocketAddr` mapping table is strictly bounded to 256 active peers to prevent memory exhaustion.
- **Explicit Opt-In for Open / Lab Deployments**:
  - Accepting UDP packets from unknown addresses or dynamic IP address migration (`allow_ip_migration`) IS ONLY ENABLED if the user EXPLICITLY requests it via the `--allow-unauthenticated-udp` CLI flag or in the configuration (`allow_unauthenticated_udp = true`).
  - Useful and permitted for development, testing, or networks already secured at the transport layer (WireGuard, encrypted tunnels).
- **Supply-Chain & VEX**:
  - `cargo-audit` and `cargo-deny` audits with zero failures; VEX `not_affected` statement recorded for `RUSTSEC-2026-0459`.
  - CI actions pinned by 40-character SHA-1.
- **Security Policy**: Documented in [SECURITY.md](SECURITY.md).

### 2. Protocol Robustness & Memory Bounds
- **Bounded Green Reassembly**:
  - Reaggregation of unreliable Green fragments uses an ordered `BTreeMap` storing the actual received chunks, without preallocating a giant vector (preventing 1 GiB OOM amplification attacks).
  - Strict contiguity validation: a bundle is only delivered to the BPA if the sum of contiguous fragment lengths exactly covers the declared size.
  - Global memory cap enforced at 32 MiB per session.
- **LTP Engine (`hardy-ltp`)**:
  - Strict limit of 1,024 concurrent receive sessions (`RecvSession`).
  - Coalescence of sparse Red blocks (`sparse_red`) to prevent quota leaks.
  - Clean cancellation of retransmission timers with cancellation notice (`record_cancelled`).
- **KISS Framing**:
  - Uses a compliant `0xFF` extension padding tag (avoiding any collision with the `0x00` end byte).
  - Safe frame slicing (`cp_end.max(1)`).

---

## 🦀 Rust Engineering Standards

1. **Zero Panic / Zero Unwrap Rule**:
   - All code (`src/` and `ltp/src/`) must be 100% free of `.unwrap()`, `.expect()`, and the `panic!` macro.
   - Strict linter: `-D warnings -D clippy::unwrap_used -D clippy::panic`.
2. **Mandatory ADRs**:
   - Any LTP/KISS architectural decision must be recorded under `adr/` and indexed in [adr/README.md](adr/README.md).
3. **Documentation Language**:
   - English must be used as the language for all documentation, unless the file is suffixed with `.fr.md` or another language is explicitly requested in the prompt.

---

## ⚡ Critical Verification Commands

- `cargo fmt --all -- --check`
- `cargo clippy --offline --workspace --lib --bins -- -D warnings -D clippy::unwrap_used -D clippy::panic`
- `cargo test --offline --all-features`
- `./scripts/check-security.sh --strict`
- `./scripts/check-coverage.sh --summary-only`
