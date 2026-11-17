# Contributing Guide — `dtnpm-cla-ltp-kiss`

This guide outlines the development rules and security requirements for the LTP and KISS CLA in `dtnpm-cla-ltp-kiss`.

---

## 1. Core Principles

- **Zero Panic / Zero Unwrap**: No `.unwrap()` or `.expect()` calls in production code.
- **Resource Bounds and Security**:
  - UDP peer authentication is mandatory by default (`static_peers`), with explicit opt-in (`--allow-unauthenticated-udp`) reserved for intentional use.
  - All network segment reassembly must be memory-bounded and validate strict contiguity before delivery to the BPA.
- **Architectural Traceability (ADR)**:
  - Any protocol addition or adjustment must be documented in an ADR file under `adr/` and indexed in `adr/README.md`.

---

## 2. Local Verification

```bash
cargo fmt --all -- --check
cargo clippy --offline --workspace --lib --bins -- -D warnings -D clippy::unwrap_used -D clippy::panic
cargo test --offline --all-features
./scripts/check-security.sh --strict
```
