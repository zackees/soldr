//! soldr#2388: container-safe broker/session socket identity.
//!
//! running-process derives its per-user/per-machine identity from
//! `/etc/machine-id` (or `/var/lib/dbus/machine-id`) and **hard-errors** when
//! neither exists — not just for the socket-name derivation soldr drives, but
//! *internally* in `serve_launching_backends` too. Minimal containers
//! (distroless, scratch-ish, some CI images) ship neither. Since Step 4 the
//! broker is mandatory for **every** compile, so a missing machine-id would
//! mean *soldr cannot compile at all* there.
//!
//! The fix must cover running-process's internal uses, not only soldr's call
//! sites, so [`ensure_machine_id`] **materializes the machine-id file itself**
//! (best-effort) before any identity is derived. Every running-process path
//! then reads the same value. [`resolve_user_sid`] additionally keeps a
//! deterministic in-memory fallback for the socket name in the rare case the
//! file could not be written (read-only `/etc` and `/var/lib/dbus`).

/// Materialize a machine-id file if the OS provides none, so running-process's
/// identity derivation (soldr's socket names AND `serve_launching_backends`'
/// internals) succeeds. Idempotent and best-effort: a pre-existing id is left
/// untouched, and an unwritable environment is simply left as-is.
pub(crate) fn ensure_machine_id() {
    const PATHS: &[&str] = &["/etc/machine-id", "/var/lib/dbus/machine-id"];
    // Already provided by the OS (systemd, dbus, a prior call) — nothing to do.
    if PATHS.iter().any(|p| non_empty_file(p)) {
        return;
    }
    let id = machine_id_value();
    // Prefer /etc/machine-id; fall back to /var/lib/dbus/machine-id (creating
    // its parent). running-process reads both, so either is sufficient.
    if std::fs::write("/etc/machine-id", format!("{id}\n")).is_ok() {
        return;
    }
    let dbus = std::path::Path::new("/var/lib/dbus/machine-id");
    if let Some(parent) = dbus.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(dbus, format!("{id}\n"));
}

/// Resolve the 16-hex per-user socket identity used to derive broker + SESSION
/// socket names. Ensures a machine-id exists first (so running-process's own
/// internals also work), then uses running-process's native derivation; only
/// if that still fails does it fall back to a deterministic value so the broker
/// and its clients at least agree on the socket name.
pub(crate) fn resolve_user_sid() -> String {
    use running_process::broker::lifecycle::sid::{hash_to_16_hex, user_sid_hash};
    ensure_machine_id();
    match user_sid_hash() {
        Ok(sid) => sid,
        Err(_) => hash_to_16_hex(format!("{}:{}", current_uid(), fallback_machine_id()).as_bytes()),
    }
}

/// A valid 32-hex machine-id. `boot_id` (a kernel-provided UUID, present in
/// virtually every Linux container via `/proc`) with dashes stripped is exactly
/// that; otherwise derive a stable one from uid + `$HOME`.
fn machine_id_value() -> String {
    if let Some(boot) = read_trimmed("/proc/sys/kernel/random/boot_id") {
        let hex: String = boot
            .chars()
            .filter(|c| c.is_ascii_hexdigit())
            .collect::<String>()
            .to_ascii_lowercase();
        if hex.len() >= 32 {
            return hex[..32].to_string();
        }
    }
    // Deterministic 32-hex from two 16-hex halves of running-process's hash.
    use running_process::broker::lifecycle::sid::hash_to_16_hex;
    let seed = format!("{}:{}", current_uid(), fallback_machine_id());
    let a = hash_to_16_hex(seed.as_bytes());
    let b = hash_to_16_hex(format!("{seed}:2").as_bytes());
    format!("{a}{b}")
}

fn fallback_machine_id() -> String {
    read_trimmed("/proc/sys/kernel/random/boot_id")
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("HOME").ok().filter(|s| !s.is_empty()))
        .unwrap_or_else(|| "soldr-container-fallback".to_string())
}

fn non_empty_file(path: &str) -> bool {
    std::fs::read_to_string(path)
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false)
}

fn read_trimmed(path: &str) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
}

fn current_uid() -> String {
    #[cfg(unix)]
    {
        (unsafe { libc::getuid() }).to_string()
    }
    #[cfg(not(unix))]
    {
        std::env::var("USERNAME").unwrap_or_else(|_| "user".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    crate::timed_test!(resolve_user_sid_is_nonempty_and_stable, {
        let a = resolve_user_sid();
        let b = resolve_user_sid();
        assert!(!a.is_empty());
        assert_eq!(a, b, "socket identity must be stable across calls");
    });

    crate::timed_test!(machine_id_value_is_32_hex, {
        let id = machine_id_value();
        assert_eq!(id.len(), 32, "machine-id must be 32 chars: {id}");
        assert!(
            id.chars().all(|c| c.is_ascii_hexdigit()),
            "machine-id must be hex: {id}"
        );
    });
}
