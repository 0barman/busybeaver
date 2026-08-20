//! Compile-time coverage for Rust examples published in standalone GitHub Markdown.
//!
//! The items in this module exist only during `cargo test --doc`. Keeping the
//! Markdown as the direct `include_str!` source prevents the GitHub examples
//! and their compiled verification from drifting apart.

/// GitHub and crates.io landing page examples.
#[doc = include_str!("../README.md")]
pub struct ReadmeExamples;

/// English integration guide examples.
#[doc = include_str!("../docs/INTEGRATION_en.md")]
pub struct EnglishIntegrationExamples;

/// Chinese integration guide examples.
#[doc = include_str!("../docs/INTEGRATION_zh.md")]
pub struct ChineseIntegrationExamples;

/// 0.2 to 0.3 migration examples.
#[doc = include_str!("../docs/MIGRATION_0_2_TO_0_3.md")]
pub struct MigrationExamples;

/// Public error handling examples.
#[doc = include_str!("../docs/ERROR_CODES.md")]
pub struct ErrorCodeExamples;
