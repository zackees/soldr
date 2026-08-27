fn resolved_toolchain_is_nightly(explicit_toolchain: Option<&str>) -> bool {
    let candidate = explicit_toolchain
        .map(str::to_owned)
        .or_else(|| std::env::var("RUSTUP_TOOLCHAIN").ok());
    candidate.is_some_and(|toolchain| {
        let channel = toolchain
            .split('-')
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase();
        channel == "nightly"
    })
}

fn emit_zthreads_fallback_warning(value: &str) {
    use std::io::IsTerminal;

    let github_actions = foreign_env_flag("GITHUB_ACTIONS");
    let use_color = !github_actions
        && std::env::var_os("NO_COLOR").is_none()
        && std::io::stderr().is_terminal();
    eprintln!(
        "{}",
        zthreads_fallback::render_warning(value, github_actions, use_color)
    );
}

fn retry_zthreads_without_flag(
    context: &ZthreadsRetryContext,
    explicit_toolchain: Option<&str>,
    plan: &zthreads_fallback::FallbackPlan,
) -> Result<i32, SoldrError> {
    let exe = std::env::current_exe()?;
    let mut command = std::process::Command::new(exe);
    command.args(context.cli_args());
    command.env(zthreads_fallback::ATTEMPTED_ENV, "1");
    // soldr#2739: this is a soldr -> soldr spawn with a fresh pid, so the
    // re-entrancy guard needs the edge marker to tell it apart from an
    // unsanctioned nested entry. Bounded by ATTEMPTED_ENV above.
    command.env(soldr_core::self_relocate::SELF_SPAWN_EDGE_ENV_VAR, "1");
    if let Some(toolchain) = explicit_toolchain {
        command.env("RUSTUP_TOOLCHAIN", toolchain);
    }
    for (key, value) in &plan.env {
        match value {
            Some(value) => {
                command.env(key, value);
            }
            None => {
                command.env_remove(key);
            }
        }
    }
    suppress_windows_console_window(&mut command);
    configure_cargo_child_for_timeout(&mut command);
    let mut child = debug_trace::spawn_traced(&mut command, "soldr -Zthreads fallback")
        .map_err(|err| SoldrError::Other(format!("spawn -Zthreads fallback failed: {err}")))?;
    let status = wait_for_cargo_child(&mut child, "soldr -Zthreads fallback", None, None)?;
    Ok(status
        .code()
        .unwrap_or(if status.success() { 0 } else { 1 }))
}

fn cargo_args_have_message_format(args: &[String]) -> bool {
    args.iter()
        .any(|arg| arg == "--message-format" || arg.starts_with("--message-format="))
}

/// soldr#1802: the anchor for elapsed-line stamping, or `None` when
/// stamping is off.
///
/// Resolved per run rather than once globally so a test or a caller can
/// flip `SOLDR_TIMESTAMP_LINES` without a process restart. `t0` is
/// `Instant::now()` at the point the relay starts, which is within
/// milliseconds of the session start recorded in the build log.
fn line_stamp_anchor(is_terminal: bool) -> Option<Instant> {
    let raw = std::env::var(timestamp_tee::TIMESTAMP_LINES_ENV_VAR).ok();
    if !timestamp_tee::should_timestamp(raw.as_deref(), is_terminal) {
        return None;
    }
    // Emit the absolute anchor once, so a reader can convert the
    // elapsed offsets that follow back into wall-clock time. Written
    // before the child starts, so it always precedes the stamped lines.
    eprint!("{}", timestamp_tee::epoch_anchor_line(current_unix_ms()));
    Some(Instant::now())
}

fn run_command_capturing_cargo_json(
    command: &mut std::process::Command,
    target_dir: &Path,
    timeout: Option<Duration>,
) -> Result<(std::process::ExitStatus, String, Vec<String>), SoldrError> {
    command.stdout(std::process::Stdio::piped());
    command.stderr(std::process::Stdio::piped());
    configure_cargo_child_for_timeout(command);
    let mut child = debug_trace::spawn_traced(command, "cargo JSON capture")
        .map_err(|err| SoldrError::Other(format!("spawn cargo for JSON capture failed: {err}")))?;
    // soldr#2546 slice 3: capture modes own their pipes, so descendant
    // observation attaches to the spawned pid post-hoc.
    let observation = debug_trace::DescendantObservation::attach(child.id(), "cargo JSON capture");
    let stamp = line_stamp_anchor(std::io::IsTerminal::is_terminal(&std::io::stdout()));
    let stdout_rx = spawn_capture_pipe_reader_to_stdout(child.stdout.take().expect("piped"), stamp);
    let stderr_rx = spawn_capture_pipe_reader(child.stderr.take().expect("piped"), stamp);
    let status = wait_for_cargo_child(
        &mut child,
        "cargo JSON capture",
        timeout,
        Some(target_dir),
    )?;
    if let Some(observation) = observation {
        observation.finish();
    }
    let stdout = drain_capture_pipe_after_child_exit(&stdout_rx, "cargo JSON stdout");
    let stderr = drain_capture_pipe_after_child_exit(&stderr_rx, "cargo JSON stderr");
    let paths = parse_cargo_artifact_closure(&stdout, target_dir);
    Ok((
        status,
        strip_diagnostics::merge_cargo_json_diagnostics(&stderr, &stdout),
        paths,
    ))
}

fn parse_cargo_artifact_closure(stdout: &[u8], target_dir: &Path) -> Vec<String> {
    let mut paths = BTreeMap::<String, ()>::new();
    let mut complete = true;
    for line in String::from_utf8_lossy(stdout).lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            complete = false;
            continue;
        };
        let Some(reason) = value.get("reason").and_then(serde_json::Value::as_str) else {
            continue;
        };
        match reason {
            "compiler-artifact" => {
                if let Some(filenames) =
                    value.get("filenames").and_then(serde_json::Value::as_array)
                {
                    for filename in filenames.iter().filter_map(serde_json::Value::as_str) {
                        add_cargo_closure_path(&mut paths, Path::new(filename), target_dir);
                    }
                } else {
                    complete = false;
                }
            }
            "build-script-executed" => {
                if let Some(out_dir) = value.get("out_dir").and_then(serde_json::Value::as_str) {
                    add_cargo_closure_path(&mut paths, Path::new(out_dir), target_dir);
                } else {
                    complete = false;
                }
            }
            "compiler-message" | "build-finished" | "text" => {}
            _ => complete = false,
        }
    }
    if !complete || paths.is_empty() {
        return Vec::new();
    }
    paths.into_keys().collect()
}

fn add_cargo_closure_path(paths: &mut BTreeMap<String, ()>, path: &Path, target_dir: &Path) {
    let Ok(relative) = path.strip_prefix(target_dir) else {
        return;
    };
    if !relative.as_os_str().is_empty() {
        paths.insert(relative.to_string_lossy().replace('\\', "/"), ());
    }
    if path
        .components()
        .any(|component| component.as_os_str() == ".fingerprint")
    {
        return;
    }
    if let Some(parent) = path.parent() {
        if let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) {
            let fingerprint_name = stem.strip_prefix("lib").unwrap_or(stem);
            let fingerprint_dir = parent
                .parent()
                .map(|profile| profile.join(".fingerprint").join(fingerprint_name));
            if let Some(dir) = fingerprint_dir {
                collect_closure_files(paths, &dir, target_dir);
            }
        }
    }
    // `symlink_metadata` rather than `is_dir()`: the latter follows the
    // link, so a symlinked directory would be descended into here even
    // though the walk deliberately skips symlinks (#1662).
    if std::fs::symlink_metadata(path).is_ok_and(|meta| meta.is_dir()) {
        collect_closure_files(paths, path, target_dir);
    }
}

fn collect_closure_files(paths: &mut BTreeMap<String, ()>, root: &Path, target_dir: &Path) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        // `file_type()` does not follow the link; `Path::is_dir`/`is_file`
        // (used here before) both do. Following them let a symlinked
        // directory pull an unrelated tree into the artifact closure, and a
        // symlink cycle recurse forever through this function and
        // `add_cargo_closure_path`, which are mutually recursive (#1662).
        let Ok(entry_type) = entry.file_type() else {
            continue;
        };
        if entry_type.is_symlink() {
            continue;
        }
        if entry_type.is_dir() {
            // Boundary check before descending. `add_cargo_closure_path`
            // already refuses to *record* a path outside `target_dir`, but
            // nothing stopped this function from *walking* one, so a
            // directory reachable from the target tree could send the walk
            // anywhere on disk.
            if path.strip_prefix(target_dir).is_err() {
                continue;
            }
            collect_closure_files(paths, &path, target_dir);
        } else if entry_type.is_file() {
            add_cargo_closure_path(paths, &path, target_dir);
        }
    }
}

fn spawn_capture_pipe_reader_to_stdout<R>(
    mut reader: R,
    stamp_from: Option<Instant>,
) -> std::sync::mpsc::Receiver<CapturePipeMessage>
where
    R: std::io::Read + Send + 'static,
{
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        // soldr#1802: the stamped copy goes to the terminal only. The
        // capture channel below must stay raw — the cargo-JSON parser
        // matches on cargo's exact bytes.
        let mut stamped =
            stamp_from.map(|t0| timestamp_tee::TimestampedTee::new(std::io::stdout(), t0));
        let mut chunk = [0u8; 8192];
        loop {
            match reader.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    let bytes = chunk[..n].to_vec();
                    match stamped.as_mut() {
                        Some(tee) => {
                            let _ = tee.write_all(&bytes);
                            let _ = tee.flush();
                        }
                        None => {
                            let _ = std::io::stdout().lock().write_all(&bytes);
                        }
                    }
                    let _ = tx.send(CapturePipeMessage::Chunk(bytes));
                }
                Err(_) => break,
            }
        }
        let _ = std::io::stdout().lock().flush();
        let _ = tx.send(CapturePipeMessage::Eof);
    });
    rx
}

fn run_command_inheriting_stdio(
    command: &mut std::process::Command,
    timeout: Option<Duration>,
    outer_target: Option<&Path>,
) -> Result<std::process::ExitStatus, SoldrError> {
    if debug_trace::enabled() {
        // soldr#2546 slice 2: under --debug the inherited-stdio mode runs
        // through the running-process observer so the timeline records
        // descendants (rustc, build scripts) rather than only the direct
        // cargo child. running-process owns containment for this spawn.
        return debug_trace::run_observed_inheriting_stdio(
            command,
            "cargo",
            timeout,
            Duration::from_secs(CARGO_WAIT_HEARTBEAT_SECS),
            outer_target,
        );
    }
    configure_cargo_child_for_timeout(command);
    let mut child = debug_trace::spawn_traced(command, "cargo")
        .map_err(|err| SoldrError::Other(format!("spawn cargo failed: {err}")))?;
    wait_for_cargo_child(&mut child, "cargo", timeout, outer_target)
}

/// Run cargo with both streams tee'd to the user's stdout/stderr AND
/// stderr accumulated into a [`String`] for post-failure scanning by
/// [`crate::cargo_diagnostics`]. Stdout is NOT buffered — we only need
/// stderr for diagnosis, and cargo can emit megabytes of compile
/// progress to stdout that would just sit unused in RAM.
///
/// Used in the non-clippy, non-TTY branch of `run_cargo_front_door`
/// (#422): when stderr is piped to a CI log / Docker stream / file,
/// cargo's progress-bar UX is already gone, so the extra
/// pipe-and-tee doesn't degrade interactive output.
fn run_command_capturing_diagnostic_tail(
    command: &mut std::process::Command,
    timeout: Option<Duration>,
    outer_target: Option<&Path>,
) -> Result<(std::process::ExitStatus, String), SoldrError> {
    command.stderr(std::process::Stdio::piped());
    // stdout stays inherited — we don't need its bytes.
    configure_cargo_child_for_timeout(command);
    let mut child = debug_trace::spawn_traced(command, "cargo diagnostic capture").map_err(|err| {
        SoldrError::Other(format!("spawn cargo for diagnostic capture failed: {err}"))
    })?;
    // soldr#2546 slice 3: same post-hoc descendant attach as JSON capture.
    let observation =
        debug_trace::DescendantObservation::attach(child.id(), "cargo diagnostic capture");
    let child_stderr = child.stderr.take().expect("piped");

    let stderr_rx = spawn_capture_pipe_reader(
        child_stderr,
        line_stamp_anchor(std::io::IsTerminal::is_terminal(&std::io::stderr())),
    );

    let status = wait_for_cargo_child(
        &mut child,
        "cargo diagnostic capture",
        timeout,
        outer_target,
    )?;
    if let Some(observation) = observation {
        observation.finish();
    }
    let bytes = drain_capture_pipe_after_child_exit(&stderr_rx, "cargo diagnostic stderr");
    let captured = String::from_utf8_lossy(&bytes).into_owned();
    Ok((status, captured))
}

enum CapturePipeMessage {
    Chunk(Vec<u8>),
    Eof,
}

fn spawn_capture_pipe_reader<R>(
    mut reader: R,
    stamp_from: Option<Instant>,
) -> std::sync::mpsc::Receiver<CapturePipeMessage>
where
    R: std::io::Read + Send + 'static,
{
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        // soldr#1802: stamped copy to the terminal, raw copy on the
        // channel — the diagnostic scanner matches cargo's exact bytes.
        let mut stamped =
            stamp_from.map(|t0| timestamp_tee::TimestampedTee::new(std::io::stderr(), t0));
        let mut chunk = [0u8; 8192];
        loop {
            match reader.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    let bytes = chunk[..n].to_vec();
                    match stamped.as_mut() {
                        Some(tee) => {
                            let _ = tee.write_all(&bytes);
                            let _ = tee.flush();
                        }
                        None => {
                            let stderr = std::io::stderr();
                            let _ = stderr.lock().write_all(&bytes);
                        }
                    }
                    let _ = tx.send(CapturePipeMessage::Chunk(bytes));
                }
                Err(_) => break,
            }
        }
        let stderr = std::io::stderr();
        let _ = stderr.lock().flush();
        let _ = tx.send(CapturePipeMessage::Eof);
    });
    rx
}

fn drain_capture_pipe_after_child_exit(
    rx: &std::sync::mpsc::Receiver<CapturePipeMessage>,
    context: &str,
) -> Vec<u8> {
    let deadline = Instant::now() + CAPTURE_PIPE_EOF_GRACE;
    let mut buf = Vec::new();
    loop {
        match rx.try_recv() {
            Ok(CapturePipeMessage::Chunk(bytes)) => {
                buf.extend_from_slice(&bytes);
                continue;
            }
            Ok(CapturePipeMessage::Eof) | Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                return buf;
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
        }

        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            eprintln!(
                "soldr: {context} pipe did not close within {}ms after cargo exited; \
                 continuing with captured output",
                CAPTURE_PIPE_EOF_GRACE.as_millis()
            );
            return buf;
        };
        match rx.recv_timeout(remaining) {
            Ok(CapturePipeMessage::Chunk(bytes)) => buf.extend_from_slice(&bytes),
            Ok(CapturePipeMessage::Eof) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                return buf;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                eprintln!(
                    "soldr: {context} pipe did not close within {}ms after cargo exited; \
                     continuing with captured output",
                    CAPTURE_PIPE_EOF_GRACE.as_millis()
                );
                return buf;
            }
        }
    }
}

// Decide whether the "did you mean: cargo X?" hint applies to a typed
// subcommand that isn't in `known_tools`. Returns `Some(suggestion)`
// only when `sub` looks like a typo of a registered cargo subcommand
// AND is not itself a legitimate cargo built-in verb.
//
// Issue #755: without the built-in guard, `soldr cargo check` falsely
// suggested `cargo chef` (Levenshtein distance 2). Built-in verbs are
// routed through the External arm by `cargo` itself; treating them as
// typos contradicts the contract documented in `CARGO_BUILTIN_VERBS`.
