//! Embed packed Darwin DWARF into the final Mach-O artifact.
//!
//! Rust's `split-debuginfo = "packed"` leaves the complete DWARF in a dSYM
//! bundle beside the linked artifact.  Keeping that bundle is useful for
//! native tooling, but remote debuggers often receive only the executable.
//! This module copies the standard DWARF sections into a read-only `__DWARF`
//! segment using the managed LLVM `llvm-objcopy` already placed on the child
//! PATH by blessed Darwin preparation.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::core::SoldrError;

const DWARF_SECTIONS: &[&str] = &[
    "__debug_abbrev",
    "__debug_addr",
    "__debug_aranges",
    "__debug_frame",
    "__debug_info",
    "__debug_line",
    "__debug_line_str",
    "__debug_loc",
    "__debug_loclists",
    "__debug_names",
    "__debug_rnglists",
    "__debug_str",
    "__debug_str_offsets",
    "__debug_sup",
];

/// Embed dSYM sections for any compiler artifacts in `relative_paths` that
/// have a sibling dSYM bundle.  Artifacts without a dSYM are untouched.
pub(super) fn embed_packed_dwarf_for_artifacts(
    target_dir: Option<&Path>,
    relative_paths: &[String],
) -> Result<(), SoldrError> {
    let Some(target_dir) = target_dir else {
        return Ok(());
    };
    for relative in relative_paths {
        if relative.to_ascii_lowercase().contains(".dsym") {
            continue;
        }
        let artifact = target_dir.join(relative);
        if !artifact.is_file() {
            continue;
        }
        let Some(bundle) = find_dsym_bundle(&artifact) else {
            continue;
        };
        embed_one(&artifact, &bundle)?;
    }
    Ok(())
}

fn find_dsym_bundle(artifact: &Path) -> Option<PathBuf> {
    let parent = artifact.parent()?;
    let name = artifact.file_name()?.to_string_lossy();
    let candidates = [
        parent.join(format!("{name}.dSYM")),
        artifact.with_extension("dSYM"),
    ];
    candidates.into_iter().find(|path| {
        path.join("Contents")
            .join("Resources")
            .join("DWARF")
            .is_dir()
    })
}

fn embed_one(artifact: &Path, bundle: &Path) -> Result<(), SoldrError> {
    let dwarf_dir = bundle.join("Contents").join("Resources").join("DWARF");
    let expected = dwarf_dir.join(artifact.file_name().unwrap_or_default());
    let dwarf_file = if expected.is_file() {
        expected
    } else {
        let mut payloads = std::fs::read_dir(&dwarf_dir)
            .map_err(|error| SoldrError::Other(format!("unable to read dSYM payload: {error}")))?
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.is_file());
        let Some(single) = payloads.next() else {
            return Err(SoldrError::Other(format!(
                "dSYM bundle {} has no DWARF payload for {}",
                bundle.display(),
                artifact.display()
            )));
        };
        if payloads.next().is_some() {
            return Err(SoldrError::Other(format!(
                "dSYM bundle {} has no uniquely matching DWARF payload for {}",
                bundle.display(),
                artifact.display()
            )));
        }
        single
    };

    let temp = tempfile::tempdir().map_err(|error| {
        SoldrError::Other(format!("unable to create dSYM embedding temp dir: {error}"))
    })?;
    let probe = temp.path().join("probe");
    if dump_section(Path::new("llvm-objcopy"), artifact, "__debug_info", &probe).is_ok() {
        // Cache restores and repeated builds can revisit an already embedded
        // artifact.  Treat the existing section as an idempotent success.
        return Ok(());
    }

    let sections = collect_dwarf_sections(Path::new("llvm-objcopy"), &dwarf_file, temp.path())?;

    let staged = temp.path().join("artifact");
    std::fs::copy(artifact, &staged).map_err(|error| {
        SoldrError::Other(format!(
            "failed to stage Mach-O artifact {}: {error}",
            artifact.display()
        ))
    })?;
    for (section, payload) in &sections {
        let spec = format!("__DWARF,{section}={}", payload.display());
        let mut command = Command::new("llvm-objcopy");
        command.args(["--add-section", &spec]).arg(&staged);
        crate::core::tool_output::run_small_tool(&mut command, "llvm-objcopy --add-section")
            .map_err(|error| {
                SoldrError::Other(format!(
                    "llvm-objcopy failed embedding {section} into {}: {error}",
                    artifact.display()
                ))
            })?;
    }

    let backup = artifact.with_extension(format!("soldr-embed-{}", std::process::id()));
    std::fs::rename(artifact, &backup).map_err(|error| {
        SoldrError::Other(format!(
            "failed to stage replacement for {}: {error}",
            artifact.display()
        ))
    })?;
    if let Err(error) = std::fs::rename(&staged, artifact) {
        let _ = std::fs::rename(&backup, artifact);
        return Err(SoldrError::Other(format!(
            "failed to promote embedded artifact {}: {error}",
            artifact.display()
        )));
    }
    let _ = std::fs::remove_file(backup);
    Ok(())
}

/// Dump every supported DWARF section of `dwarf_file` into `dir`. When none
/// can be dumped, the error lists each section's own llvm-objcopy failure
/// instead of a bare "no supported sections" (soldr#3384).
fn collect_dwarf_sections(
    objcopy: &Path,
    dwarf_file: &Path,
    dir: &Path,
) -> Result<Vec<(&'static str, PathBuf)>, SoldrError> {
    let mut sections = Vec::new();
    let mut failures = Vec::new();
    for section in DWARF_SECTIONS {
        let output = dir.join(section.trim_start_matches('_'));
        match dump_section(objcopy, dwarf_file, section, &output) {
            Ok(()) if output.is_file() => sections.push((*section, output)),
            Ok(()) => failures.push(format!("{section}: llvm-objcopy wrote no output file")),
            Err(error) => failures.push(format!("{section}: {error}")),
        }
    }
    if sections.is_empty() {
        return Err(SoldrError::Other(format!(
            "dSYM payload {} contains no supported DWARF sections; per-section failures: {}",
            dwarf_file.display(),
            failures.join("; ")
        )));
    }
    Ok(sections)
}

fn dump_section(
    objcopy: &Path,
    input: &Path,
    section: &str,
    output: &Path,
) -> Result<(), SoldrError> {
    let spec = format!("__DWARF,{section}={}", output.display());
    let mut command = Command::new(objcopy);
    command.args(["--dump-section", &spec]).arg(input);
    crate::core::tool_output::run_small_tool(&mut command, "llvm-objcopy --dump-section")
        .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// soldr#3384: when every section dump fails, the error names each
    /// section's own llvm-objcopy stderr.
    #[test]
    fn all_section_dumps_failing_lists_each_stderr() {
        let tmp = tempfile::tempdir().expect("temp");
        let objcopy = crate::core::tool_output::write_fake_tool(
            tmp.path(),
            "llvm-objcopy",
            "",
            "MARKER_OBJCOPY_3384",
            1,
        );
        let dwarf = tmp.path().join("payload");
        std::fs::write(&dwarf, b"x").expect("payload");
        let err = collect_dwarf_sections(&objcopy, &dwarf, tmp.path())
            .unwrap_err()
            .to_string();
        assert!(err.contains("no supported DWARF sections"), "{err}");
        for section in DWARF_SECTIONS {
            assert!(
                err.contains(&format!("{section}: ")),
                "{section} missing: {err}"
            );
        }
        assert!(
            err.matches("MARKER_OBJCOPY_3384").count() >= DWARF_SECTIONS.len(),
            "{err}"
        );
    }

    #[test]
    fn artifact_without_dsym_is_unchanged() {
        let tmp = tempfile::tempdir().expect("temp");
        let target = tmp.path().join("target");
        std::fs::create_dir_all(&target).expect("target");
        let artifact = target.join("app");
        std::fs::write(&artifact, b"not a Mach-O").expect("artifact");
        let before = std::fs::read(&artifact).expect("read");
        embed_packed_dwarf_for_artifacts(Some(&target), &["app".into()]).expect("no-op");
        assert_eq!(std::fs::read(&artifact).expect("read"), before);
    }

    #[test]
    fn finds_both_dsym_naming_conventions() {
        let tmp = tempfile::tempdir().expect("temp");
        let artifact = tmp.path().join("app");
        std::fs::write(&artifact, b"artifact").expect("artifact");
        let first = tmp.path().join("app.dSYM/Contents/Resources/DWARF");
        std::fs::create_dir_all(&first).expect("bundle");
        assert_eq!(
            find_dsym_bundle(&artifact),
            Some(tmp.path().join("app.dSYM"))
        );
    }
}
