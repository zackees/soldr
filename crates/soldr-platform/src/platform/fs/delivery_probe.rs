//! Cache-to-target delivery capability probe (soldr#3440, zccache#1792).
//!
//! zccache's own `AUTO` delivery mode now means reflink, else an
//! independent copy — it never hardlinks (zccache#1791). Soldr wants the
//! explicit, cheapest-available mechanism instead of letting zccache retry
//! a chain per cache hit, so it probes **once**, before the build starts,
//! whether the pair of directories a cache hit actually moves between (the
//! zccache cache dir and the cargo target dir) supports a copy-on-write
//! clone or, failing that, a hardlink. A cross-volume pair can do neither,
//! in which case an independent copy is the only option.
//!
//! The probe performs real, tiny filesystem operations rather than reading
//! filesystem-type feature flags: those vary release to release and are
//! easy to get wrong, while attempting the operation and observing whether
//! it succeeds is always correct. Reflinking goes through
//! `kernal_api::platform::fs::reflink_file` — already cross-platform (Linux
//! `ioctl_ficlone`, macOS `clonefile`, Windows
//! `FSCTL_DUPLICATE_EXTENTS_TO_FILE`) — and hardlinking through
//! `std::fs::hard_link`, itself a portable std primitive, so this module
//! adds no host `cfg` of its own.

use kernal_api::platform::fs::reflink_file;
use std::path::Path;

/// Which materialization mechanism a `(cache_dir, target_dir)` pair
/// supports, in the order zccache's old `AUTO` tried them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryCapability {
    /// The pair supports a copy-on-write clone.
    Reflink,
    /// The pair does not reflink but does support a hardlink.
    Link,
    /// Neither a reflink nor a hardlink is possible (e.g. cross-volume);
    /// an independent copy is the only option. Also the safe fallback when
    /// the probe itself could not run (permission error, missing
    /// directory that could not be created, ...).
    Copy,
}

/// Probe whether a cache hit can move from `cache_dir` to `target_dir` by
/// reflink, hardlink, or only by copy. Creates both directories if they do
/// not exist yet. Always removes every probe file it creates, regardless
/// of the outcome.
///
/// A probe-level error (the directories could not be created, the source
/// probe file could not be written, ...) reports [`DeliveryCapability::Copy`]
/// -- **not** because copy is what actually works, but because it is the
/// conservative choice: overclaiming `Reflink` or `Link` here would let an
/// inconclusive probe silently produce a shared-inode or shared-extent
/// output when soldr could not actually verify one is safe.
pub fn probe_delivery_capability(cache_dir: &Path, target_dir: &Path) -> DeliveryCapability {
    probe_delivery_capability_inner(cache_dir, target_dir).unwrap_or(DeliveryCapability::Copy)
}

fn probe_delivery_capability_inner(
    cache_dir: &Path,
    target_dir: &Path,
) -> std::io::Result<DeliveryCapability> {
    std::fs::create_dir_all(cache_dir)?;
    std::fs::create_dir_all(target_dir)?;

    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let source = cache_dir.join(format!(".soldr-mode-probe-{pid}-{nanos}.src"));
    let reflink_dest = target_dir.join(format!(".soldr-mode-probe-{pid}-{nanos}.reflink"));
    let hardlink_dest = target_dir.join(format!(".soldr-mode-probe-{pid}-{nanos}.hardlink"));

    // Best-effort: a stale probe file from a killed prior run should not
    // make this probe report a false negative.
    let _ = std::fs::remove_file(&source);
    let _ = std::fs::remove_file(&reflink_dest);
    let _ = std::fs::remove_file(&hardlink_dest);

    std::fs::write(&source, b"soldr zccache delivery-mode probe")?;

    let reflink_ok = reflink_file(&source, &reflink_dest).is_ok();
    let hardlink_ok = !reflink_ok && std::fs::hard_link(&source, &hardlink_dest).is_ok();

    let _ = std::fs::remove_file(&source);
    let _ = std::fs::remove_file(&reflink_dest);
    let _ = std::fs::remove_file(&hardlink_dest);

    Ok(if reflink_ok {
        DeliveryCapability::Reflink
    } else if hardlink_ok {
        DeliveryCapability::Link
    } else {
        DeliveryCapability::Copy
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_temp_dir(label: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("soldr-{label}-{}-{nanos}", std::process::id()))
    }

    #[test]
    fn probe_leaves_no_files_behind() {
        let cache_dir = unique_temp_dir("delivery-probe-cache");
        let target_dir = unique_temp_dir("delivery-probe-target");
        // Whatever this host's filesystem supports, the probe must report a
        // capability and clean up after itself -- never panic, never leak.
        let _ = probe_delivery_capability(&cache_dir, &target_dir);
        for dir in [&cache_dir, &target_dir] {
            let leftovers: Vec<_> = std::fs::read_dir(dir)
                .map(|entries| entries.filter_map(Result::ok).collect())
                .unwrap_or_default();
            assert!(
                leftovers.is_empty(),
                "delivery probe left files behind in {}: {leftovers:?}",
                dir.display()
            );
        }
        let _ = std::fs::remove_dir(&cache_dir);
        let _ = std::fs::remove_dir(&target_dir);
    }

    #[test]
    fn probe_creates_missing_directories() {
        let cache_dir = unique_temp_dir("delivery-probe-mkdir-cache");
        let target_dir = unique_temp_dir("delivery-probe-mkdir-target");
        assert!(!cache_dir.exists());
        assert!(!target_dir.exists());
        let _ = probe_delivery_capability(&cache_dir, &target_dir);
        assert!(cache_dir.exists());
        assert!(target_dir.exists());
        let _ = std::fs::remove_dir(&cache_dir);
        let _ = std::fs::remove_dir(&target_dir);
    }

    #[test]
    fn probe_on_the_same_directory_is_at_least_link_capable() {
        // Same directory means same volume, so a hardlink (at minimum) must
        // succeed on every platform this runs on -- this pins down that the
        // probe does not systematically under-report.
        let dir = unique_temp_dir("delivery-probe-same-dir");
        let capability = probe_delivery_capability(&dir, &dir);
        assert_ne!(
            capability,
            DeliveryCapability::Copy,
            "same-directory pair must support at least a hardlink"
        );
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn probe_is_idempotent_across_repeated_calls() {
        let cache_dir = unique_temp_dir("delivery-probe-repeat-cache");
        let target_dir = unique_temp_dir("delivery-probe-repeat-target");
        let first = probe_delivery_capability(&cache_dir, &target_dir);
        let second = probe_delivery_capability(&cache_dir, &target_dir);
        assert_eq!(
            first, second,
            "the same volume pair must answer consistently"
        );
        let _ = std::fs::remove_dir(&cache_dir);
        let _ = std::fs::remove_dir(&target_dir);
    }
}
