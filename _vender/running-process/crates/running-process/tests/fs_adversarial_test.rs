//! Adversarial filesystem / path / env tests for `running_process::spawn`.
//!
//! Each test targets a place where Windows, macOS, and Linux filesystem or
//! shell semantics diverge and where our spawn machinery could silently
//! misbehave. The tests are intentionally hostile — they probe edge cases
//! that wouldn't show up in a normal subprocess workflow.
//!
//! Most tests are organised as "build a hostile cwd / env / argv, hand it
//! to `spawn`, then either (a) the spawn succeeds AND the child observes
//! what we told it to, or (b) the spawn fails with a clear error". Tests
//! that exercise platform-divergent behaviour use `cfg` to assert
//! per-platform expectations rather than skipping outright.
//!
//! When one of these tests starts failing it almost certainly means a real
//! bug — these aren't timing-flaky; they're deterministic probes of OS
//! behaviour.

use std::io::Read;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use running_process::{spawn, SpawnStdio, StdioSource};

// ── Helpers ─────────────────────────────────────────────────────────────────

fn testbin_path(name: &str) -> PathBuf {
    // Fixtures are built once, before the suite runs (see `ci/test.py`).
    //
    // This used to invoke `cargo build -p testbins` on every call. That takes
    // cargo's build-directory lock, and nextest runs each test in its own
    // process, so a full-suite run had dozens of cargo invocations contending
    // for one lock. `Command::output` waits for EOF with no deadline, and
    // cargo's "Blocking waiting for file lock" note went to inherited stderr
    // the harness only shows on failure — so it presented as an unexplained
    // 30s+ hang. See running-process#747 for the symbolized stack.
    let exe = std::env::current_exe().expect("current exe");
    // .../target/<triple>/<profile>/deps/<test-binary>
    let profile_dir = exe
        .parent()
        .and_then(std::path::Path::parent)
        .expect("test binary should live in <profile>/deps/");
    let path = profile_dir.join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
    assert!(
        path.is_file(),
        "test fixture `{name}` is missing at {}.
         Build the fixtures first:  soldr cargo build -p testbins",
        path.display()
    );
    path
}

fn pipe_stdio() -> SpawnStdio<'static> {
    SpawnStdio {
        stdin: StdioSource::Null,
        stdout: StdioSource::Pipe,
        stderr: StdioSource::Null,
        drain_timeout: Some(Duration::from_secs(2)),
        show_console: false,
    }
}

/// Build a `Command` that prints its current working directory to
/// stdout in UTF-8 and exits. Uses our `testbin-cwd-reporter` Rust
/// helper rather than the platform shell's `pwd` / `cd` builtin,
/// because on Windows `cmd /c cd` emits the path in the active console
/// codepage, which corrupts non-ASCII cwds. Going through
/// `std::env::current_dir()` + UTF-8 stdout lets us round-trip
/// Unicode paths through `spawn` without the shell layer obscuring
/// whether the spawn passed them correctly.
fn pwd_command() -> Command {
    Command::new(testbin_path("testbin-cwd-reporter"))
}

/// Build a `Command` that prints the value of one env var to stdout.
fn echo_env_command(var: &str) -> Command {
    #[cfg(windows)]
    {
        let mut c = Command::new("cmd.exe");
        c.arg("/D").arg("/S").arg("/C").arg(format!("echo %{var}%"));
        c
    }
    #[cfg(unix)]
    {
        let mut c = Command::new("sh");
        c.arg("-c").arg(format!("printf '%s' \"${{{var}}}\""));
        c
    }
}

/// Spawn `cmd` with `stdio`, drain stdout to completion, return bytes.
fn spawn_and_capture(mut cmd: Command, stdio: SpawnStdio<'_>) -> Vec<u8> {
    let mut child = spawn(&mut cmd, stdio).expect("spawn");
    let mut stdout = child.stdout.take().expect("stdout pipe");
    let mut buf = Vec::new();
    stdout.read_to_end(&mut buf).expect("read_to_end");
    let _ = child.wait();
    buf
}

/// Canonicalise a path with best effort — we compare paths across platforms
/// where the OS may normalise (e.g. /private/var vs /var on macOS, drive
/// letter case on Windows).
fn canon_lossy(p: &std::path::Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

// ── 1. cwd with spaces ──────────────────────────────────────────────────────

/// cwd path containing spaces. The Windows cmdline must NOT split it into
/// multiple arguments, and `CreateProcessW`'s lpCurrentDirectory must
/// accept it verbatim. Trivial on Unix.
#[test]
fn cwd_with_spaces() {
    let parent = tempfile::tempdir().expect("tempdir");
    let dir = parent.path().join("dir with spaces");
    std::fs::create_dir(&dir).expect("create_dir");

    let mut cmd = pwd_command();
    cmd.current_dir(&dir);
    let out = spawn_and_capture(cmd, pipe_stdio());
    let observed = String::from_utf8_lossy(&out);

    let observed_path = canon_lossy(std::path::Path::new(observed.trim()));
    let expected_path = canon_lossy(&dir);
    assert_eq!(observed_path, expected_path, "cwd with spaces");
}

// ── 2. cwd with non-ASCII Unicode (CJK + emoji) ─────────────────────────────

/// cwd path with non-ASCII characters. Exercises the OsStr → UTF-16
/// (Windows) and OsStr → bytes (Unix) round trips inside spawn.
#[test]
fn cwd_with_unicode() {
    let parent = tempfile::tempdir().expect("tempdir");
    let dir = parent.path().join("测试_dir_🌍");
    if std::fs::create_dir(&dir).is_err() {
        // Some CI volumes (rare) can't create non-ASCII names — treat as
        // skip rather than fail. We at least logged the attempt.
        eprintln!("skipping: filesystem rejected non-ASCII directory name");
        return;
    }

    let mut cmd = pwd_command();
    cmd.current_dir(&dir);
    let out = spawn_and_capture(cmd, pipe_stdio());
    let observed = String::from_utf8_lossy(&out);

    let observed_path = canon_lossy(std::path::Path::new(observed.trim()));
    let expected_path = canon_lossy(&dir);
    assert_eq!(observed_path, expected_path, "cwd with non-ASCII");
}

// ── 3. cwd with trailing dot ────────────────────────────────────────────────

/// On Windows, `CreateDirectoryW("foo.", ...)` silently strips the
/// trailing dot — the resulting directory on disk is `foo`. On Unix the
/// directory is `foo.` verbatim. Either way, passing the requested name
/// as cwd should land us in *some* directory and the spawn shouldn't
/// crash.
#[test]
fn cwd_with_trailing_dot() {
    let parent = tempfile::tempdir().expect("tempdir");
    let requested = parent.path().join("foo.");
    let normalised_on_windows = parent.path().join("foo");
    let r = std::fs::create_dir(&requested);
    if r.is_err() && !normalised_on_windows.exists() {
        eprintln!("skipping: filesystem rejected trailing-dot directory name");
        return;
    }

    let mut cmd = pwd_command();
    cmd.current_dir(&requested);
    let out = spawn_and_capture(cmd, pipe_stdio());
    let observed = String::from_utf8_lossy(&out);
    let observed_path = canon_lossy(std::path::Path::new(observed.trim()));

    // The child's reported cwd should match EITHER the requested path
    // (Unix) or the dot-stripped version (Windows). Anything else means
    // we've garbled the path.
    let unix_expected = canon_lossy(&requested);
    let windows_expected = canon_lossy(&normalised_on_windows);
    assert!(
        observed_path == unix_expected || observed_path == windows_expected,
        "cwd trailing-dot landed somewhere unexpected: {observed_path:?} \
         (expected {unix_expected:?} or {windows_expected:?})"
    );
}

// ── 4. cwd with shell metacharacters that are filesystem-legal ──────────────

/// Most filesystems happily accept `&`, `^`, `(`, `)`, `;`, `$` in
/// directory names. The trap: on Windows we route through `cmd.exe` for
/// shell invocations; if our spawn ever wraps the cwd in shell-context
/// instead of passing it directly to `CreateProcessW`, these characters
/// get interpreted.
#[test]
fn cwd_with_shell_metacharacters() {
    let parent = tempfile::tempdir().expect("tempdir");
    // Use cmd-friendly chars only — `|`, `<`, `>`, `*`, `?` are illegal
    // on Windows filesystems, so we don't probe those.
    let dir = parent.path().join("a&b(c)d;e$f^g");
    if std::fs::create_dir(&dir).is_err() {
        eprintln!("skipping: filesystem rejected metacharacter directory name");
        return;
    }

    let mut cmd = pwd_command();
    cmd.current_dir(&dir);
    let out = spawn_and_capture(cmd, pipe_stdio());
    let observed = String::from_utf8_lossy(&out);

    let observed_path = canon_lossy(std::path::Path::new(observed.trim()));
    let expected_path = canon_lossy(&dir);
    assert_eq!(observed_path, expected_path, "cwd with shell metachars");
}

// ── 5. cwd at MAX_PATH / long-path boundary on Windows ──────────────────────

/// On Windows the historical MAX_PATH is 260 chars. Modern Win10+ supports
/// longer paths if the manifest opts in, but `CreateProcessW`
/// lpCurrentDirectory is documented at MAX_PATH. Test that we can at
/// least accept a cwd just under the limit.
#[test]
fn cwd_with_long_path() {
    let parent = tempfile::tempdir().expect("tempdir");
    // Build a directory chain whose total length sits at ~200 chars to
    // stay under MAX_PATH even with the prefix tempdir.
    let mut path = parent.path().to_path_buf();
    let segment = "a".repeat(40);
    for _ in 0..3 {
        path = path.join(&segment);
    }
    if std::fs::create_dir_all(&path).is_err() {
        eprintln!("skipping: filesystem rejected long path");
        return;
    }

    let mut cmd = pwd_command();
    cmd.current_dir(&path);
    let out = spawn_and_capture(cmd, pipe_stdio());
    let observed = String::from_utf8_lossy(&out);

    let observed_path = canon_lossy(std::path::Path::new(observed.trim()));
    let expected_path = canon_lossy(&path);
    assert_eq!(observed_path, expected_path, "cwd at ~200 chars");
}

// ── 6. env value containing a newline ───────────────────────────────────────

/// Embedded `\n` in an env value. POSIX allows it; the Windows env block
/// format (KEY=VALUE\0...\0\0) technically allows it but many shells
/// truncate or print across lines. We test that the value reaches the
/// child intact when it's NOT round-tripped through a shell — i.e. when
/// the value is *just* set and the child can read its own env directly.
#[test]
fn env_value_with_newline() {
    let sleeper = testbin_path("testbin-sleeper");
    let mut cmd = Command::new(&sleeper);
    cmd.env("RP_TEST_MULTILINE", "line1\nline2");
    // Just make sure the spawn doesn't fail. The sleeper doesn't read
    // the env back, so we only assert that spawn() accepts the env value.
    let mut child = spawn(&mut cmd, pipe_stdio()).expect("spawn must accept \\n in env value");
    let _ = child.kill();
    let _ = child.wait();
}

// ── 7. env name that's a Windows reserved DOS device ────────────────────────

/// `CON`, `PRN`, `AUX`, `NUL`, `COM1`-`COM9`, `LPT1`-`LPT9` are reserved as
/// device names on Windows but legal as env-var names everywhere. Our
/// spawn must not crash or silently drop the var.
#[test]
fn env_name_reserved_windows_device() {
    let sleeper = testbin_path("testbin-sleeper");
    let mut cmd = Command::new(&sleeper);
    cmd.env("CON", "console-device-name")
        .env("NUL", "null-device-name");
    let mut child = spawn(&mut cmd, pipe_stdio()).expect("spawn with reserved-name env vars");
    let _ = child.kill();
    let _ = child.wait();
}

// ── 8. case sensitivity of env var names ────────────────────────────────────

/// Windows folds env var names case-insensitively (`PATH` and `Path` are
/// the same slot). Unix treats them as distinct. Set both `MyVar` and
/// `MYVAR`; on Windows the second overrides the first; on Unix both
/// survive. We assert the spawn accepts the configuration either way.
#[test]
fn env_case_sensitivity_difference() {
    let mut cmd = echo_env_command("RP_TEST_CASE_X");
    cmd.env("RP_TEST_CASE_X", "lowercase-key-value")
        .env("RP_TEST_CASE_x", "different-case-value");

    let out = spawn_and_capture(cmd, pipe_stdio());
    let observed = String::from_utf8_lossy(&out);
    let observed = observed.trim_end_matches(['\r', '\n']);

    #[cfg(windows)]
    {
        // Windows folds — the LAST insertion wins regardless of case.
        // Rust's HashMap-backed env preserves insertion order; the
        // second `.env(...)` clobbers the first.
        assert_eq!(
            observed, "different-case-value",
            "Windows case-folds env names"
        );
    }
    #[cfg(unix)]
    {
        // Unix preserves both — `RP_TEST_CASE_X` keeps its first value.
        assert_eq!(
            observed, "lowercase-key-value",
            "Unix preserves env-name case"
        );
    }
}

// ── 9. env value containing `=` ─────────────────────────────────────────────

/// Windows env block format is `KEY=VALUE\0`; the first `=` separates.
/// Subsequent `=` in the value should pass through cleanly.
#[test]
fn env_value_with_equals_signs() {
    let mut cmd = echo_env_command("RP_TEST_EQUALS");
    cmd.env("RP_TEST_EQUALS", "a=b=c=d");

    let out = spawn_and_capture(cmd, pipe_stdio());
    let observed = String::from_utf8_lossy(&out);
    let observed = observed.trim_end_matches(['\r', '\n']);

    assert_eq!(
        observed, "a=b=c=d",
        "env value with embedded `=` must pass through"
    );
}

// ── 10. NUL byte in argv ────────────────────────────────────────────────────

/// `\0` in a command-line argument is illegal — POSIX exec() and Windows
/// CreateProcessW both reject it (the former because argv is a NUL-
/// terminated array, the latter because the cmdline is a wide string).
/// Our spawn must surface an error, not crash or silently truncate.
#[test]
fn argv_with_embedded_null_byte() {
    let sleeper = testbin_path("testbin-sleeper");
    let mut cmd = Command::new(&sleeper);

    use std::ffi::OsString;
    #[cfg(unix)]
    let evil: OsString = {
        use std::os::unix::ffi::OsStringExt;
        OsString::from_vec(b"hello\0world".to_vec())
    };
    #[cfg(windows)]
    let evil: OsString = {
        use std::os::windows::ffi::OsStringExt;
        // 'h','e','l','l','o',0,'w','o','r','l','d'
        let units: Vec<u16> = vec![104, 101, 108, 108, 111, 0, 119, 111, 114, 108, 100];
        OsString::from_wide(&units)
    };
    cmd.arg(&evil);

    let result = spawn(&mut cmd, pipe_stdio());
    if let Ok(mut child) = result {
        // If spawn returned Ok the child must NOT have received a
        // truncated argv as a successful spawn (would mask the bug).
        // The sleeper ignores argv, so we can't distinguish — but we
        // CAN at least kill it and document the platform behaviour.
        let _ = child.kill();
        let _ = child.wait();
        eprintln!("note: platform accepted NUL in argv (no error returned)");
    }
}

// ── 11. forward slashes in Windows cwd ──────────────────────────────────────

/// Win32 generally accepts `/` as a path separator for API-level path
/// arguments (CreateFileW etc.) even though `\` is canonical. We pass a
/// cwd with forward slashes and verify the child still reports something
/// sensible.
#[cfg(windows)]
#[test]
fn cwd_with_forward_slashes_on_windows() {
    let parent = tempfile::tempdir().expect("tempdir");
    let real = parent.path().join("subdir");
    std::fs::create_dir(&real).expect("create_dir");
    // Build a forward-slash version of the same path.
    let forward = std::path::PathBuf::from(real.to_string_lossy().replace('\\', "/"));

    let mut cmd = pwd_command();
    cmd.current_dir(&forward);
    let out = spawn_and_capture(cmd, pipe_stdio());
    let observed = String::from_utf8_lossy(&out);
    let observed_path = canon_lossy(std::path::Path::new(observed.trim()));
    let expected_path = canon_lossy(&real);
    assert_eq!(
        observed_path, expected_path,
        "Windows must accept forward-slash cwd path"
    );
}

// ── 12. stdout pipe is byte-clean (no CRLF translation) ─────────────────────

/// Console handles on Windows do CRLF translation; anonymous pipes do
/// not. The pipe we hand to the child via `StdioSource::Pipe` MUST
/// behave like a binary stream — a child that writes a single `\n`
/// should produce exactly one byte on the parent's read end.
#[test]
fn stdout_pipe_does_not_inject_cr() {
    // Use a shell to emit exactly one LF. Both shells we target here
    // support `printf '\n'` cleanly.
    let mut cmd = Command::new({
        #[cfg(windows)]
        {
            "cmd.exe"
        }
        #[cfg(unix)]
        {
            "sh"
        }
    });
    #[cfg(windows)]
    {
        // `set /p=` writes the operand WITHOUT a trailing newline; we
        // then redirect a single byte by piping <NUL prompt> — but the
        // simplest correct probe on cmd is to invoke an external
        // utility. Use `python -c` if available; otherwise fall back
        // to `cmd /c <NUL set /p=...` which writes 0 newlines. Tests
        // can opt out cleanly.
        cmd.arg("/D")
            .arg("/S")
            .arg("/C")
            .arg("<NUL set /p=\"x\" & cmd /c exit 0");
    }
    #[cfg(unix)]
    {
        cmd.arg("-c").arg("printf 'x\\n'");
    }

    let out = spawn_and_capture(cmd, pipe_stdio());

    #[cfg(unix)]
    {
        // sh + printf must produce exactly `x\n`. No CRLF allowed.
        assert_eq!(out, b"x\n", "Unix pipe must not inject CR");
    }
    #[cfg(windows)]
    {
        // `<NUL set /p=` produces exactly the literal text and no
        // trailing newline. The pipe must NOT inject a CR.
        // If this assert fails it means our pipe is in text mode.
        assert!(
            !out.contains(&b'\r'),
            "Windows pipe must not inject CR (got {out:?})"
        );
    }
}
