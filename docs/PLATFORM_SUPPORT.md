# Platform and panic support

- MSRV: Rust 1.88.
- Supported: native `std` targets on Windows, Linux, and macOS with a Tokio
  current-thread or multi-thread runtime.
- Cross-runtime submission is supported; execution stays on the runtime bound
  when Scheduler or Beaver is constructed.
- WebAssembly is not supported in 0.3. A `compile_error!` provides an explicit
  diagnostic instead of leaving support to incidental dependency failures.
- Panic isolation is effective only with Rust's `panic=unwind` strategy.
  `panic=abort` builds are supported for compilation, but a panic terminates the
  process and cannot become `TaskTerminal::Panicked` or a legacy listener error.
- User futures must yield cooperatively. Blocking or non-yielding work can delay
  graceful cancellation; confirmed Tokio abort cannot stop an already-running
  `spawn_blocking` closure.
