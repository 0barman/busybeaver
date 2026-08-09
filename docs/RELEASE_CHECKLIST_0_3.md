# BusyBeaver 0.3 release checklist

Recorded on 2026-08-08 and revalidated after the 2026-08-09 coded-error
hardening. Commands are run from `busybeaver/` unless a manifest path is shown.

## Required local gates

| Gate | Command/evidence | Status |
|---|---|---|
| Format | `cargo fmt --all -- --check` | pass |
| Static analysis | `cargo clippy --all-targets --all-features -- -D warnings` | pass |
| No production expect | `cargo clippy --lib --all-features -- -D clippy::expect_used` | pass; production `.expect()` count is zero |
| Full tests | `cargo test --all-features` | pass in final sweep; 450 unit/integration/doc tests green |
| Minimal features | `cargo test --no-default-features` | pass in final sweep; 450 unit/integration/doc tests green |
| Rustdoc | `cargo test --doc --all-features` | pass: 7 compiled, 0 ignored |
| Strict API docs | `RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --all-features` | pass: all public items documented; no broken links or rustdoc warnings |
| Loom | `cargo test --test loom_state_models` | pass: 4 models |
| Native cross-check | `cargo check --lib --target aarch64-pc-windows-msvc`; `i686-pc-windows-msvc` | pass |
| Panic abort compile | `RUSTFLAGS=-C panic=abort cargo check --lib` | pass |
| Performance | `cargo bench --bench legacy_baseline` | pass under recorded absolute threshold; see quality baseline |
| Rust 1.89 fallback | `cargo +1.89.0 check --all-targets --all-features` | pass; useful local evidence but not a substitute for exact MSRV 1.88 |
| Package | `cargo package --allow-dirty` | pass: package verified by Cargo; 86 archive entries and no `tmp`, `target`, or review material |

## Environment-dependent gates

- Exact Rust 1.88 installation was attempted locally but the rustup artifact
  download produced zero-length partial files and timed out after ten minutes.
  Rust 1.89 is locally installed, but is not counted as 1.88 evidence. The CI
  `msrv` job installs 1.88.0 and runs all-target check plus lib tests.
- `cargo-llvm-cov` 0.8.7 and LLVM tools were installed. Three Windows runs
  (full, selected integration targets, and lib-only) each remained in the
  instrumented build/link step until their 3–10 minute command timeout and
  produced no percentage. This is recorded as a local tool timeout, not a pass.
  Linux CI runs the full suite with a 90% line threshold.
- Local `cargo-semver-checks` was unavailable. Installing it was attempted but
  repeated crates.io low-throughput timeouts prevented installation. CI uses the official action with
  baseline revision `df6bb1f` (the repository's 0.2.0 manifest commit). Because
  0.2 to 0.3 is an intentional pre-1.0 minor-version boundary, detected API
  changes must still agree with the migration guide.
- Local `cargo-mutants` was unavailable. The opt-in CI mutation job targets
  `src/retry.rs` and `src/schedule.rs`; this remains a release evidence item,
  not a locally passed gate.
- Linux and macOS cannot be executed from this Windows host. The native CI
  matrix is the required evidence for those platforms.
- The WebAssembly diagnostic is enforced by source and CI. Wasm remains
  unsupported; it is not treated as a successful target build.

## Release blockers

The source tree is ready for local final validation, but publishing must remain
blocked until the CI run supplies all environment-dependent evidence above.
No missing tool or timed-out tool is recorded as passing.
