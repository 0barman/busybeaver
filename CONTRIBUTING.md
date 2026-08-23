# Contributing to BusyBeaver

Thank you for improving BusyBeaver. Contributions should preserve its typed error model, bounded
resource behavior, cancellation safety, privacy guarantees, and panic-free production policy.

Before opening a pull request:

1. Read the contributor section of the [English developer guide](docs/GUIDE_en.md).
2. Add deterministic tests for changed behavior.
3. Update rustdoc and repository documentation for public API changes.
4. Run formatting, strict Clippy, all-feature tests, no-default-feature tests, and doctests.
5. Confirm that no correctness test or rustdoc example is ignored.

Bug reports should include BusyBeaver, Rust, and Tokio versions; the target OS; runtime flavor;
enabled features; any `BB-*` error or diagnostic code; and a minimal reproduction without secrets
or business data.

By contributing, you agree that your contribution may be distributed under the project's dual
MIT OR Apache-2.0 license.
