//! Regression guard for soldr#1663: no environment variable may be
//! mutated under two different barriers.
//!
//! ## What went wrong
//!
//! Rust runs a crate's unit tests in one process, so a module-local
//! `static ENV_LOCK: Mutex<()>` serialises only that module. Two
//! modules mutated `SOLDR_USE_LEGACY_XWIN`: `blessed_build` under the
//! crate-wide `TEST_PROCESS_ENV_LOCK`, `main_tests` under a private
//! mutex of its own.
//!
//! Two mutexes guarding one variable provide **no mutual exclusion at
//! all**. Both tests took *a* lock, so each reads as correct in
//! isolation — you have to notice the locks are different objects.
//!
//! ## Why this shape of lint
//!
//! The obvious fix is "make every module use one crate-wide lock", and
//! I tried it: all 19 private barriers aliased to
//! `TEST_PROCESS_ENV_LOCK`. The suite then failed with
//! `TEST HUNG (>5s): compile_daemon_fallback_count_recovers_from_log_replacement`,
//! and passed under `--test-threads=1`.
//!
//! It was starvation, not deadlock. `compile_dispatch`'s `EnvVarGuard`
//! holds its mutex for the guard's whole lifetime, so once every
//! env-mutating test in the crate shares one mutex, a test carrying a
//! short custom deadline queues behind all of them. Collapsing
//! fine-grained locks over *disjoint* variables into one global barrier
//! costs suite latency and buys no correctness.
//!
//! So the rule enforced here is the one that actually matters: a given
//! variable must be mutated under a single barrier. Modules that guard
//! variables nobody else touches keep their own lock.
//!
//! This is a source lint because the defect is *identity* — "these two
//! locks are not the same object" — which no passing test can observe,
//! and the race it enables is timing-dependent.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::common;

/// Every workspace crate's `src/`, not just this one's.
///
/// soldr#1994: this returned only `soldr-cli/src`, so the lint guarded one of
/// five crates. `soldr-fetch`'s `llvm.rs` raced itself on `SOLDR_LLVM_DIR`
/// with no barrier at all, and the failure surfaced on an unrelated daemon PR
/// -- exactly the class this lint exists to prevent, in a crate it could not
/// see. `no_timed_test_guard.rs` next door already walked the whole workspace.
fn crate_src_roots() -> Vec<PathBuf> {
    // soldr#2008: resolved at *runtime*, not from `CARGO_MANIFEST_DIR`. That
    // env var is baked in at compile time and points at the machine that built
    // the archive, so the pre-built test-archive lanes -- which run away from
    // any checkout -- would silently scan nothing and report a clean lint.
    // `test_archived_source_tests_use_only_runtime_workspace_resolution`
    // enforces this repo-wide; these files were violating it.
    let crates_dir = common::workspace_root().join("crates");
    let Ok(entries) = fs::read_dir(&crates_dir) else {
        return Vec::new();
    };
    let mut roots: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path().join("src"))
        .filter(|src| src.is_dir())
        .collect();
    roots.sort();
    roots
}

/// True when at least one workspace crate source tree is present.
///
/// The pre-built test-archive lanes run away from the checkout, where every
/// root is absent and the lint has nothing to say.
fn any_src_root_exists(roots: &[PathBuf]) -> bool {
    roots.iter().any(|root| root.is_dir())
}

fn repo_relative(path: &Path) -> String {
    let root = common::workspace_root();
    path.strip_prefix(&root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn collect_rs(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_rs(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// Which barrier a file serialises on: the shared crate lock, a private
/// mutex, or nothing.
#[derive(Debug, PartialEq, Eq, Clone)]
enum Barrier {
    Shared,
    Private,
    None,
}

fn barrier_of(text: &str) -> Barrier {
    if text.contains("TEST_PROCESS_ENV_LOCK") {
        return Barrier::Shared;
    }
    let declares_private = text.lines().any(|line| {
        let t = line.trim();
        t.contains("Mutex")
            && (t.starts_with("static ") || t.contains(" static "))
            && (t.contains("ENV_LOCK") || t.contains("ENV_MUTEX") || t.contains("ENV_GUARD"))
    });
    if declares_private {
        Barrier::Private
    } else {
        Barrier::None
    }
}

/// Environment variables mutated in `text`, by the literal or constant
/// name at the call site. Constants are reduced to their final path
/// segment so `USE_LEGACY_XWIN_ENV_VAR` and
/// `crate::blessed_build::USE_LEGACY_XWIN_ENV_VAR` compare equal —
/// missing that is precisely how the original overlap stayed hidden.
fn mutated_vars(text: &str) -> Vec<String> {
    let mut found = Vec::new();
    // `.set("NAME", ..)` catches guard helpers whose own `set_var(key, ..)`
    // takes a *variable*, which the two markers below cannot see through --
    // that blind spot is why soldr#1938's mutations were invisible here.
    for marker in ["set_var(", "remove_var(", ".set("] {
        let mut rest = text;
        while let Some(idx) = rest.find(marker) {
            rest = &rest[idx + marker.len()..];
            let arg: String = rest
                .chars()
                .take_while(|c| *c != ',' && *c != ')')
                .collect::<String>()
                .trim()
                .trim_matches('"')
                .trim_start_matches('&')
                .to_string();
            let name = arg.rsplit("::").next().unwrap_or(&arg).to_string();
            let looks_like_a_name = !name.is_empty()
                && name
                    .chars()
                    .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
            if looks_like_a_name {
                found.push(name);
            }
        }
    }
    found.sort();
    found.dedup();
    found
}

/// Files whose env mutation is production behaviour, not a test
/// fixture, so no test barrier applies. Each needs a reason.
const PRODUCTION_ENV_WRITERS: &[(&str, &str)] = &[
    (
        "crates/soldr-cli/src/cli_dispatch.rs",
        "prepend_path_dirs_to_env rewrites PATH for the child process",
    ),
    (
        "crates/soldr-cli/src/msvc_host.rs",
        "exports the discovered MSVC environment (PATH/INCLUDE/LIB) for the build",
    ),
    (
        "crates/soldr-cli/src/prepare_github_env.rs",
        "soldr prepare exports resolved target/toolchain environment into its own process \
         before later preparation stages; production behavior, not a test fixture",
    ),
    (
        "crates/soldr-cli/src/soldr_main.rs",
        "soldr#1766: --allow-unpinned is surfaced as SOLDR_ALLOW_UNPINNED at startup          so the whole process tree agrees, including the daemon, which auto-forwards          SOLDR_*. This is the CLI translating a flag into environment, not a test          mutating shared state, so no barrier applies",
    ),
    (
        "crates/soldr-cli/src/cli_args.rs",
        "soldr#1802: `Cli::export_global_env` publishes --allow-unpinned, --jobs and \
         --timestamp-lines as SOLDR_* so one resolver owns each knob's precedence. Same \
         flag-into-environment translation as the soldr_main.rs entry above, not a test \
         mutating shared state. Previously invisible to this lint only because the file \
         also carried a test module using TEST_PROCESS_ENV_LOCK; moving those tests to \
         cli_args_tests.rs left the production write correctly bare",
    ),
    (
        "crates/soldr-cli/src/target_lifecycle.rs",
        "apply_to_process installs the blessed target's resolved environment and mutually \
         exclusive Rust flags before dispatching the production build; this is child-process \
         setup, not a test fixture mutating ambient state",
    ),
];

#[test]
fn no_env_var_is_guarded_by_two_different_barriers() {
    let roots = crate_src_roots();
    if !any_src_root_exists(&roots) {
        // The pre-built test-archive lanes run away from the checkout.
        eprintln!("env_lock_lint: skipping — no workspace crate sources present");
        return;
    }

    let mut files = Vec::new();
    for root in &roots {
        collect_rs(root, &mut files);
    }
    files.sort();

    let production: Vec<&str> = PRODUCTION_ENV_WRITERS.iter().map(|(p, _)| *p).collect();

    // var -> (barrier -> files)
    let mut by_var: BTreeMap<String, BTreeMap<String, Vec<String>>> = BTreeMap::new();
    for file in &files {
        let rel = repo_relative(file);
        if rel.ends_with("soldr-cli/src/lib.rs") || production.contains(&rel.as_str()) {
            continue;
        }
        let Ok(text) = fs::read_to_string(file) else {
            continue;
        };
        let barrier = barrier_of(&text);
        // A file with no barrier at all and no tests is production code
        // we have not catalogued; only flag it if it also shares a var.
        let label = match barrier {
            Barrier::Shared => "TEST_PROCESS_ENV_LOCK".to_string(),
            Barrier::Private => format!("private mutex in {rel}"),
            Barrier::None => format!("NO barrier ({rel})"),
        };
        for var in mutated_vars(&text) {
            by_var
                .entry(var)
                .or_default()
                .entry(label.clone())
                .or_default()
                .push(rel.clone());
        }
    }

    let mut violations = Vec::new();
    for (var, barriers) in &by_var {
        if barriers.len() < 2 {
            continue;
        }
        let mut lines = vec![format!(
            "  {var} is mutated under {} barriers:",
            barriers.len()
        )];
        for (barrier, files) in barriers {
            lines.push(format!("      [{barrier}] {}", files.join(", ")));
        }
        violations.push(lines.join("\n"));
    }

    assert!(
        violations.is_empty(),
        "soldr#1663: an environment variable guarded by two different barriers \
         is not guarded at all — a crate's unit tests share one process, so each \
         module's mutex only excludes its own tests.\n\n{}\n\n\
         Point every mutator of that variable at the same barrier; \
         `use crate::TEST_PROCESS_ENV_LOCK as ENV_LOCK;` is the shared one. \
         Do NOT move every module onto it wholesale — barriers over disjoint \
         variables are deliberately separate, and collapsing them starved a \
         test with a short deadline (see this file's header). If the mutation \
         is production behaviour, add the file to PRODUCTION_ENV_WRITERS with \
         a reason.",
        violations.join("\n\n"),
    );
}

// Plain `//`, not `///`: a doc comment cannot attach to a macro
// invocation and `-D warnings` makes `unused_doc_comments` fatal.
// Separate from the identity lint above, deliberately.
//
// #1899 raised poison policy while reviewing #1896 and chose not to
// fold it into that lint, on the grounds that it enforces barrier
// *identity* and mixing in a second property would blur reasoning that
// is currently very clear. That judgement stands — so this is its own
// check with its own argument.
//
// The argument: a `Mutex` poisons when a thread panics while holding
// it, and every later `lock()` returns `Err`. While a barrier is
// module-private that is contained — only a panic in that module can
// poison it, so one failure stays one failure. Once modules *share* a
// barrier, a panic anywhere under it poisons the lock for everyone,
// and each bare `.unwrap()` converts an unrelated failure into another
// one. #1899 fixed seven such sites in `main_tests`; this catches the
// next batch before it lands.
//
// The convention is `.lock().unwrap_or_else(|e| e.into_inner())`:
// these barriers serialise access, they do not protect an invariant
// inside the guarded data (it is `()`), so a poisoned lock carries no
// information worth propagating.
//
// Scoped to files that actually alias the shared barrier. A private
// barrier keeps its blast radius, so a bare unwrap there is a style
// question rather than a correctness one, and flagging ~60 of them
// would bury the signal.
#[test]
fn shared_barrier_acquisitions_recover_from_poisoning() {
    let roots = crate_src_roots();
    if !any_src_root_exists(&roots) {
        eprintln!("env_lock_lint: skipping — no workspace crate sources present");
        return;
    }

    let mut files = Vec::new();
    for root in &roots {
        collect_rs(root, &mut files);
    }
    files.sort();

    let mut offenders = Vec::new();
    for file in &files {
        let Ok(text) = fs::read_to_string(file) else {
            continue;
        };
        // Only files on the shared barrier; `lib.rs` declares it.
        let rel = repo_relative(file);
        if rel.ends_with("soldr-cli/src/lib.rs") || !text.contains("TEST_PROCESS_ENV_LOCK") {
            continue;
        }
        for (idx, line) in text.lines().enumerate() {
            if line.contains(".lock().unwrap()") {
                offenders.push(format!("  {rel}:{}", idx + 1));
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "soldr#1663 / #1899: these acquire the *shared* environment barrier \
         with a bare `.unwrap()`:\n{}\n\n\
         A panic anywhere under a shared barrier poisons it for every other \
         module, so each of these turns one unrelated failure into an extra \
         one — noisy red CI that trains people to re-run rather than read.\n\n\
         Use the convention the other sites use:\n\
         \x20   let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());\n\n\
         The guarded data is `()`, so a poisoned lock carries no invariant \
         worth propagating.",
        offenders.join("\n"),
    );
}

/// Variables that production code reads to resolve paths, from many call
/// sites, so *any* concurrent test is potentially a reader.
///
/// soldr#1938: `trampoline_config_tests` mutated all three under a private
/// mutex. The "two barriers for one variable" rule above could not fire --
/// no other file writes them -- yet the race was real, because the readers
/// are not other writers. They are `trampoline_config.rs`, `binaries.rs`,
/// `exec_cmd.rs`, and `rust_plan_memo.rs`, reached transitively by tests
/// that have no idea the variable is being swapped underneath them.
///
/// For this class, "does anyone else write it" is the wrong question.
const AMBIENT_ENV_VARS: &[&str] = &["CARGO_HOME", "HOME", "USERPROFILE", "RUSTUP_HOME", "PATH"];

#[test]
fn ambient_path_vars_are_mutated_only_under_the_shared_barrier() {
    // Deliberately soldr-cli only, unlike the two rules above.
    //
    // This one requires the *shared* barrier, and `TEST_PROCESS_ENV_LOCK`
    // lives in soldr-cli. The dependency runs soldr-core -> soldr-cli, so
    // an upstream crate cannot reach it: `soldr-core`'s
    // `cargo_path_check.rs` mutates PATH in a test and could not satisfy a
    // widened rule however true the rule is. Widening this without first
    // moving a barrier into soldr-core (soldr#1896 identified that as the
    // only common dependency) would manufacture an unfixable failure.
    let src = common::workspace_root().join("crates/soldr-cli/src");
    if !src.is_dir() {
        eprintln!("env_lock_lint: skipping — {} absent", src.display());
        return;
    }
    let mut files = Vec::new();
    collect_rs(&src, &mut files);
    files.sort();

    let mut offenders = Vec::new();
    for file in &files {
        let rel = repo_relative(file);
        if PRODUCTION_ENV_WRITERS.iter().any(|(p, _)| *p == rel) {
            continue;
        }
        let Ok(text) = fs::read_to_string(file) else {
            continue;
        };
        if barrier_of(&text) != Barrier::Private {
            continue;
        }
        let ambient: Vec<String> = mutated_vars(&text)
            .into_iter()
            .filter(|v| AMBIENT_ENV_VARS.contains(&v.as_str()))
            .collect();
        if !ambient.is_empty() {
            offenders.push(format!(
                "{rel} mutates {} under a private mutex",
                ambient.join(", ")
            ));
        }
    }

    assert!(
        offenders.is_empty(),
        "these files mutate path-resolution variables that production code reads \
         from many call sites, so every concurrent test is a potential reader. A \
         private mutex serialises only the module that declares it. Use \
         `crate::TEST_PROCESS_ENV_LOCK`:\n  {}",
        offenders.join("\n  ")
    );
}
