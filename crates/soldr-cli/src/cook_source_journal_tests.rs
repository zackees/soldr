//! zackees/soldr#3518: a cook killed between cargo-chef's in-place skeleton
//! reconstruction and the restore must not leave (or re-snapshot) a
//! truncated tree.

use super::*;

const MANIFEST: &str = "[package]\nname = \"ban_raw_process_creation\"\nversion = \"0.4.2\"\n";
const LIB_RS: &str = "pub fn dylint_version() -> &'static str {\n    \"0.1.0\"\n}\n";

/// A one-crate workspace inside a git checkout (journal under `.git/`).
fn workspace(git: bool) -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    std::fs::create_dir_all(root.join("src")).unwrap();
    if git {
        std::fs::create_dir_all(root.join(".git")).unwrap();
    }
    std::fs::write(root.join("Cargo.toml"), MANIFEST).unwrap();
    std::fs::write(root.join("Cargo.lock"), "# lock\n").unwrap();
    std::fs::write(root.join("src/lib.rs"), LIB_RS).unwrap();
    tmp
}

/// What cargo-chef's `cook` does to the checkout in place.
fn skeletonize(root: &Path) {
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"ban_raw_process_creation\"\nversion = \"0.0.1\"\n",
    )
    .unwrap();
    std::fs::write(root.join("src/lib.rs"), "").unwrap();
    std::fs::write(root.join("src/main.rs"), "fn main() {}").unwrap();
}

fn assert_pristine(root: &Path) {
    assert_eq!(
        std::fs::read_to_string(root.join("Cargo.toml")).unwrap(),
        MANIFEST
    );
    assert_eq!(
        std::fs::read_to_string(root.join("src/lib.rs")).unwrap(),
        LIB_RS
    );
    assert!(!root.join("src/main.rs").exists());
}

fn journal_of(root: &Path) -> PathBuf {
    let root = canonical(root);
    journal_paths(&journal_dir_for(&root), &root).0
}

#[test]
fn journal_is_durable_before_mutation_and_retired_after_restore() {
    let tmp = workspace(true);
    let root = tmp.path();
    let guard = CookSourceGuard::begin(root).unwrap();
    let journal = journal_of(root);
    assert!(journal.starts_with(canonical(&root.join(".git"))));
    let (_, journaled) = decode(&std::fs::read(&journal).unwrap()).unwrap();
    assert_eq!(journaled.len(), 3);
    skeletonize(root);
    guard.restore().unwrap();
    assert_pristine(root);
    assert!(!journal.exists());
}

#[test]
fn killed_cook_is_restored_by_the_next_invocation() {
    let tmp = workspace(true);
    let root = tmp.path();
    let guard = CookSourceGuard::begin(root).unwrap();
    skeletonize(root);
    guard.abandon_for_test(); // SIGKILL between mutate and restore
    assert_eq!(std::fs::read(root.join("src/lib.rs")).unwrap().len(), 0);

    // Any later soldr invocation (front door / cook entry) recovers first.
    assert_eq!(recover_stale_cook_journals(root).unwrap(), 1);
    assert_pristine(root);
    assert!(!journal_of(root).exists());
    // Idempotent: nothing left to recover.
    assert_eq!(recover_stale_cook_journals(root).unwrap(), 0);
}

#[test]
fn recovery_is_found_from_a_parent_directory() {
    // ci-test cooks `dylints/<lib>` while a later build runs at the repo root.
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join(".git")).unwrap();
    let nested = tmp.path().join("dylints/ban_raw_process_creation");
    std::fs::create_dir_all(nested.join("src")).unwrap();
    std::fs::write(nested.join("Cargo.toml"), MANIFEST).unwrap();
    std::fs::write(nested.join("src/lib.rs"), LIB_RS).unwrap();
    let guard = CookSourceGuard::begin(&nested).unwrap();
    skeletonize(&nested);
    guard.abandon_for_test();

    assert_eq!(recover_stale_cook_journals(tmp.path()).unwrap(), 1);
    assert_pristine(&nested);
}

#[test]
fn next_cook_never_resnapshots_the_broken_tree() {
    let tmp = workspace(true);
    let root = tmp.path();
    let guard = CookSourceGuard::begin(root).unwrap();
    skeletonize(root);
    guard.abandon_for_test();

    // The next cook goes straight to `begin` (no separate recovery call).
    let next = CookSourceGuard::begin(root).unwrap();
    assert_pristine(root);
    let lib = next
        .snapshot
        .files
        .iter()
        .find(|(rel, _)| rel == Path::new("src/lib.rs"))
        .unwrap();
    assert_eq!(lib.1, LIB_RS.as_bytes(), "re-snapshotted the skeleton");
    skeletonize(root);
    next.restore().unwrap();
    assert_pristine(root);
}

#[test]
fn live_cook_journal_is_not_recovered_under_it() {
    let tmp = workspace(true);
    let root = tmp.path();
    let guard = CookSourceGuard::begin(root).unwrap();
    skeletonize(root);
    // A concurrent `soldr cargo ...` (or chef's own front-door calls in this
    // process) must not restore while the cook is still compiling.
    assert_eq!(recover_stale_cook_journals(root).unwrap(), 0);
    assert_eq!(std::fs::read(root.join("src/lib.rs")).unwrap().len(), 0);
    guard.restore().unwrap();
    assert_pristine(root);
}

#[test]
fn early_return_restores_via_drop() {
    let tmp = workspace(false);
    let root = tmp.path();
    {
        let _guard = CookSourceGuard::begin(root).unwrap();
        skeletonize(root);
    }
    assert_pristine(root);
    assert!(!journal_of(root).exists());
}

#[test]
fn edits_made_after_the_crash_are_backed_up_not_lost() {
    let tmp = workspace(true);
    let root = tmp.path();
    let guard = CookSourceGuard::begin(root).unwrap();
    skeletonize(root);
    guard.abandon_for_test();
    std::fs::write(root.join("src/new_module.rs"), "pub fn mine() {}\n").unwrap();

    recover_stale_cook_journals(root).unwrap();
    assert_pristine(root);
    let dir = journal_dir_for(&canonical(root));
    let backup = std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .find(|p| {
            p.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("recovered-")
        })
        .expect("backup dir");
    assert_eq!(
        std::fs::read_to_string(backup.join("src/new_module.rs")).unwrap(),
        "pub fn mine() {}\n"
    );
    assert!(backup.join("src/lib.rs").exists());
}

#[test]
fn corrupt_journal_refuses_with_recovery_instructions() {
    let tmp = workspace(true);
    let root = tmp.path();
    let guard = CookSourceGuard::begin(root).unwrap();
    skeletonize(root);
    guard.abandon_for_test();
    let journal = journal_of(root);
    let mut bytes = std::fs::read(&journal).unwrap();
    let mid = bytes.len() / 2;
    bytes[mid] ^= 0xff;
    std::fs::write(&journal, bytes).unwrap();

    let error = recover_stale_cook_journals(root).unwrap_err().to_string();
    assert!(error.contains("refusing to continue"), "{error}");
    assert!(error.contains("git checkout"), "{error}");
    // And the next cook refuses too, instead of snapshotting the skeleton.
    assert!(CookSourceGuard::begin(root).is_err());
    assert!(journal.exists());
}

#[test]
fn journal_codec_round_trips_and_rejects_tampering() {
    let snapshot = ProjectSourceSnapshot {
        files: vec![
            (PathBuf::from("src/lib.rs"), LIB_RS.as_bytes().to_vec()),
            (PathBuf::from("Cargo.toml"), Vec::new()),
        ],
    };
    let bytes = encode(Path::new("/work/tree"), &snapshot);
    let (root, decoded) = decode(&bytes).unwrap();
    assert_eq!(root, PathBuf::from("/work/tree"));
    assert_eq!(decoded.files, snapshot.files);
    assert!(decode(&bytes[..bytes.len() - 1]).is_err());
    let escaping = ProjectSourceSnapshot {
        files: vec![(PathBuf::from("../outside.rs"), Vec::new())],
    };
    assert!(decode(&encode(Path::new("/w"), &escaping)).is_err());
}
