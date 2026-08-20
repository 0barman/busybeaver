use std::fs;
use std::io;
use std::path::{Path, PathBuf};

const FORBIDDEN: &[&str] = &[
    ".unwrap(",
    ".expect(",
    ".expect_err(",
    ".unwrap_err(",
    "todo!(",
    "panic!(",
    "assert!(",
    "assert_eq!(",
    "debug_assert!(",
    "unimplemented!(",
    "unreachable!(",
    "RefCell",
    ".borrow_mut(",
    "unsafe {",
    "std::process::abort(",
];

#[test]
fn production_sources_contain_no_forbidden_panic_or_unchecked_borrow_paths(
) -> Result<(), Box<dyn std::error::Error>> {
    let source_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    collect_rust_files(&source_root, &mut files)?;
    let mut violations = Vec::new();

    for path in files {
        if path.file_name().and_then(|name| name.to_str()) == Some("tests.rs") {
            continue;
        }
        let source = fs::read_to_string(&path)?;
        for (index, line) in source.lines().enumerate() {
            let trimmed = line.trim_start();
            if trimmed.starts_with("//") {
                continue;
            }
            for forbidden in FORBIDDEN {
                if line.contains(forbidden) {
                    violations.push(format!(
                        "{}:{} contains {forbidden}",
                        path.display(),
                        index.saturating_add(1)
                    ));
                }
            }
        }
    }

    if violations.is_empty() {
        Ok(())
    } else {
        Err(io::Error::new(io::ErrorKind::InvalidData, violations.join("\n")).into())
    }
}

fn collect_rust_files(directory: &Path, files: &mut Vec<PathBuf>) -> io::Result<()> {
    for entry in fs::read_dir(directory)? {
        let path = entry?.path();
        if path.is_dir() {
            collect_rust_files(&path, files)?;
        } else if path.extension().and_then(|extension| extension.to_str()) == Some("rs") {
            files.push(path);
        }
    }
    Ok(())
}
