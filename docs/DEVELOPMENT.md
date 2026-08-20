# BusyBeaver development guide

This guide is for contributors and maintainers working on BusyBeaver itself. For application
integration, start with the [README](../README.md) and the
[integration guide](INTEGRATION_en.md).

## Prerequisites

- Rust 1.88 for MSRV verification.
- The current stable Rust toolchain with `rustfmt` and Clippy.
- A native target supported by Tokio.
- Git.

The commands below assume the repository root is the current directory and the crate manifest is
`busybeaver/Cargo.toml`.

## Repository map

| Path | Purpose |
| --- | --- |
| `busybeaver/src/` | Library implementation and rustdoc. |
| `busybeaver/tests/` | Public API, lifecycle, concurrency, and regression tests. |
| `busybeaver/docs/` | Integration, contract, migration, error, and verification documents. |
| `busybeaver/README.md` | GitHub and crates.io entry point. |
| `plan/` | Design and implementation rationale. |
| `.github/workflows/ci.yml` | Cross-platform and release-gate automation. |

## Fast local feedback

Run these commands before the full release gate:

```text
cargo fmt --manifest-path busybeaver/Cargo.toml --all -- --check
cargo check --manifest-path busybeaver/Cargo.toml --all-targets --all-features
cargo clippy --manifest-path busybeaver/Cargo.toml --all-targets --all-features -- -D warnings
cargo test --manifest-path busybeaver/Cargo.toml --all-targets --all-features
```

When changing feature-gated logging or dependencies, also run:

```text
cargo check --manifest-path busybeaver/Cargo.toml --all-targets --no-default-features
cargo test --manifest-path busybeaver/Cargo.toml --all-targets --no-default-features
```

## Required release gate

Every release candidate must pass all of the following:

```text
cargo fmt --manifest-path busybeaver/Cargo.toml --all -- --check
cargo clippy --manifest-path busybeaver/Cargo.toml --all-targets --all-features -- -D warnings
cargo test --manifest-path busybeaver/Cargo.toml --all-targets --all-features -- --include-ignored
cargo test --manifest-path busybeaver/Cargo.toml --release --all-targets --all-features
cargo test --manifest-path busybeaver/Cargo.toml --doc --all-features
RUSTDOCFLAGS="-D warnings" cargo doc --manifest-path busybeaver/Cargo.toml --no-deps --all-features
RUSTFLAGS="-C panic=abort" cargo check --manifest-path busybeaver/Cargo.toml --lib --all-features
cargo package --manifest-path busybeaver/Cargo.toml
```

Verify the MSRV separately:

```text
rustup run 1.88.0 cargo check --manifest-path busybeaver/Cargo.toml --all-targets --all-features
```

GitHub Actions repeats supported checks on Linux, macOS, and Windows with Rust 1.88 and stable.
Local success does not replace the cross-platform CI result.

## Implementation rules

### Fallible behavior

- A recoverable public failure returns `Result`, a typed wait error, or a typed terminal failure.
- Public error enums remain `#[non_exhaustive]` so compatible variants can be added.
- Do not make application policy depend on `Display` text. Add or preserve a typed variant.
- Stable public diagnostic strings use the `BB-*` namespace and are documented in
  [ERROR_CODES.md](ERROR_CODES.md).
- An internal-only invariant failure must log a stable diagnostic code without exposing business
  values, business errors, panic text, or metadata.

### Panic-free production policy

Production code must not introduce:

- `unwrap`, `expect`, `expect_err`, or `unwrap_err`;
- `panic!`, `todo!`, `assert!`, `assert_eq!`, `unimplemented!`, or `unreachable!`;
- `RefCell` dynamic borrows or mutable borrows;
- unchecked indexing;
- unchecked arithmetic on state, sequence, attempt, or schedule counters;
- new `unsafe` blocks.

The crate-level Clippy denies and `production_safety_audit_tests` enforce this policy. Assertions
and deliberate panic fixtures are allowed in tests when they are the behavior being verified.
Tokio `watch::Receiver::borrow()` is not a `RefCell` borrow; keep its guard short and never hold it
across an `.await`.

### Concurrency and lifecycle

- Never call user futures, callbacks, hashing/equality implementations, destructors, or hooks while
  an internal state lock is held.
- Keep lock critical sections short and document any required lock ordering.
- Admission must either publish every required registry/queue record or roll back completely.
- Cancellation and terminal publication are first-wins transitions.
- Dropping a public wait future must not revoke an operation already accepted by a synchronous API.
- Queue cancellation must restore capacity and ordering-key state exactly once.
- Parent terminal cleanup closes child admission before cancelling and joining remaining children.
- Runtime loss must wake waiters and terminalize accepted handles; it must not create detached
  pending futures.

### Time and retry

- Capture documented duration-based deadlines at the synchronous API boundary, not at first poll.
- Use checked `Duration` and `Instant` arithmetic.
- SDK-owned waits must observe cancellation and re-check it before the next side effect.
- Do not retry an attempt timeout unless the caller explicitly authorized that risk.
- Prefer paused Tokio time or injected boundaries over long wall-clock sleeps in tests.

### Privacy and observability

- Events, snapshots, shutdown reports, and default tracing fields stay bounded.
- Do not place `T`, `E`, panic messages, ordering-key bytes, tags, or arbitrary metadata into metrics
  labels or default logs.
- Keep event publication non-blocking; slow subscribers receive lag instead of backpressure on
  execution.

## Testing expectations

Choose the narrowest deterministic test that proves the contract:

| Change | Primary evidence |
| --- | --- |
| Pure arithmetic or state transition | Unit test next to the implementation. |
| Public API behavior | Integration test under `busybeaver/tests/`. |
| Cancellation, deadline, or schedule | Paused-time test with explicit synchronization. |
| Queue or lifecycle race | Barrier-controlled regression test plus stress coverage where useful. |
| Panic boundary | A deliberate test fixture proving classification and worker survival. |
| Memory ownership | Drop/weak-reference regression test. |
| Documentation API | Doctest or a compiled documentation-example integration test. |

A passing stress test is supplementary evidence; it does not replace a deterministic invariant
test. Do not add `#[ignore]` to correctness tests or rustdoc examples. If a platform-specific test
cannot run everywhere, document the target and gate it explicitly.

## Documentation standards

- The README is the GitHub and crates.io landing page. Keep the quick start short and compilable.
- Public types, variants, cancellation behavior, future drop behavior, and runtime requirements
  belong in rustdoc.
- Behavioral guarantees belong in [API_CONTRACT_0_3.md](API_CONTRACT_0_3.md).
- Upgrade breaks and compatibility shims belong in [MIGRATION_0_2_TO_0_3.md](MIGRATION_0_2_TO_0_3.md)
  and [CHANGELOG.md](../CHANGELOG.md).
- Keep the English and Chinese integration guides semantically aligned.
- Use relative links for repository documents and HTTPS links for external resources.
- Add a language identifier to every fenced code block.
- Use complete Rust examples in GitHub Markdown; rustdoc-only hidden lines beginning with `#` are not
  appropriate in standalone `.md` files.
- Wrap prose consistently, leave blank lines around headings/lists/fences, and avoid skipped heading
  levels.

After changing documentation, run doctests, rustdoc with warnings denied, the compiled documentation
example tests, and `cargo package` so missing packaged files or broken relative paths are caught.

## Pull-request checklist

- [ ] The change has one clear behavioral objective.
- [ ] Public behavior and compatibility impact are documented.
- [ ] New fallible paths return typed errors or stable diagnostic codes.
- [ ] Deterministic tests cover the success, failure, cancellation, and relevant race boundaries.
- [ ] No correctness test or rustdoc example is ignored.
- [ ] README, rustdoc, integration, migration, error-code, and changelog documents are updated where
      applicable.
- [ ] Formatting, strict Clippy, all feature combinations, doctests, and release tests pass.
- [ ] No sensitive business data is introduced into events, reports, metrics, or logs.

## Release checklist

1. Confirm the version and `rust-version` in `Cargo.toml`.
2. Update `CHANGELOG.md`, the API contract status, migration notes, and verification matrix.
3. Run the required release gate and MSRV check.
4. Run `cargo package` and inspect the packaged file list.
5. Push the candidate and wait for every GitHub Actions matrix job to pass.
6. Run `cargo publish --dry-run --manifest-path busybeaver/Cargo.toml` from the exact release commit.
7. Publish only from the reviewed, tagged commit using authorized crates.io credentials.

## Design and verification references

- [BusyBeaver 0.3 API contract](API_CONTRACT_0_3.md)
- [BusyBeaver 0.3 verification matrix](TEST_COVERAGE_0_3.md)
- [BusyBeaver 0.3 error code reference](ERROR_CODES.md)
- [BusyBeaver 0.2 → 0.3 migration guide](MIGRATION_0_2_TO_0_3.md)
