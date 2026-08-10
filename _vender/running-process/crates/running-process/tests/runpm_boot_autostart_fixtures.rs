#![cfg(feature = "client")]
//! Fixture tests for [`running_process::boot_autostart::render_unit`]
//! (Phase 4 of #222 — #427).
//!
//! These tests deliberately avoid calling `install` / `uninstall` so the
//! runner's actual init system is never touched. They only assert on the
//! rendered unit/plist/task contents — shape, required fields, and
//! shell-safety of an arbitrary daemon-binary path. This is enough to
//! catch a regression in how we template paths into a service definition.

use std::path::PathBuf;

use running_process::boot_autostart::render_unit;

// ---------------------------------------------------------------------------
// Linux: systemd user unit
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
#[test]
fn linux_unit_contains_expected_systemd_fields() {
    let daemon = PathBuf::from("/usr/local/bin/running-process-daemon");
    let unit = render_unit(&daemon);

    assert!(unit.contains("[Unit]"), "missing [Unit] section: {unit}");
    assert!(
        unit.contains("[Service]"),
        "missing [Service] section: {unit}"
    );
    assert!(
        unit.contains("[Install]"),
        "missing [Install] section: {unit}"
    );
    assert!(
        unit.contains("Description=runpm process supervisor"),
        "missing Description: {unit}"
    );
    assert!(unit.contains("Type=simple"), "missing Type=simple: {unit}");
    assert!(
        unit.contains("Restart=on-failure"),
        "missing Restart=on-failure: {unit}"
    );
    assert!(
        unit.contains("RestartSec=5"),
        "missing RestartSec=5: {unit}"
    );
    assert!(
        unit.contains("WantedBy=default.target"),
        "missing WantedBy=default.target: {unit}"
    );
    // The daemon binary path appears in an `ExecStart` directive, wrapped
    // in single quotes so embedded spaces don't break the unit parser.
    assert!(
        unit.contains("ExecStart='/usr/local/bin/running-process-daemon' start"),
        "missing or unquoted ExecStart: {unit}"
    );
    assert!(
        unit.contains("ExecStop='/usr/local/bin/running-process-daemon' stop"),
        "missing or unquoted ExecStop: {unit}"
    );
}

// ---------------------------------------------------------------------------
// macOS: launchd LaunchAgent plist
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
#[test]
fn macos_plist_contains_expected_launchd_keys() {
    let daemon = PathBuf::from("/usr/local/bin/running-process-daemon");
    let plist = render_unit(&daemon);

    assert!(
        plist.contains("<?xml"),
        "plist must declare XML header: {plist}"
    );
    assert!(
        plist.contains("<plist version=\"1.0\">"),
        "plist must declare plist version: {plist}"
    );
    assert!(
        plist.contains("<key>Label</key>"),
        "plist must contain Label key"
    );
    assert!(
        plist.contains("<string>com.zackees.runpm-daemon</string>"),
        "plist must use the canonical reverse-DNS label"
    );
    assert!(
        plist.contains("<key>ProgramArguments</key>"),
        "plist must contain ProgramArguments key"
    );
    assert!(
        plist.contains("<string>/usr/local/bin/running-process-daemon</string>"),
        "plist must reference the daemon binary path"
    );
    assert!(
        plist.contains("<string>start</string>"),
        "plist must pass `start` to the daemon"
    );
    assert!(
        plist.contains("<key>RunAtLoad</key>") && plist.contains("<true/>"),
        "plist must set RunAtLoad=true"
    );
    assert!(
        plist.contains("<key>KeepAlive</key>"),
        "plist must set KeepAlive"
    );
}

// ---------------------------------------------------------------------------
// Windows: schtasks ONLOGON task
// ---------------------------------------------------------------------------

#[cfg(target_os = "windows")]
#[test]
fn windows_unit_is_schtasks_invocation_with_correct_flags() {
    let daemon = PathBuf::from(r"C:\Program Files\running-process\running-process-daemon.exe");
    let unit = render_unit(&daemon);

    assert!(
        unit.starts_with("schtasks /Create"),
        "must invoke schtasks /Create: {unit}"
    );
    assert!(unit.contains("/SC ONLOGON"), "must use /SC ONLOGON: {unit}");
    assert!(
        unit.contains("/TN \"runpm-daemon\""),
        "must use the canonical task name: {unit}"
    );
    assert!(
        unit.contains("/RL HIGHEST"),
        "must request HIGHEST RL: {unit}"
    );
    assert!(unit.contains("/F"), "must force-overwrite with /F: {unit}");
    // /TR receives the quoted daemon command plus "start".
    assert!(
        unit.contains(r#""C:\Program Files\running-process\running-process-daemon.exe start""#),
        "must include the daemon path and the start arg inside /TR: {unit}"
    );
}

// ---------------------------------------------------------------------------
// Cross-platform: paths with spaces must be quoted safely.
// ---------------------------------------------------------------------------

#[test]
fn render_unit_quotes_path_with_spaces_safely() {
    // Pick a path-with-spaces appropriate for the current OS. On Linux/macOS
    // the rendered output uses single quotes; on Windows it uses doubled
    // quotes inside a `"..."` group.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    let daemon = PathBuf::from("/opt/Program Files/running-process-daemon");
    #[cfg(target_os = "windows")]
    let daemon = PathBuf::from(r"C:\Program Files\rp space\running-process-daemon.exe");
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    let daemon = PathBuf::from("/tmp/running-process-daemon");

    let unit = render_unit(&daemon);

    // The literal path string must appear in the output.
    assert!(
        unit.contains(&daemon.to_string_lossy().into_owned()),
        "expected daemon path verbatim in rendered unit:\n{unit}"
    );

    // Linux: single-quoted ExecStart with no naked space in the
    // ExecStart argument list.
    #[cfg(target_os = "linux")]
    {
        assert!(
            unit.contains("ExecStart='/opt/Program Files/running-process-daemon' start"),
            "linux ExecStart must single-quote the path: {unit}"
        );
        // Cheap heuristic: no double-quotes anywhere in the unit body —
        // we only emit single quotes around shell-injectable values.
        assert!(
            !unit.contains('"'),
            "linux unit must not use double quotes: {unit}"
        );
    }

    // macOS: each argv string lives inside a <string> tag; nothing
    // needs shell quoting (launchd is not shell-mediated). What matters
    // is that the daemon path appears verbatim and the binary string
    // is not split across tags.
    #[cfg(target_os = "macos")]
    {
        let needle = format!(
            "<string>{}</string>",
            "/opt/Program Files/running-process-daemon"
        );
        assert!(
            unit.contains(&needle),
            "macos plist must wrap the daemon path in a single <string>: {unit}"
        );
    }

    // Windows: the daemon path lives inside `/TR "..."` with embedded
    // spaces in the path being legal inside CMD's outer quotes.
    #[cfg(target_os = "windows")]
    {
        assert!(
            unit.contains(r#""C:\Program Files\rp space\running-process-daemon.exe start""#),
            "windows /TR must wrap the full command in outer quotes: {unit}"
        );
    }
}
