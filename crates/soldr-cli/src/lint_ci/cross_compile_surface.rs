//! `cross-compile-surface` rule (soldr#2038).
//!
//! Enforces that Apple Darwin and Windows MSVC builds go through the blessed
//! `soldr build --target ...` surface (with `soldr prepare` allowed) and that
//! CI does not directly invoke a legacy cross wrapper (`cargo xwin`,
//! `cargo zigbuild`), a raw cross compiler (`zig cc`, mingw, osxcross, or a
//! `clang`/`gcc` carrying an Apple/Windows `--target`) for those targets.
//!
//! Zig / cargo-zigbuild remain **allowed** for `*-unknown-linux-*` and
//! manylinux builds — the rule is target-aware, so a legitimate Linux-Zig
//! matrix row never masks an invalid Apple/Windows row.

use super::model::{Finding, Severity};
use super::registry::CiRule;
use super::scan::{classify_triple, tokenize, LogicalLine, ScannedFile, TargetKind};

pub const RULE_ID: &str = "cross-compile-surface";

pub struct CrossCompileSurface;

impl CiRule for CrossCompileSurface {
    fn id(&self) -> &'static str {
        RULE_ID
    }

    fn check(&self, files: &[ScannedFile]) -> Vec<Finding> {
        let mut findings = Vec::new();
        for file in files {
            for line in &file.lines {
                if line.is_comment || !line.is_command {
                    continue;
                }
                findings.extend(check_line(file, line));
            }
        }
        findings
    }
}

/// The non-blessed tools the rule detects.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ToolKind {
    /// `cargo xwin` / `cargo-xwin` — Windows-MSVC-only cross wrapper.
    Xwin,
    /// `cargo zigbuild` / `cargo-zigbuild` — cross wrapper (Linux-legit).
    Zigbuild,
    /// `zig cc` / `zig c++` / `zig build-exe` used as a compiler/linker.
    ZigCc,
    /// A `*-w64-mingw32-*` cross compiler — implies Windows GNU.
    Mingw,
    /// An osxcross `o64-clang` / `*-apple-darwin*-clang` — implies Apple.
    OsxCross,
    /// A generic `clang`/`gcc`/`cc` carrying an Apple/Windows `--target`.
    AltCompiler,
}

impl ToolKind {
    fn display(self) -> &'static str {
        match self {
            ToolKind::Xwin => "cargo xwin",
            ToolKind::Zigbuild => "cargo zigbuild",
            ToolKind::ZigCc => "zig",
            ToolKind::Mingw => "mingw cross compiler",
            ToolKind::OsxCross => "osxcross clang",
            ToolKind::AltCompiler => "non-blessed cross compiler",
        }
    }
}

fn check_line(file: &ScannedFile, line: &LogicalLine) -> Vec<Finding> {
    let code = &line.tool_code;
    // Blessed short-circuit: a `soldr build` / `soldr prepare` command is
    // always compliant, even if it names a legacy tool elsewhere.
    if contains_word_pair(code, "soldr", "build") || contains_word_pair(code, "soldr", "prepare") {
        return Vec::new();
    }

    let tool = match detect_tool(code) {
        Some(tool) => tool,
        None => return Vec::new(),
    };

    let targets = extract_targets(&line.original);
    let outcomes = evaluate(tool, &targets, file);

    outcomes
        .into_iter()
        .map(|(severity, target)| Finding {
            rule: RULE_ID.to_string(),
            severity,
            file: file.rel_path.clone(),
            line: line.line,
            tool: tool.display().to_string(),
            recommendation: recommendation(tool, &target),
            target,
        })
        .collect()
}

/// Detect the single highest-priority non-blessed tool on a command line.
///
/// Detection is token-oriented so that a tool *name* mentioned as data — a
/// cache key (`...-cargo-zigbuild-0.23.0`), a `find -name cargo-zigbuild`
/// argument, or a `--json cargo-zigbuild` query arg — is not mistaken for an
/// invocation. The `cargo <sub>` form must be two adjacent tokens, and the
/// `cargo-<sub>` executable form must sit at a command position (its previous
/// token is not a flag).
fn detect_tool(code: &str) -> Option<ToolKind> {
    let toks: Vec<&str> = code.split_whitespace().collect();

    // `cargo xwin` / `cargo zigbuild` — adjacent subcommand tokens.
    for w in toks.windows(2) {
        if w[0] == "cargo" && w[1] == "xwin" {
            return Some(ToolKind::Xwin);
        }
        if w[0] == "cargo" && w[1] == "zigbuild" {
            return Some(ToolKind::Zigbuild);
        }
    }

    // `cargo-xwin` / `cargo-zigbuild` executable at a command position.
    for (i, tok) in toks.iter().enumerate() {
        let prev_is_flag = i > 0 && toks[i - 1].starts_with('-');
        if prev_is_flag {
            continue;
        }
        let normalized = tok.trim_start_matches("./");
        if normalized == "cargo-xwin" {
            return Some(ToolKind::Xwin);
        }
        if normalized == "cargo-zigbuild" {
            return Some(ToolKind::Zigbuild);
        }
    }

    // `zig cc` / `zig c++` / `zig build-exe` — adjacent tokens.
    for w in toks.windows(2) {
        if w[0] == "zig" && matches!(w[1], "cc" | "c++" | "build-exe") {
            return Some(ToolKind::ZigCc);
        }
    }

    let owned: Vec<String> = tokenize(code).collect();
    if owned.iter().any(|t| t.contains("-w64-mingw32-")) {
        return Some(ToolKind::Mingw);
    }
    if owned
        .iter()
        .any(|t| t == "o64-clang" || t == "oa64-clang" || is_osxcross_clang(t))
    {
        return Some(ToolKind::OsxCross);
    }
    if owned.iter().any(|t| is_generic_compiler(t)) {
        return Some(ToolKind::AltCompiler);
    }
    None
}

fn is_osxcross_clang(token: &str) -> bool {
    token.contains("-apple-darwin") && (token.ends_with("-clang") || token.ends_with("-gcc"))
}

fn is_generic_compiler(token: &str) -> bool {
    matches!(
        token,
        "clang" | "clang++" | "clang-cl" | "gcc" | "g++" | "cc" | "c++"
    )
}

/// Parse `--target <value>` / `--target=<value>` occurrences.
fn extract_targets(original: &str) -> Vec<(String, TargetKind)> {
    let toks: Vec<String> = tokenize(original).collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < toks.len() {
        let tok = &toks[i];
        if tok == "--target" {
            if let Some(value) = toks.get(i + 1) {
                out.push((value.clone(), classify_triple(value)));
            }
            i += 2;
            continue;
        }
        if let Some(value) = tok.strip_prefix("--target=") {
            out.push((value.to_string(), classify_triple(value)));
        }
        i += 1;
    }
    out
}

/// Resolve severity + display target for a detected tool.
fn evaluate(
    tool: ToolKind,
    targets: &[(String, TargetKind)],
    file: &ScannedFile,
) -> Vec<(Severity, String)> {
    let has_placeholder = targets.iter().any(|(_, k)| *k == TargetKind::Placeholder);

    // Concrete targets on the line, plus matrix expansion when the line
    // references a matrix/var placeholder.
    let mut concrete: Vec<(String, TargetKind)> = targets
        .iter()
        .filter(|(_, k)| *k != TargetKind::Placeholder)
        .cloned()
        .collect();
    if has_placeholder {
        for mt in &file.matrix_targets {
            concrete.push((mt.clone(), classify_triple(mt)));
        }
    }

    // xwin/mingw are Windows-only wrappers, so they also reject windows-gnu.
    let flag_windows_gnu = matches!(tool, ToolKind::Xwin | ToolKind::Mingw);

    let mut flagged: Vec<(Severity, String)> = Vec::new();
    for (raw, kind) in &concrete {
        let is_flag = match kind {
            TargetKind::Apple | TargetKind::WindowsMsvc => true,
            TargetKind::WindowsGnu => flag_windows_gnu,
            _ => false,
        };
        if is_flag {
            flagged.push((Severity::Error, raw.clone()));
        }
    }
    dedup_outcomes(&mut flagged);
    if !flagged.is_empty() {
        return flagged;
    }

    // No concrete Apple/Windows target was flagged.
    match tool {
        // Windows/Apple-only tools are violations even without an explicit
        // `--target` (the tool itself names the target class).
        ToolKind::Xwin => vec![(Severity::Error, "*-pc-windows-msvc".to_string())],
        ToolKind::Mingw => vec![(Severity::Error, "*-pc-windows-gnu".to_string())],
        ToolKind::OsxCross => vec![(Severity::Error, "*-apple-darwin".to_string())],
        // zigbuild is legitimate for Linux. If it resolved to concrete
        // (Linux) targets, pass. If the target is unresolvable on a surface
        // that is capable of Apple/Windows, warn rather than silently pass.
        ToolKind::Zigbuild => {
            if !concrete.is_empty() {
                Vec::new()
            } else if file.capable {
                vec![(Severity::Warning, "<unresolved>".to_string())]
            } else {
                Vec::new()
            }
        }
        // Raw zig / generic compilers only violate when a concrete
        // Apple/Windows target is present (handled above). Otherwise pass to
        // avoid flagging Linux/native compiler use.
        ToolKind::ZigCc | ToolKind::AltCompiler => Vec::new(),
    }
}

fn dedup_outcomes(outcomes: &mut Vec<(Severity, String)>) {
    outcomes.sort();
    outcomes.dedup();
}

fn recommendation(tool: ToolKind, target: &str) -> String {
    if target == "<unresolved>" {
        return format!(
            "resolve the build target; use `soldr build --target <triple>` \
             (or `soldr prepare --target <triple>`) instead of `{}`",
            tool.display()
        );
    }
    format!(
        "use `soldr build --target {target}` (or `soldr prepare --target {target}`) \
         instead of `{}`",
        tool.display()
    )
}

/// True when `code` contains `a` immediately followed by `b` as whitespace-
/// separated words (e.g. `soldr build`), tolerating extra flags is *not*
/// intended — this matches the adjacent `soldr build` / `soldr prepare` heads.
fn contains_word_pair(code: &str, a: &str, b: &str) -> bool {
    let toks: Vec<&str> = code.split_whitespace().collect();
    toks.windows(2).any(|w| w[0] == a && w[1] == b)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lint_ci::scan::scan_text;

    fn findings(text: &str) -> Vec<Finding> {
        let file = scan_text("wf.yml".into(), text);
        CrossCompileSurface.check(&[file])
    }

    fn errors(text: &str) -> Vec<Finding> {
        findings(text)
            .into_iter()
            .filter(|f| f.severity == Severity::Error)
            .collect()
    }

    // ---- Negative fixtures: must FLAG (error) ----

    #[test]
    fn flags_cargo_xwin_windows_msvc() {
        for target in ["x86_64-pc-windows-msvc", "aarch64-pc-windows-msvc"] {
            let e = errors(&format!("        run: cargo xwin build --target {target}"));
            assert_eq!(e.len(), 1, "target {target}");
            assert_eq!(e[0].tool, "cargo xwin");
            assert_eq!(e[0].target, target);
            assert!(e[0].recommendation.contains("soldr build --target"));
        }
    }

    #[test]
    fn flags_cargo_zigbuild_apple_darwin() {
        for target in ["x86_64-apple-darwin", "aarch64-apple-darwin"] {
            let e = errors(&format!("        run: cargo zigbuild --target {target}"));
            assert_eq!(e.len(), 1, "target {target}");
            assert_eq!(e[0].tool, "cargo zigbuild");
            assert_eq!(e[0].target, target);
        }
    }

    #[test]
    fn flags_raw_zig_cc_for_apple_and_windows() {
        for target in [
            "aarch64-apple-darwin",
            "x86_64-apple-darwin",
            "x86_64-pc-windows-msvc",
            "aarch64-pc-windows-msvc",
        ] {
            let e = errors(&format!(
                "        run: zig cc -o out.o --target={target} main.c"
            ));
            assert_eq!(e.len(), 1, "target {target}");
            assert_eq!(e[0].tool, "zig");
        }
    }

    #[test]
    fn flags_alternate_compiler_with_apple_or_windows_target() {
        for target in [
            "aarch64-apple-darwin",
            "x86_64-apple-darwin",
            "x86_64-pc-windows-msvc",
            "aarch64-pc-windows-msvc",
        ] {
            let e = errors(&format!("        run: clang --target={target} -c main.c"));
            assert_eq!(e.len(), 1, "target {target}");
        }
    }

    #[test]
    fn flags_env_prefix_form() {
        let e = errors(
            "        run: CC_x86_64_pc_windows_msvc=zig-cc cargo xwin build --target x86_64-pc-windows-msvc",
        );
        assert_eq!(e.len(), 1);
        assert_eq!(e[0].tool, "cargo xwin");
    }

    #[test]
    fn flags_multiline_run_block() {
        // Each physical line of a `run: |` block is scanned independently.
        let text = "        run: |\n          set -e\n          FOO=bar cargo zigbuild --target aarch64-apple-darwin\n          echo done";
        let e = errors(text);
        assert_eq!(e.len(), 1);
        assert_eq!(e[0].line, 3);
    }

    #[test]
    fn flags_backslash_continuation_split_command() {
        let text = "        run: |\n          cargo zigbuild \\\n            --target aarch64-apple-darwin";
        let e = errors(text);
        assert_eq!(e.len(), 1);
        assert_eq!(e[0].line, 2);
    }

    #[test]
    fn flags_cargo_install_xwin() {
        let e = errors("        run: cargo install cargo-xwin");
        assert_eq!(e.len(), 1);
        assert_eq!(e[0].target, "*-pc-windows-msvc");
    }

    // ---- Positive fixtures: must PASS ----

    #[test]
    fn passes_soldr_build_and_prepare_for_apple_windows() {
        assert!(errors("        run: soldr build --target aarch64-apple-darwin").is_empty());
        assert!(errors("        run: soldr build --target x86_64-pc-windows-msvc").is_empty());
        assert!(errors("        run: soldr prepare --target aarch64-apple-darwin").is_empty());
    }

    #[test]
    fn passes_zig_for_linux_and_manylinux() {
        assert!(errors("        run: cargo zigbuild --target x86_64-unknown-linux-gnu").is_empty());
        assert!(
            errors("        run: cargo zigbuild --target aarch64-unknown-linux-musl").is_empty()
        );
        assert!(
            findings("        run: cargo zigbuild --target x86_64-unknown-linux-gnu").is_empty(),
            "no warnings for a plainly-Linux zigbuild"
        );
        // maturin-driven zig for manylinux.
        assert!(
            errors("        run: maturin build --zig --target x86_64-unknown-linux-gnu").is_empty()
        );
    }

    #[test]
    fn mixed_matrix_flags_apple_row_only() {
        let text = "\
    strategy:
      matrix:
        target:
          - x86_64-unknown-linux-gnu
          - aarch64-apple-darwin
    steps:
      - run: cargo zigbuild --target ${{ matrix.target }}";
        let e = errors(text);
        assert_eq!(e.len(), 1, "only the apple row is a violation");
        assert_eq!(e[0].target, "aarch64-apple-darwin");
    }

    #[test]
    fn all_linux_matrix_zigbuild_passes() {
        let text = "\
    strategy:
      matrix:
        target:
          - x86_64-unknown-linux-gnu
          - aarch64-unknown-linux-musl
    steps:
      - run: cargo zigbuild --target ${{ matrix.target }}";
        assert!(errors(text).is_empty());
        assert!(findings(text).is_empty());
    }

    #[test]
    fn echoed_command_in_summary_is_not_flagged() {
        // Balanced-quote stripping means a documentation echo is not an
        // invocation.
        let text = "        run: echo \"soldr cargo zigbuild --target aarch64-apple-darwin\"";
        assert!(findings(text).is_empty());
    }

    #[test]
    fn comment_mentioning_legacy_tool_is_not_flagged() {
        let text = "        # legacy: cargo xwin build --target x86_64-pc-windows-msvc";
        assert!(findings(text).is_empty());
    }

    #[test]
    fn prose_yaml_keys_are_not_flagged() {
        // Step names and cache keys mention tools as prose/identifiers, not
        // as invocations — they are not `run:` command surfaces.
        let name = "      - name: Exercise 4 — cargo xwin build for x86_64-pc-windows-msvc";
        assert!(findings(name).is_empty());
        let key = "          key: soldr-zigbuild-cargo-zigbuild-0.23.0-aarch64-apple-darwin";
        assert!(findings(key).is_empty());
    }

    #[test]
    fn tool_name_as_argument_is_not_flagged() {
        // A tool name passed as an argument to another command is data.
        let json_arg = "        run: python3 query.py --json cargo-zigbuild aarch64-apple-darwin";
        assert!(findings(json_arg).is_empty());
        let find_arg = "        run: find . -type f -name cargo-zigbuild -print";
        assert!(findings(find_arg).is_empty());
    }

    #[test]
    fn helper_script_indirection_is_flagged() {
        // A referenced `.sh` helper is scanned in full (every non-comment
        // line is a command surface).
        let file = scan_text(
            "ci/build.sh".into(),
            "#!/usr/bin/env bash\nset -e\ncargo zigbuild --target aarch64-apple-darwin\n",
        );
        let e: Vec<Finding> = CrossCompileSurface
            .check(&[file])
            .into_iter()
            .filter(|f| f.severity == Severity::Error)
            .collect();
        assert_eq!(e.len(), 1);
        assert_eq!(e[0].file, "ci/build.sh");
        assert_eq!(e[0].line, 3);
    }

    #[test]
    fn unresolved_target_on_capable_surface_warns() {
        // A zigbuild whose target is a bare placeholder with no matrix
        // declaration, on a file that mentions apple/windows, warns.
        let text = "\
    env:
      NOTE: builds x86_64-pc-windows-msvc somewhere
    steps:
      - run: cargo zigbuild --target ${{ matrix.target }}";
        let all = findings(text);
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].severity, Severity::Warning);
        assert_eq!(all[0].target, "<unresolved>");
        // Warning-only does not produce an error.
        assert!(errors(text).is_empty());
    }
}
