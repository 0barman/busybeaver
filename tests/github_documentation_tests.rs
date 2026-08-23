use std::fs;
use std::io;
use std::path::{Path, PathBuf};

#[test]
fn github_markdown_is_structurally_valid() -> Result<(), Box<dyn std::error::Error>> {
    let crate_root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut documents = vec![
        crate_root.join("README.md"),
        crate_root.join("CONTRIBUTING.md"),
        crate_root.join("CHANGELOG.md"),
    ];
    if let Some(repository_root) = crate_root.parent() {
        let github_contributing = repository_root.join(".github/CONTRIBUTING.md");
        if github_contributing.exists() {
            documents.push(github_contributing);
        }
    }
    collect_markdown(&crate_root.join("docs"), &mut documents)?;

    for document in documents {
        validate_document(&document)?;
    }
    Ok(())
}

#[test]
fn complete_guides_cover_every_public_feature_family() -> Result<(), Box<dyn std::error::Error>> {
    let crate_root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let guides = [
        crate_root.join("docs/GUIDE_en.md"),
        crate_root.join("docs/GUIDE_zh.md"),
    ];
    let required_families = [
        "TaskSpec",
        "TaskHandle",
        "TaskControlHandle",
        "TaskSelector",
        "spawn_child",
        "LaneConfig",
        "SpawnOptions",
        "OrderingKey",
        "RetryBuilder",
        "RecurringBuilder",
        "TaskSlot",
        "ScopeGeneration",
        "ServiceBuilder",
        "FixedCountBuilder",
        "TimeIntervalBuilder",
        "RangeIntervalBuilder",
        "PeriodicBuilder",
        "WorkListener",
        "ResourceLimits",
        "subscribe_events",
        "ExecutorSnapshot",
        "ShutdownOptions",
        "AbortPolicy",
        "cancel_and_wait",
        "wait_checked",
    ];

    for guide in guides {
        let source = fs::read_to_string(&guide)?;
        for family in required_families {
            if !source.contains(family) {
                return invalid(
                    &guide,
                    1,
                    &format!("complete guide is missing public feature family {family}"),
                );
            }
        }
    }
    Ok(())
}

fn collect_markdown(directory: &Path, documents: &mut Vec<PathBuf>) -> io::Result<()> {
    for entry in fs::read_dir(directory)? {
        let path = entry?.path();
        if path.is_dir() {
            collect_markdown(&path, documents)?;
        } else if path.extension().and_then(|extension| extension.to_str()) == Some("md") {
            documents.push(path);
        }
    }
    Ok(())
}

fn validate_document(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let source = fs::read_to_string(path)?;
    let mut h1_count = 0usize;
    let mut previous_heading = 0usize;
    let mut fence_language: Option<&str> = None;

    for (line_index, line) in source.lines().enumerate() {
        let line_number = line_index.saturating_add(1);
        if line.ends_with(' ') || line.ends_with('\t') {
            return invalid(path, line_number, "trailing whitespace");
        }

        if let Some(info) = line.strip_prefix("```") {
            if fence_language.is_some() {
                if !info.is_empty() {
                    return invalid(
                        path,
                        line_number,
                        "closing code fence contains an info string",
                    );
                }
                fence_language = None;
            } else {
                if info.is_empty() {
                    return invalid(path, line_number, "opening code fence has no language");
                }
                fence_language = Some(info);
            }
            continue;
        }

        if let Some(language) = fence_language {
            if language == "rust" && line.starts_with("# ") {
                return invalid(
                    path,
                    line_number,
                    "standalone GitHub Rust example contains a rustdoc-only hidden line",
                );
            }
            continue;
        }

        if let Some(level) = heading_level(line) {
            if level == 1 {
                h1_count = h1_count.saturating_add(1);
            }
            if previous_heading != 0 && level > previous_heading.saturating_add(1) {
                return invalid(path, line_number, "heading level is skipped");
            }
            previous_heading = level;
        }

        validate_relative_links(path, line_number, line)?;
    }

    if fence_language.is_some() {
        return invalid(path, source.lines().count(), "code fence is not closed");
    }
    if h1_count != 1 {
        return invalid(
            path,
            1,
            "document must contain exactly one level-one heading",
        );
    }
    Ok(())
}

fn heading_level(line: &str) -> Option<usize> {
    let hashes = line.bytes().take_while(|byte| *byte == b'#').count();
    (hashes > 0 && hashes <= 6 && line.as_bytes().get(hashes) == Some(&b' ')).then_some(hashes)
}

fn validate_relative_links(
    document: &Path,
    line_number: usize,
    line: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut remainder = line;
    while let Some(start) = remainder.find("](") {
        remainder = &remainder[start.saturating_add(2)..];
        let Some(end) = remainder.find(')') else {
            return invalid(
                document,
                line_number,
                "Markdown link destination is not closed",
            );
        };
        let destination = &remainder[..end];
        remainder = &remainder[end.saturating_add(1)..];
        if destination.is_empty()
            || destination.starts_with('#')
            || destination.starts_with("https://")
            || destination.starts_with("http://")
            || destination.starts_with("mailto:")
        {
            continue;
        }
        let relative = destination.split('#').next().unwrap_or_default();
        let Some(parent) = document.parent() else {
            return invalid(document, line_number, "document has no parent directory");
        };
        if !parent.join(relative).exists() {
            return invalid(
                document,
                line_number,
                "relative Markdown link target does not exist",
            );
        }
    }
    Ok(())
}

fn invalid<T>(path: &Path, line: usize, message: &str) -> Result<T, Box<dyn std::error::Error>> {
    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        format!("{}:{line}: {message}", path.display()),
    )
    .into())
}
