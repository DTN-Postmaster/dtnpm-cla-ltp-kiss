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

## License

Dual-licensed under the Apache License, Version 2.0 or the MIT License. See [LICENSE.md](LICENSE.md).
