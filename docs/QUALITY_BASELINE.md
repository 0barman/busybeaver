# BusyBeaver 0.2.0 quality baseline

Recorded on 2026-08-08 before the execution-core changes.

## Toolchain and commands

- Active stable toolchain: Rust/Cargo 1.95.0.
- Locally available older toolchain: 1.89.0; the declared MSRV 1.88.0 is not installed yet.
- `cargo fmt --manifest-path busybeaver/Cargo.toml -- --check`: passed.
- `cargo test --manifest-path busybeaver/Cargo.toml --all-features`: 264 passed, 2 ignored correctness tests, 7 ignored documentation examples.
- `cargo clippy --manifest-path busybeaver/Cargo.toml --all-targets --all-features -- -D warnings`: failed on 8 pre-existing lints (six documentation indent findings, one public return type complexity finding, and one range-loop finding). The baseline infrastructure commit removes these without changing behavior.
- `cargo llvm-cov`, `cargo semver-checks`, and `cargo mutants`: not installed; release gates remain unsatisfied until installed and run.

The repository root is not a Cargo workspace. A root-level bare Cargo command is
not accepted as evidence for this crate.

## Known test debt

- Listener panic isolation is ignored in `tests/improvement_tests.rs`.
- Progress callback panic isolation is ignored in the same file.
- Seven rustdoc examples use `ignore` and therefore do not compile as doctests.
- The former broad `#[should_panic]` zero-buffer test is converted to a scoped
  `catch_unwind`, so runtime-construction failures cannot satisfy it accidentally.

## Performance baseline

`benches/legacy_baseline.rs` fixes two dependency-free, repeatable workloads:

- fixed-count task construction;
- one-task enqueue followed by deterministic Beaver shutdown.

Benchmark output is machine-specific and is not committed as a universal target.
Before and after comparisons must run on the same host with the same profile and
must treat correctness as a hard gate before interpreting throughput.

First same-host release-profile sample after fixing the harness:

- fixed-count build: 10.1452 ms / 100,000 iterations;
- enqueue and shutdown: 16.0896 ms / 1,000 iterations.

After the non-behavior cleanup, the strict all-target Clippy command and the full
all-features test command both pass. The two callback-isolation tests were then
enabled by their RED/GREEN implementation change and now run in every full suite.

## 0.3 post-implementation comparison

The benchmark harness now also measures a 10,000-task completed legacy burst
and the equivalent typed Scheduler burst. A detached worktree at commit
`1e50241` supplied the same-host pre-adapter comparison; the worktree was
removed after measurement.

Representative release-profile samples on 2026-08-08:

| Workload | Pre-adapter | 0.3 post-adapter |
|---|---:|---:|
| fixed-count build, 100,000 | 30.3–30.9 ms | 30.4–31.2 ms |
| create/enqueue/destroy, 1,000 | 20.4–20.8 ms | 31.6–46.9 ms |
| completed legacy burst, 10,000 | 12.7 ms | 65.4–66.2 ms |
| completed Scheduler burst, 10,000 | 53.6 ms | 47.3–47.6 ms |

The legacy-to-Scheduler adapter adds about 5.3 microseconds per empty task in
the burst test. The relative change is large because the payload does no work;
the absolute cost and the reason for it are both recorded rather than hidden.
The 0.3 gate is: no regression in task construction, no correctness/cancellation
regression, and less than 10 microseconds additional dispatch cost per empty
legacy task on this host. The sample passes that absolute dispatch gate.

The 2026-08-09 coded-error/`.expect()` hardening was measured again on the same
host: fixed-count build 31.1035 ms/100,000, create-enqueue-destroy 39.8373
ms/1,000, legacy burst 59.8854 ms/10,000, and Scheduler burst 49.1019
ms/10,000. It remains inside the recorded construction range and absolute
legacy dispatch threshold.

This is not a universal performance promise. Release comparisons must use the
same host/profile and stop if the absolute threshold is exceeded or any
correctness test regresses.
