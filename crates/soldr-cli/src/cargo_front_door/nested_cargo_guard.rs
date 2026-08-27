//! Detect direct nested Cargo builds that can self-lock behind their parent Cargo.

use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use sysinfo::{Pid, System};

pub(crate) const PERMIT_ENV: &str = "SOLDR_NESTED_CARGO";
pub(crate) const PERMIT_VALUE: &str = "allow";
pub(crate) const SCAN_INTERVAL: Duration = Duration::from_millis(250);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NestedCargoDecision {
    Ignore,
    AllowIsolatedTarget,
    Reject,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NestedCargoFinding {
    pub pid: u32,
    pub argv: Vec<String>,
}

impl NestedCargoFinding {
    pub(crate) fn diagnostic(&self, outer_pid: u32) -> String {
        let argv = self
            .argv
            .iter()
            .take(12)
            .cloned()
            .collect::<Vec<_>>()
            .join(" ");
        format!(
            "rejected direct nested Cargo build before it could self-lock on the outer target: \
             outer cargo pid={outer_pid}, nested pid={}, argv={argv:?}; if this nested build uses \
             an intentionally isolated target, pass an explicit different --target-dir or set \
             {PERMIT_ENV}={PERMIT_VALUE} on the outer Soldr invocation (soldr#2924)",
            self.pid
        )
    }

    pub(crate) fn write_record(
        &self,
        outer_pid: u32,
        outer_target: Option<&Path>,
    ) -> Option<PathBuf> {
        let root = crate::core::SoldrPaths::new().ok()?.root;
        let dir = root.join("logs").join("nested-cargo");
        std::fs::create_dir_all(&dir).ok()?;
        let unix_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_millis())
            .unwrap_or(0);
        let path = dir.join(format!("{unix_ms}-{outer_pid}-{}.json", self.pid));
        let argv = self.argv.iter().take(12).collect::<Vec<_>>();
        let body = serde_json::json!({
            "schema_version": 1,
            "event": "nested_cargo_rejected",
            "unix_ms": unix_ms as u64,
            "outer_cargo_pid": outer_pid,
            "nested_cargo_pid": self.pid,
            "outer_target": outer_target.map(|target| target.display().to_string()),
            "argv": argv,
            "argv_truncated": self.argv.len() > 12,
            "permit_env": PERMIT_ENV,
        });
        std::fs::write(&path, body.to_string()).ok()?;
        Some(path)
    }
}

fn nested_cargo_permitted(value: Option<&str>) -> bool {
    value.is_some_and(|value| value.trim().eq_ignore_ascii_case(PERMIT_VALUE))
}

pub(crate) struct NestedCargoMonitor {
    root: Pid,
    outer_target: Option<PathBuf>,
    system: System,
}

impl NestedCargoMonitor {
    pub(crate) fn new(root_pid: u32, outer_target: Option<&Path>) -> Option<Self> {
        Self::new_with_permit(
            root_pid,
            outer_target,
            nested_cargo_permitted(std::env::var(PERMIT_ENV).ok().as_deref()),
        )
    }

    fn new_with_permit(
        root_pid: u32,
        outer_target: Option<&Path>,
        permitted: bool,
    ) -> Option<Self> {
        if permitted {
            return None;
        }
        Some(Self {
            root: Pid::from_u32(root_pid),
            outer_target: outer_target.map(normalize_path),
            system: System::new(),
        })
    }

    pub(crate) fn poll(&mut self) -> Option<NestedCargoFinding> {
        self.system.refresh_processes();
        for (pid, process) in self.system.processes() {
            let descendant = *pid != self.root && descends_from(*pid, self.root, &self.system);
            if !descendant {
                continue;
            }
            let argv = if process.cmd().is_empty() {
                running_process::observer::read_process_cmdline(pid.as_u32())
                    .ok()
                    .map(|command_line| split_command_line(&command_line))
                    .filter(|args| !args.is_empty())
                    .unwrap_or_else(|| vec![process.name().to_string()])
            } else {
                process.cmd().to_vec()
            };
            if classify_descendant(
                process.name(),
                &argv,
                process.cwd(),
                self.outer_target.as_deref(),
            ) == NestedCargoDecision::Reject
            {
                return Some(NestedCargoFinding {
                    pid: pid.as_u32(),
                    argv,
                });
            }
        }
        None
    }
}

fn split_command_line(command_line: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    for ch in command_line.chars() {
        match ch {
            '"' => quoted = !quoted,
            ch if ch.is_whitespace() && !quoted => {
                if !current.is_empty() {
                    args.push(std::mem::take(&mut current));
                }
            }
            ch => current.push(ch),
        }
    }
    if !current.is_empty() {
        args.push(current);
    }
    args
}

fn descends_from(pid: Pid, root: Pid, system: &System) -> bool {
    let mut cursor = Some(pid);
    let mut seen = HashSet::new();
    while let Some(current) = cursor {
        if current == root {
            return true;
        }
        if !seen.insert(current) {
            return false;
        }
        cursor = system.process(current).and_then(|process| process.parent());
    }
    false
}

pub(crate) fn classify_descendant(
    process_name: &str,
    argv: &[String],
    cwd: Option<&Path>,
    outer_target: Option<&Path>,
) -> NestedCargoDecision {
    if !is_cargo_name(process_name) && !argv.first().is_some_and(|value| is_cargo_name(value)) {
        return NestedCargoDecision::Ignore;
    }
    let Some(verb) = cargo_verb(argv) else {
        return NestedCargoDecision::Ignore;
    };
    if verb == "metadata" && argv.iter().any(|arg| arg == "--no-deps") {
        return NestedCargoDecision::Ignore;
    }
    if !is_build_like(verb) {
        return NestedCargoDecision::Ignore;
    }

    let Some(outer_target) = outer_target.map(normalize_path) else {
        return NestedCargoDecision::Reject;
    };
    let Some(target) = cargo_target_dir(argv) else {
        return NestedCargoDecision::Reject;
    };
    let target = if target.is_absolute() {
        normalize_path(&target)
    } else {
        normalize_path(&cwd.unwrap_or_else(|| Path::new(".")).join(target))
    };
    if paths_equal(&target, &outer_target) {
        NestedCargoDecision::Reject
    } else {
        NestedCargoDecision::AllowIsolatedTarget
    }
}

fn is_cargo_name(value: &str) -> bool {
    Path::new(value)
        .file_stem()
        .and_then(std::ffi::OsStr::to_str)
        .is_some_and(|stem| stem.eq_ignore_ascii_case("cargo"))
}

fn cargo_verb(argv: &[String]) -> Option<&str> {
    const VALUE_FLAGS: &[&str] = &[
        "--color",
        "--config",
        "--jobs",
        "-j",
        "--manifest-path",
        "--target-dir",
        "--lockfile-path",
        "-Z",
    ];
    let mut index = usize::from(argv.first().is_some_and(|value| is_cargo_name(value)));
    while index < argv.len() {
        let arg = argv[index].as_str();
        if arg.starts_with('+') || arg.starts_with('-') {
            if VALUE_FLAGS.contains(&arg) {
                index = index.saturating_add(2);
            } else {
                index = index.saturating_add(1);
            }
            continue;
        }
        return Some(arg);
    }
    None
}

fn cargo_target_dir(argv: &[String]) -> Option<PathBuf> {
    let mut index = 0usize;
    while index < argv.len() {
        let arg = &argv[index];
        if let Some(value) = arg.strip_prefix("--target-dir=") {
            return (!value.is_empty()).then(|| PathBuf::from(value));
        }
        if arg == "--target-dir" {
            return argv
                .get(index + 1)
                .filter(|value| !value.is_empty())
                .map(PathBuf::from);
        }
        index = index.saturating_add(1);
    }
    None
}

fn is_build_like(verb: &str) -> bool {
    matches!(
        verb,
        "b" | "build"
            | "c"
            | "check"
            | "t"
            | "test"
            | "clippy"
            | "r"
            | "run"
            | "bench"
            | "doc"
            | "rustc"
            | "fix"
    )
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                let _ = normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

fn paths_equal(left: &Path, right: &Path) -> bool {
    if cfg!(windows) {
        left.to_string_lossy()
            .eq_ignore_ascii_case(&right.to_string_lossy())
    } else {
        left == right
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|part| (*part).to_string()).collect()
    }

    #[test]
    fn build_like_nested_cargo_without_isolation_is_rejected() {
        for command in [
            argv(&["cargo", "build", "-p", "mock-agent"]),
            argv(&["cargo.exe", "test", "--workspace"]),
            argv(&["cargo", "--color", "always", "check"]),
            argv(&["cargo", "clippy", "--all-targets"]),
        ] {
            assert_eq!(
                classify_descendant(
                    "cargo.exe",
                    &command,
                    Some(Path::new("/repo")),
                    Some(Path::new("/repo/target"))
                ),
                NestedCargoDecision::Reject,
                "argv={command:?}"
            );
        }
    }

    #[test]
    fn non_locking_and_non_cargo_descendants_are_ignored() {
        for (name, command) in [
            ("cargo", argv(&["cargo", "--version"])),
            ("cargo.exe", argv(&["cargo", "metadata", "--no-deps"])),
            ("rustc", argv(&["rustc", "--crate-name", "demo"])),
        ] {
            assert_eq!(
                classify_descendant(
                    name,
                    &command,
                    Some(Path::new("/repo")),
                    Some(Path::new("/repo/target"))
                ),
                NestedCargoDecision::Ignore,
                "name={name} argv={command:?}"
            );
        }
    }

    #[test]
    fn only_explicit_allow_value_permits_nested_cargo() {
        for value in ["allow", "ALLOW", " allow "] {
            assert!(nested_cargo_permitted(Some(value)));
        }
        for value in [None, Some(""), Some("true"), Some("off")] {
            assert!(!nested_cargo_permitted(value));
        }
    }

    #[test]
    fn quoted_windows_command_line_is_split_for_fallback_inspection() {
        assert_eq!(
            split_command_line(
                r#""C:\Program Files\Rust\bin\cargo.exe" build --manifest-path "C:\my repo\Cargo.toml""#
            ),
            argv(&[
                r#"C:\Program Files\Rust\bin\cargo.exe"#,
                "build",
                "--manifest-path",
                r#"C:\my repo\Cargo.toml"#,
            ])
        );
    }

    #[test]
    fn explicit_distinct_target_is_allowed_but_same_target_is_rejected() {
        let outer = Path::new("/repo/target");
        assert_eq!(
            classify_descendant(
                "cargo",
                &argv(&["cargo", "build", "--target-dir", "/tmp/isolated"]),
                Some(Path::new("/repo")),
                Some(outer)
            ),
            NestedCargoDecision::AllowIsolatedTarget
        );
        assert_eq!(
            classify_descendant(
                "cargo",
                &argv(&["cargo", "build", "--target-dir=/repo/target"]),
                Some(Path::new("/repo")),
                Some(outer)
            ),
            NestedCargoDecision::Reject
        );
    }
}
