//! Compile-time coverage for Rust examples published in standalone GitHub Markdown.
//!
//! The items in this module exist only during `cargo test --doc`. Keeping the
//! Markdown as the direct `include_str!` source prevents the GitHub examples
//! and their compiled verification from drifting apart.

/// GitHub and crates.io landing page examples.
#[doc = include_str!("../README.md")]
pub struct ReadmeExamples;

/// Complete English developer guide examples.
#[doc = include_str!("../docs/GUIDE_en.md")]
pub struct EnglishGuideExamples;

/// Complete Chinese developer guide examples.
#[doc = include_str!("../docs/GUIDE_zh.md")]
pub struct ChineseGuideExamples;
