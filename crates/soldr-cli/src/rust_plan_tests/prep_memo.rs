//! Tests for the rust-plan prep memo (soldr#1540): every semantic mutation
//! class from the issue must invalidate the memo, and unknown/corrupt state
//! must fall back to the authoritative subprocesses.
//!
//! Mutation classes covered: workspace membership globs, path dependencies
//! (in- and out-of-workspace), Cargo.lock, `.cargo/config*`, explicit
//! `--config`/feature args, toolchain binary identity (path + bytes),
//! rust-toolchain pin files, parent-workspace manifest injection, and the
//! captured environment. Plus: corrupt-file / schema-version fallback and a
//! unix end-to-end proof that a memo hit skips the `cargo metadata`,
//! `rustc -Vv`, and `cargo --version` subprocesses until an input changes.

use crate::rust_plan::rust_plan_memo::{
    wire, MemoContext, MemoEnvSnapshot, PREP_MEMO_SCHEMA_VERSION, RUST_PLAN_MEMO_ENV_VAR,
};
use crate::rust_plan::{CargoMetadata, CargoMetadataPackage, ToolchainProbe, WorkspaceFileHashes};
use prost::Message as _;
use std::path::PathBuf;

struct MemoFixture {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    ext_manifest: PathBuf,
    cargo_bin: PathBuf,
    rustc_bin: PathBuf,
    plan_dir: PathBuf,
    args: Vec<String>,
}

impl MemoFixture {
    fn new() -> Self {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let base = tmp.path().to_path_buf();
        let root = base.join("ws");
        std::fs::create_dir_all(root.join("app/src")).unwrap();
        std::fs::create_dir_all(root.join(".cargo")).unwrap();
        std::fs::write(root.join("Cargo.toml"), "[workspace]\nmembers=[\"app\"]\n").unwrap();
        std::fs::write(
            root.join("app/Cargo.toml"),
            "[package]\nname=\"app\"\nversion=\"0.1.0\"\n",
        )
        .unwrap();
        std::fs::write(root.join("Cargo.lock"), "# lock v1\n").unwrap();
        std::fs::write(root.join(".cargo/config.toml"), "[build]\njobs = 2\n").unwrap();

        let ext_manifest = base.join("ext/dep/Cargo.toml");
        std::fs::create_dir_all(ext_manifest.parent().unwrap()).unwrap();
        std::fs::write(
            &ext_manifest,
            "[package]\nname=\"dep\"\nversion=\"0.1.0\"\n",
        )
        .unwrap();

        let bin_dir = base.join("bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        let cargo_bin = bin_dir.join("cargo-fake");
        let rustc_bin = bin_dir.join("rustc-fake");
        std::fs::write(&cargo_bin, "cargo binary v1").unwrap();
        std::fs::write(&rustc_bin, "rustc binary v1").unwrap();

        let plan_dir = base.join("plans");

        Self {
            _tmp: tmp,
            root,
            ext_manifest,
            cargo_bin,
            rustc_bin,
            plan_dir,
            args: vec!["build".to_string()],
        }
    }

    fn env(&self) -> MemoEnvSnapshot {
        MemoEnvSnapshot {
            cwd: self.root.display().to_string(),
            cargo_home: None,
            cargo_target_dir: None,
            cargo_build_target_dir: None,
            rustup_home: None,
            rustup_toolchain: None,
        }
    }

    fn context(&self) -> MemoContext {
        self.context_with(self.env(), &self.args)
    }

    fn context_with(&self, env: MemoEnvSnapshot, args: &[String]) -> MemoContext {
        MemoContext::gather_with_env(env, &self.cargo_bin, &self.rustc_bin, args)
            .expect("gather memo context")
    }

    fn metadata(&self) -> CargoMetadata {
        CargoMetadata {
            workspace_root: self.root.clone(),
            target_directory: self.root.join("target"),
            workspace_members: vec!["path+file:///ws/app#app@0.1.0".to_string()],
            packages: vec![
                CargoMetadataPackage {
                    id: "path+file:///ws/app#app@0.1.0".to_string(),
                    source: None,
                    manifest_path: Some(self.root.join("app/Cargo.toml").display().to_string()),
                },
                CargoMetadataPackage {
                    id: "registry+https://github.com/rust-lang/crates.io-index#serde@1.0.0"
                        .to_string(),
                    source: Some("registry+https://github.com/rust-lang/crates.io-index".into()),
                    manifest_path: Some("/registry/serde-1.0.0/Cargo.toml".to_string()),
                },
                CargoMetadataPackage {
                    id: "path+file:///ext/dep#dep@0.1.0".to_string(),
                    source: None,
                    manifest_path: Some(self.ext_manifest.display().to_string()),
                },
            ],
        }
    }

    fn probe() -> ToolchainProbe {
        ToolchainProbe {
            rustc_verbose: "rustc 1.94.1-fake\nhost: x86_64-unknown-fake\nrelease: 1.94.1\n"
                .to_string(),
            cargo_version: "cargo 1.94.1-fake\n".to_string(),
        }
    }

    /// Store a memo through a fresh context and verify it immediately
    /// round-trips as a hit.
    fn store_and_verify_hit(&self) {
        let context = self.context();
        let hashes = WorkspaceFileHashes::collect(&self.root).expect("collect hashes");
        context
            .store(
                &self.plan_dir,
                &self.metadata(),
                &Self::probe(),
                &self.root.display().to_string(),
                &hashes,
            )
            .expect("store memo");
        assert!(
            self.context().try_load(&self.plan_dir).is_some(),
            "freshly stored memo must load as a hit"
        );
    }

    /// After `store_and_verify_hit`, apply `mutate` and assert the memo no
    /// longer loads (i.e. the caller would run the authoritative
    /// subprocesses).
    fn assert_invalidated_by(&self, mutate: impl FnOnce(&Self)) {
        self.store_and_verify_hit();
        mutate(self);
        assert!(
            self.context().try_load(&self.plan_dir).is_none(),
            "mutation must invalidate the prep memo"
        );
    }
}

#[test]
fn prep_memo_roundtrips_metadata_and_probe_on_hit() {
    let fixture = MemoFixture::new();
    fixture.store_and_verify_hit();

    let (metadata, probe, hashes) = fixture
        .context()
        .try_load(&fixture.plan_dir)
        .expect("memo hit");
    assert_eq!(metadata.workspace_root, fixture.root);
    assert_eq!(metadata.target_directory, fixture.root.join("target"));
    assert_eq!(
        metadata.workspace_members,
        vec!["path+file:///ws/app#app@0.1.0".to_string()]
    );
    assert_eq!(metadata.packages.len(), 3);
    assert_eq!(
        metadata.packages[1].source.as_deref(),
        Some("registry+https://github.com/rust-lang/crates.io-index")
    );
    assert_eq!(metadata.packages[0].source, None);
    assert!(probe.rustc_verbose.contains("host: x86_64-unknown-fake"));
    assert!(probe.cargo_version.starts_with("cargo 1.94.1-fake"));
    // The hit also hands back the exact plan-input hash families so no
    // second manifest walk is needed.
    let fresh = WorkspaceFileHashes::collect(&fixture.root).expect("collect");
    assert_eq!(hashes.lockfile_hash, fresh.lockfile_hash);
    assert_eq!(hashes.cargo_config_hash, fresh.cargo_config_hash);
    assert_eq!(hashes.manifest_hashes, fresh.manifest_hashes);
}

#[test]
fn prep_memo_invalidated_by_workspace_membership_change() {
    let fixture = MemoFixture::new();
    fixture.assert_invalidated_by(|f| {
        std::fs::write(
            f.root.join("Cargo.toml"),
            "[workspace]\nmembers=[\"app\",\"crates/*\"]\n",
        )
        .unwrap();
    });
}

#[test]
fn prep_memo_invalidated_by_new_member_manifest() {
    let fixture = MemoFixture::new();
    fixture.assert_invalidated_by(|f| {
        std::fs::create_dir_all(f.root.join("newcrate")).unwrap();
        std::fs::write(
            f.root.join("newcrate/Cargo.toml"),
            "[package]\nname=\"newcrate\"\nversion=\"0.1.0\"\n",
        )
        .unwrap();
    });
}

#[test]
fn prep_memo_invalidated_by_in_workspace_path_dep_edit() {
    let fixture = MemoFixture::new();
    fixture.assert_invalidated_by(|f| {
        std::fs::write(
            f.root.join("app/Cargo.toml"),
            "[package]\nname=\"app\"\nversion=\"0.1.0\"\n[dependencies]\ndep={path=\"../../ext/dep\"}\n",
        )
        .unwrap();
    });
}

#[test]
fn prep_memo_invalidated_by_external_path_dep_manifest_edit() {
    let fixture = MemoFixture::new();
    fixture.assert_invalidated_by(|f| {
        std::fs::write(
            &f.ext_manifest,
            "[package]\nname=\"dep\"\nversion=\"0.2.0\"\n",
        )
        .unwrap();
    });
}

#[test]
fn prep_memo_invalidated_by_lockfile_change() {
    let fixture = MemoFixture::new();
    fixture.assert_invalidated_by(|f| {
        std::fs::write(f.root.join("Cargo.lock"), "# lock v2\n").unwrap();
    });
}

#[test]
fn prep_memo_invalidated_by_cargo_config_change() {
    let fixture = MemoFixture::new();
    fixture.assert_invalidated_by(|f| {
        std::fs::write(f.root.join(".cargo/config.toml"), "[build]\njobs = 8\n").unwrap();
    });
}

#[test]
fn prep_memo_invalidated_by_explicit_config_arg() {
    let fixture = MemoFixture::new();
    fixture.store_and_verify_hit();
    let args = vec![
        "build".to_string(),
        "--config".to_string(),
        "build.jobs=4".to_string(),
    ];
    let context = fixture.context_with(fixture.env(), &args);
    assert!(
        context.try_load(&fixture.plan_dir).is_none(),
        "an explicit --config passthrough must not reuse the plain memo"
    );
}

#[test]
fn prep_memo_invalidated_by_feature_selection_args() {
    let fixture = MemoFixture::new();
    fixture.store_and_verify_hit();
    let args = vec!["build".to_string(), "--all-features".to_string()];
    let context = fixture.context_with(fixture.env(), &args);
    assert!(
        context.try_load(&fixture.plan_dir).is_none(),
        "feature-selection passthrough args must not reuse the plain memo"
    );
}

#[test]
fn prep_memo_invalidated_by_toolchain_binary_content_change() {
    let fixture = MemoFixture::new();
    fixture.assert_invalidated_by(|f| {
        std::fs::write(&f.cargo_bin, "cargo binary v2 - definitely different").unwrap();
    });
}

#[test]
fn prep_memo_invalidated_by_toolchain_binary_path_change() {
    let fixture = MemoFixture::new();
    fixture.store_and_verify_hit();
    let other_rustc = fixture.rustc_bin.with_file_name("rustc-other");
    std::fs::write(&other_rustc, "rustc binary v1").unwrap();
    let context = MemoContext::gather_with_env(
        fixture.env(),
        &fixture.cargo_bin,
        &other_rustc,
        &fixture.args,
    )
    .expect("gather");
    assert!(
        context.try_load(&fixture.plan_dir).is_none(),
        "a different resolved rustc path must invalidate the memo"
    );
}

#[test]
fn prep_memo_invalidated_by_env_change() {
    let fixture = MemoFixture::new();
    fixture.store_and_verify_hit();
    let mutations: [fn(&mut MemoEnvSnapshot); 3] = [
        |env| env.rustup_toolchain = Some("nightly".to_string()),
        |env| env.cargo_target_dir = Some("/elsewhere".to_string()),
        |env| env.cargo_home = Some("/other-cargo-home".to_string()),
    ];
    for mutate in mutations {
        let mut env = fixture.env();
        mutate(&mut env);
        let context = fixture.context_with(env, &fixture.args);
        assert!(
            context.try_load(&fixture.plan_dir).is_none(),
            "environment mutations must invalidate the memo"
        );
    }
}

#[test]
fn prep_memo_invalidated_by_rust_toolchain_pin_change() {
    let fixture = MemoFixture::new();
    fixture.assert_invalidated_by(|f| {
        std::fs::write(
            f.root.join("rust-toolchain.toml"),
            "[toolchain]\nchannel=\"1.95.0\"\n",
        )
        .unwrap();
    });
}

#[test]
fn prep_memo_invalidated_by_parent_workspace_injection() {
    let fixture = MemoFixture::new();
    fixture.assert_invalidated_by(|f| {
        // A new Cargo.toml in a strict ancestor of the discovered manifest
        // can change the true workspace root without touching anything
        // under the old root.
        std::fs::write(
            f.root.parent().unwrap().join("Cargo.toml"),
            "[workspace]\nmembers=[\"ws\"]\n",
        )
        .unwrap();
    });
}

#[test]
fn prep_memo_rejects_corrupt_file() {
    let fixture = MemoFixture::new();
    fixture.store_and_verify_hit();
    let context = fixture.context();
    std::fs::write(
        context.memo_path(&fixture.plan_dir),
        b"not a protobuf at all",
    )
    .unwrap();
    assert!(
        context.try_load(&fixture.plan_dir).is_none(),
        "a corrupt memo must fall back to the authoritative subprocesses"
    );
}

#[test]
fn prep_memo_rejects_unknown_schema_version() {
    let fixture = MemoFixture::new();
    fixture.store_and_verify_hit();
    let context = fixture.context();
    let path = context.memo_path(&fixture.plan_dir);
    let bytes = std::fs::read(&path).unwrap();
    let mut memo = wire::RustPlanPrepMemoV1::decode(bytes.as_slice()).unwrap();
    assert_eq!(memo.schema_version, PREP_MEMO_SCHEMA_VERSION);
    memo.schema_version = PREP_MEMO_SCHEMA_VERSION + 1;
    let mut raised = Vec::with_capacity(memo.encoded_len());
    memo.encode(&mut raised).unwrap();
    std::fs::write(&path, raised).unwrap();
    assert!(
        context.try_load(&fixture.plan_dir).is_none(),
        "an unknown schema version must fall back to the authoritative subprocesses"
    );
}

// Schema drift guard for `rust_plan_memo.proto`: encode/decode round trip
// preserves every field.
#[test]
fn prep_memo_wire_roundtrip_preserves_fields() {
    let memo = wire::RustPlanPrepMemoV1 {
        schema_version: PREP_MEMO_SCHEMA_VERSION,
        key_hash: "abc123".to_string(),
        rustc_verbose_version: "rustc 1.94.1\nhost: x\n".to_string(),
        cargo_version_line: "cargo 1.94.1\n".to_string(),
        workspace_root: "/ws".to_string(),
        target_directory: "/ws/target".to_string(),
        workspace_members: vec!["member-a".to_string(), "member-b".to_string()],
        packages: vec![
            wire::PrepMemoPackage {
                id: "pkg-a".to_string(),
                source: String::new(),
                has_source: false,
                manifest_path: "/ws/a/Cargo.toml".to_string(),
            },
            wire::PrepMemoPackage {
                id: "pkg-b".to_string(),
                source: "registry+https://example".to_string(),
                has_source: true,
                manifest_path: "/registry/b/Cargo.toml".to_string(),
            },
        ],
    };
    let mut bytes = Vec::with_capacity(memo.encoded_len());
    memo.encode(&mut bytes).unwrap();
    let decoded = wire::RustPlanPrepMemoV1::decode(bytes.as_slice()).unwrap();
    assert_eq!(decoded, memo);
}

/// End-to-end (unix-gated at runtime): a memo hit must skip all three
/// prep subprocesses; mutating a workspace manifest must re-run them.
/// The fake `cargo`/`rustc` are `#!/bin/sh` scripts, so the test
/// self-skips on Windows where they cannot execute.
mod end_to_end {
    use super::*;
    use crate::build_cache_session::BuildCacheSession;
    use crate::rust_plan::maybe_prepare_rust_artifact_plan;
    use crate::TARGET_CACHE_MODE_ENV_VAR;
    use std::path::Path;
    use std::sync::Mutex;

    /// Serialises the env mutation of `SOLDR_TARGET_CACHE_MODE` /
    /// `SOLDR_RUST_PLAN_MEMO` within this test binary.
    static MODE_ENV_LOCK: Mutex<()> = Mutex::new(());

    fn write_script(path: &Path, body: &str) {
        std::fs::write(path, body).unwrap();
        let source = std::fs::metadata(path).unwrap().permissions();
        crate::platform::fs::permissions::make_executable_from(path, &source).unwrap();
    }

    fn call_count(path: &Path) -> usize {
        std::fs::read_to_string(path)
            .map(|body| body.lines().count())
            .unwrap_or(0)
    }

    #[test]
    fn prep_memo_hit_skips_prep_subprocesses_until_manifest_changes() {
        if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows {
            return;
        }
        let _lock = MODE_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let previous_mode = std::env::var_os(TARGET_CACHE_MODE_ENV_VAR);
        let previous_memo = std::env::var_os(RUST_PLAN_MEMO_ENV_VAR);
        std::env::set_var(TARGET_CACHE_MODE_ENV_VAR, "thin");
        std::env::remove_var(RUST_PLAN_MEMO_ENV_VAR);

        let result = std::panic::catch_unwind(|| {
            let tmp = tempfile::tempdir().expect("tmpdir");
            let base = tmp.path();
            let ws = base.join("ws");
            std::fs::create_dir_all(ws.join("app/src")).unwrap();
            std::fs::write(ws.join("Cargo.toml"), "[workspace]\nmembers=[\"app\"]\n").unwrap();
            std::fs::write(
                ws.join("app/Cargo.toml"),
                "[package]\nname=\"app\"\nversion=\"0.1.0\"\n",
            )
            .unwrap();
            std::fs::write(ws.join("Cargo.lock"), "# lock\n").unwrap();

            let metadata_json = serde_json::json!({
                "packages": [{
                    "id": "path+file:///ws/app#app@0.1.0",
                    "source": null,
                    "manifest_path": ws.join("app/Cargo.toml").display().to_string(),
                }],
                "workspace_members": ["path+file:///ws/app#app@0.1.0"],
                "workspace_root": ws.display().to_string(),
                "target_directory": ws.join("target").display().to_string(),
            });
            let metadata_file = base.join("metadata.json");
            std::fs::write(&metadata_file, serde_json::to_vec(&metadata_json).unwrap()).unwrap();

            let cargo_calls = base.join("cargo-calls.log");
            let rustc_calls = base.join("rustc-calls.log");
            let cargo_bin = base.join("cargo-fake");
            let rustc_bin = base.join("rustc-fake");
            write_script(
                &cargo_bin,
                &format!(
                    "#!/bin/sh\necho \"$@\" >> {}\nif [ \"$1\" = metadata ]; then cat {}; else echo 'cargo 1.94.1-fake'; fi\n",
                    cargo_calls.display(),
                    metadata_file.display(),
                ),
            );
            write_script(
                &rustc_bin,
                &format!(
                    "#!/bin/sh\necho \"$@\" >> {}\nprintf 'rustc 1.94.1-fake\\nhost: x86_64-unknown-fake\\nrelease: 1.94.1\\n'\n",
                    rustc_calls.display(),
                ),
            );

            let cache_dir = base.join("cache");
            std::fs::create_dir_all(cache_dir.join("logs")).unwrap();
            let session = BuildCacheSession {
                cache_dir: cache_dir.clone(),
                session_id: "session-memo-e2e".to_string(),
                session_log_path: cache_dir.join("logs/last-session.log"),
                journal_path: cache_dir.join("logs/last-session.jsonl"),
                session_stats_path: cache_dir.join("logs/last-session-stats.json"),
            };
            let args = vec!["build".to_string()];

            let plan_one = maybe_prepare_rust_artifact_plan(
                &cargo_bin, &rustc_bin, &args, &session, None, None,
            )
            .expect("first prepare")
            .expect("plan context");
            assert_eq!(
                call_count(&cargo_calls),
                2,
                "metadata + --version on first run"
            );
            assert_eq!(call_count(&rustc_calls), 1, "-Vv on first run");

            let plan_two = maybe_prepare_rust_artifact_plan(
                &cargo_bin, &rustc_bin, &args, &session, None, None,
            )
            .expect("second prepare")
            .expect("plan context");
            assert_eq!(
                call_count(&cargo_calls),
                2,
                "memo hit must not re-run cargo metadata / cargo --version"
            );
            assert_eq!(
                call_count(&rustc_calls),
                1,
                "memo hit must not re-run rustc -Vv"
            );
            assert_eq!(
                plan_one.plan_inputs_hash, plan_two.plan_inputs_hash,
                "a memo hit must produce the identical plan identity"
            );

            std::fs::write(
                ws.join("Cargo.toml"),
                "[workspace]\nmembers=[\"app\",\"other\"]\n",
            )
            .unwrap();
            std::fs::create_dir_all(ws.join("other")).unwrap();
            std::fs::write(
                ws.join("other/Cargo.toml"),
                "[package]\nname=\"other\"\nversion=\"0.1.0\"\n",
            )
            .unwrap();

            let _plan_three = maybe_prepare_rust_artifact_plan(
                &cargo_bin, &rustc_bin, &args, &session, None, None,
            )
            .expect("third prepare")
            .expect("plan context");
            assert_eq!(
                call_count(&cargo_calls),
                4,
                "a manifest mutation must re-run the authoritative subprocesses"
            );
            assert_eq!(call_count(&rustc_calls), 2);
        });

        match previous_mode {
            Some(value) => std::env::set_var(TARGET_CACHE_MODE_ENV_VAR, value),
            None => std::env::remove_var(TARGET_CACHE_MODE_ENV_VAR),
        }
        match previous_memo {
            Some(value) => std::env::set_var(RUST_PLAN_MEMO_ENV_VAR, value),
            None => std::env::remove_var(RUST_PLAN_MEMO_ENV_VAR),
        }
        if let Err(panic) = result {
            std::panic::resume_unwind(panic);
        }
    }
}
