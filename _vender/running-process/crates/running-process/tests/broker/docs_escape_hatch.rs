#![cfg(feature = "client")]

use std::path::{Path, PathBuf};

#[test]
fn broker_docs_use_canonical_disable_escape_hatch() {
    let crate_root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let repo_root = crate_root.join("..").join("..");
    let mut files = Vec::new();
    collect_markdown_files(&repo_root.join("docs"), &mut files);

    for readme in [repo_root.join("README.md"), crate_root.join("README.md")] {
        if readme.exists() {
            files.push(readme);
        }
    }

    let mut stale = Vec::new();
    let mut documents_canonical_disable = false;
    for path in files {
        let text = std::fs::read_to_string(&path).unwrap();
        if text.contains("RUNNING_PROCESS_USE_BROKER") {
            stale.push(path);
        }
        if text.contains("RUNNING_PROCESS_DISABLE=1") {
            documents_canonical_disable = true;
        }
    }

    assert!(
        stale.is_empty(),
        "broker docs must use RUNNING_PROCESS_DISABLE=1, not RUNNING_PROCESS_USE_BROKER:\n{}",
        stale
            .iter()
            .map(|path| display_path(&repo_root, path))
            .collect::<Vec<_>>()
            .join("\n")
    );
    assert!(
        documents_canonical_disable,
        "broker docs must document RUNNING_PROCESS_DISABLE=1"
    );
}

fn collect_markdown_files(dir: &Path, files: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            collect_markdown_files(&path, files);
        } else if path.extension().and_then(|ext| ext.to_str()) == Some("md") {
            files.push(path);
        }
    }
}

fn display_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}
