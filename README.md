# dtnpm-cla-ltp-kiss

Licklider Transmission Protocol (LTP, RFC 5326 / RFC 7122) and KISS Convergence Layer Adapter (CLA) for Hardy BPv7 and **DTN Postmaster** (`dtnpm`).

## Workspace Structure

- `.` (`dtnpm-cla-ltp-kiss`): Hardy BPv7 CLA adapter and standalone gRPC CLA daemon binary (`dtnpm-cla-ltp-kiss`).
- `ltp/` (`hardy-ltp`): Protocol state machine, segment codecs, and UDP / KISS transports.
- `fuzz/` and `ltp/fuzz/`: `cargo-fuzz` targets (`dtnpm-cla-ltp-kiss-fuzz`, `hardy-ltp-fuzz`).

## Features

- **UDP Transport (RFC 7122)**: Standard LTP over UDP datagrams (default port 1113).
- **KISS Transport**: LTP frames encapsulated in KISS for AX.25 packet radio / Direwolf TNCs.
- **Full BPA Integration**: Implements `hardy_bpa::cla::Cla` and runs either embedded in `dtnpm` or standalone via `cli` feature and `hardy-proto` gRPC (`example_config.yaml`).

## Build and Test

Requires sibling checkouts of `../dtnpm-core` and `../../hardy/*` as referenced in `Cargo.toml`.

```bash
cargo test --workspace --exclude dtnpm-cla-ltp-kiss-fuzz --exclude hardy-ltp-fuzz
```

## Security & Compliance

- **Cyber Resilience Act (CRA)**: Secure-by-default UDP peering (`static_peers` enforced, dynamic map capped to 256 peers), VEX audit documentation (`RUSTSEC-2026-0459`), and SHA-pinned CI actions.
- **Explicit Permissive Mode (Opt-in)**: Unauthenticated UDP ingestion or dynamic IP migration requires explicit user activation via `--allow-unauthenticated-udp` or `allow_unauthenticated_udp = true`.
- **Resource Bounds & Protocol Safety**: Chunked `BTreeMap` green reassembly with strict contiguous boundary validation (32 MiB max cap), bounded receive sessions (1,024 slots), `sparse_red` coalescing, and safe KISS framing (`0xFF` padding).
- **Rust Quality Standards**: 100% `unwrap`-free and `panic!`-free production codebase.
- **Documentation**: See [`AGENTS.md`](AGENTS.md), [`CONTRIBUTING.md`](CONTRIBUTING.md), [`SECURITY.md`](SECURITY.md), and Architectural Decision Records in [`adr/`](adr/).

## License

Dual-licensed under the Apache License, Version 2.0 or the MIT License. See [LICENSE.md](LICENSE.md).
