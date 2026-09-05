//! Always-on hierarchical per-build XML log (issue #1790).
//!
//! Every managed `soldr cargo ...` build writes one XML file to
//! `<soldr root>/logs/builds/<timestamp>-<sanitized-cwd>.xml` — a flat
//! directory, newest-first by filename (the timestamp prefix sorts
//! lexically = chronologically). The file is a self-contained,
//! best-effort snapshot of the build: header (argv, cwd, timing,
//! exit code), derived `[profile.*]` metadata, and a three-section
//! hierarchy (`download` / `compile` / `link`) built from whatever
//! the daemon build-history DB and the zccache compile journal have
//! recorded for the session.
//!
//! ## Why XML (owner decision, issue #1790 follow-up)
//!
//! The log is dominated by repeated attribute-blocks inside group
//! nodes — every `compile` / `link` item is a flat, fixed-shape record
//! and the derived `[profile.*]` settings are stamped once per group
//! rather than once per item. XML's attribute-on-element shape
//! expresses that more naturally (and more compactly) than JSON's
//! object-per-item repetition, so the log was converted from JSON to
//! XML. The emitter is hand-rolled (no new dependency) since the
//! shape is small and fixed.
//!
//! ## Never fails the build
//!
//! [`write_build_log`] returns a `Result` so callers can log a warning
//! on failure, but internally every data source (daemon DB, compile
//! journal) is read best-effort: a missing or unreadable source yields
//! an empty section rather than an error. Only directory-creation /
//! final-file-write failures propagate — callers must treat even those
//! as non-fatal to the build itself.
//!
//! ## Schema (stable, `schema_version: 1`)
//!
//! ```xml
//! <?xml version="1.0" encoding="UTF-8"?>
//! <build schema_version="1" soldr_version="0.8.21" cwd="C:\Users\niteris\dev\soldr2" started_at_ms="0" ended_at_ms="0" duration_ms="0" exit_code="0">
//!   <args>
//!     <arg>cargo</arg>
//!     <arg>build</arg>
//!     <arg>--release</arg>
//!   </args>
//!   <fingerprint_dirty>
//!     <unit name="serde" version="1.0.0" reason="the config settings changed"/>
//!   </fingerprint_dirty>
//!   <steps>
//!     <download wall_ms="0" cpu_ms="0">
//!       <item name="cargo-nextest" source="github-release" started_at_ms="0" duration_ms="0"/>
//!     </download>
//!     <compile wall_ms="0" cpu_ms="0" target="x86_64-pc-windows-msvc" profile="release" debug="false" opt_level="3" lto="off">
//!       <item crate="foo" duration_ms="0" cache="hit"/>
//!     </compile>
//!     <link wall_ms="0" cpu_ms="0" derived="true" target="x86_64-pc-windows-msvc" profile="release" debug="false" opt_level="3" lto="off">
//!       <item crate="foo" duration_ms="0"/>
//!     </link>
//!   </steps>
//!   <totals wall_ms="0" cpu_ms="0" crate_count="0" cache_hits="0" cache_misses="0"/>
//! </build>
//! ```
//!
//! The top-level `build` element carries only header data
//! (`schema_version`, `soldr_version`, `cwd`, `started_at_ms`,
//! `ended_at_ms`, `duration_ms`, `exit_code`) — there is no separate
//! `<settings>` element. The derived `[profile.*]` metadata
//! (`target` / `profile` / `debug` / `opt_level` / `lto`) is instead
//! stamped as attributes on BOTH the `compile` and `link` group nodes
//! — that duplication is intentional (the owner's load-bearing
//! requirement): each group is meant to be readable/greppable on its
//! own without cross-referencing a separate settings block.
//!
//! ## Derived-link caveat
//!
//! v1 has no dedicated "link" event from the daemon. The `link` section
//! is *derived*: the compile event with the latest `CompileEnd` end
//! timestamp is treated as the linking crate. That crate's entry
//! therefore appears in BOTH the `compile` section AND the derived
//! `link` section — this is intentional (v1 does not subtract it out
//! of `compile`), and every `link` item is tagged `derived: true` so
//! consumers can tell it apart from a real, independently-measured
//! link phase.
//!
//! ## `cpu_ms` caveat
//!
//! `cpu_ms` in every section is *aggregate busy time summed across
//! (possibly parallel) units* — e.g. the sum of every compile's
//! `duration_us`, converted to milliseconds — NOT OS-reported CPU
//! time. On a build with N-way parallelism, `cpu_ms` can comfortably
//! exceed `wall_ms`. Rename with caution if a future version wires up
//! real `getrusage`-style CPU accounting.
//!
//! ## RUSTFLAGS out of scope
//!
//! [`derive_build_meta`] reads `[profile.<name>]` out of the target
//! `Cargo.toml` for `opt-level` / `debug` / `lto`. It does NOT account
//! for `RUSTFLAGS` / `CARGO_PROFILE_*` environment overrides — those
//! can silently change the effective values. Follow-up work, not v1.

use crate::build_log_meta::{fetch_timing, sanitize_cwd_slug, utc_compact_timestamp};
use crate::core::{SoldrError, SoldrPaths, TargetTriple};
use crate::daemon::db::{Event, EventKind};

#[cfg(test)]
use crate::daemon::db;
use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

const SCHEMA_VERSION: u32 = 1;

/// How many per-build XML logs [`prune_build_logs`] keeps by default.
/// Newest-first by filename (the timestamp prefix sorts lexically =
/// chronologically).
pub const BUILD_LOG_KEEP: usize = 100;

/// Everything [`write_build_log`] needs to render one build's XML log.
/// Constructed by the cargo front door once a managed build finishes
/// (success OR failure — the log is always-on, not failure-only).
/// Which toolchain homes a build actually executed under (soldr#1799).
///
/// The failure this exists to make visible is the quiet one. A host-resolved
/// `cargo`/`rustc` running under soldr's managed, default-less `RUSTUP_HOME`
/// either dies with "no default toolchain" (#1768) or -- far worse -- keeps
/// working while flipping which compiler binary is used between runs, which
/// invalidates cargo fingerprints and zccache keys and silently recompiles
/// the world on what should be a warm build. Nothing fails; builds are just
/// 10-50x slower, indefinitely.
///
/// Recording the resolved binary next to the discriminant is what makes the
/// log self-checking: `home_origin="managed"` is only legitimate when
/// `binary` physically lives inside a managed home, and that is exactly the
/// pair CI asserts on.
/// soldr#2545: the effective compiler-wrapper identity this build applied
/// to its cargo child, beside the origin that produced it. `effective` is
/// `None` when Soldr explicitly disabled the wrapper. Mirrors the
/// `SOLDR_EFFECTIVE_RUSTC_WRAPPER` pair so a future wrapper-flip rebuild
/// storm explains itself from the log alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WrapperIdentity {
    pub effective: Option<PathBuf>,
    pub origin: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolchainHomes {
    /// `caller` or `managed` -- see `binaries::HomeOrigin`.
    pub home_origin: &'static str,
    /// The resolved cargo binary this build ran.
    pub binary: PathBuf,
}

pub struct BuildLogRequest<'a> {
    pub paths: &'a SoldrPaths,
    pub session_id: u64,
    pub cwd: &'a Path,
    /// Full invoked argv (soldr subcommand + cargo args), e.g.
    /// `["cargo", "build", "--release"]`.
    pub args: &'a [String],
    pub started_at_ms: i64,
    pub ended_at_ms: i64,
    pub exit_code: i32,
    /// Path to zccache's per-session compile journal (JSONL). `None`
    /// when the session never resolved one (e.g. cache disabled).
    pub compile_journal_path: Option<PathBuf>,
    /// Byte offset into `compile_journal_path` where this session's
    /// entries begin. Lines before this offset belong to a prior
    /// session sharing the same journal file and are ignored.
    pub compile_journal_start_len: u64,
    /// soldr#1799: the homes this build's toolchain ran under. `None` when
    /// the caller could not resolve them, which is logged as absent rather
    /// than guessed -- a wrong value here would be worse than no value,
    /// since CI keys on it.
    pub toolchain: Option<ToolchainHomes>,
    /// soldr#2545: effective wrapper identity, absent when the caller could
    /// not resolve it (logged as absent rather than guessed).
    pub wrapper: Option<WrapperIdentity>,
    /// Units cargo reported as dirty, with its stated reason, parsed from
    /// the `fingerprint dirty for` records that
    /// `CARGO_LOG=cargo::core::compiler::fingerprint=info` makes cargo emit.
    /// Empty when that level is off or nothing was dirty. Recorded here so
    /// the "why did this recompile?" signal lives in soldr's own log instead
    /// of only in whatever captured the terminal stderr.
    pub fingerprint_dirty: Vec<FingerprintDirty>,
}

/// One `fingerprint dirty for <name> v<version>` record plus the
/// `dirty: <reason>` line cargo prints under it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FingerprintDirty {
    pub name: String,
    pub version: String,
    pub reason: String,
}

/// `<soldr root>/logs/builds` — flat directory, one XML file per
/// managed build.
pub fn build_logs_dir(paths: &SoldrPaths) -> PathBuf {
    paths.root.join("logs").join("builds")
}

/// Render and write the per-build XML log for `request`. Never panics;
/// every data source is read best-effort. Only directory-creation and
/// final-write I/O errors are surfaced — callers should treat even
/// those as warnings, not build failures.
/// Build-log inputs, preferring the daemon that owns the tables
/// (soldr#1814 slice 2a).
///
/// When the daemon is unavailable the log deliberately remains incomplete:
/// it is a best-effort artifact and must never contend for daemon-owned state.
fn daemon_build_log_inputs(
    request: &BuildLogRequest<'_>,
) -> (
    Vec<Event>,
    Option<Box<crate::daemon::protocol::BuildRecord>>,
    &'static str,
) {
    let sock = crate::daemon::client::default_sock_path(request.paths);
    match crate::daemon::client::build_log_inputs(&sock, request.session_id) {
        Ok((events, record)) => (events, record, "daemon"),
        Err(error) => {
            tracing::debug!(
                event = "build_log_inputs_unavailable",
                session_id = request.session_id,
                error = ?error,
                "daemon did not serve build-log inputs; build log will be incomplete"
            );
            (Vec::new(), None, "daemon-unavailable")
        }
    }
}

pub fn write_build_log(request: &BuildLogRequest<'_>) -> Result<PathBuf, SoldrError> {
    // Build history is daemon-owned. An unavailable daemon produces an
    // intentionally incomplete best-effort log rather than a second opener.
    let (events, daemon_record, history_source) = daemon_build_log_inputs(request);
    write_build_log_with_history(request, &events, daemon_record, history_source)
}

fn write_build_log_with_history(
    request: &BuildLogRequest<'_>,
    events: &[Event],
    daemon_record: Option<Box<crate::daemon::protocol::BuildRecord>>,
    history_source: &'static str,
) -> Result<PathBuf, SoldrError> {
    let dir = build_logs_dir(request.paths);
    std::fs::create_dir_all(&dir)?;

    let build_meta = derive_build_meta(request.args, request.cwd);
    let download_step = build_download_step(fetch_timing::drain());

    let cache_outcomes = read_compile_cache_outcomes(
        request.compile_journal_path.as_deref(),
        request.compile_journal_start_len,
    );

    let (compile_items, compile_wall_ms, compile_cpu_ms) =
        build_compile_items(events, &cache_outcomes);
    let link_step = build_link_step(events);

    let mut hits = compile_items
        .iter()
        .filter(|item| item.cache == "hit")
        .count() as u64;
    let mut misses = compile_items
        .iter()
        .filter(|item| item.cache == "miss")
        .count() as u64;
    if hits == 0 && misses == 0 {
        if let Some(summary) = daemon_record.and_then(|record| record.cache_summary) {
            hits = summary.hits;
            misses = summary.misses;
        }
    }

    let totals_wall_ms = (request.ended_at_ms - request.started_at_ms).max(0) as u64;
    // The derived link section re-labels the final compile slice, so its
    // cpu_ms is already included in `compile_cpu_ms` — adding it again
    // would double-count. Only a genuine (non-derived) link timing
    // contributes separately to the totals.
    let link_cpu_ms = if link_step.derived {
        0
    } else {
        link_step.cpu_ms
    };
    let totals_cpu_ms = download_step.cpu_ms + compile_cpu_ms + link_cpu_ms;
    let crate_count = compile_items.len();

    let doc = BuildLogDocument {
        schema_version: SCHEMA_VERSION,
        soldr_version: env!("CARGO_PKG_VERSION"),
        cwd: request.cwd.display().to_string(),
        args: request.args.to_vec(),
        started_at_ms: request.started_at_ms,
        ended_at_ms: request.ended_at_ms,
        duration_ms: totals_wall_ms,
        exit_code: request.exit_code,
        build: build_meta,
        toolchain: request.toolchain.as_ref().map(|t| ToolchainHomes {
            home_origin: t.home_origin,
            binary: t.binary.clone(),
        }),
        wrapper: request.wrapper.clone(),
        fingerprint_dirty: request.fingerprint_dirty.clone(),
        steps: Steps {
            download: download_step,
            compile: CompileStep {
                items: compile_items,
                wall_ms: compile_wall_ms,
                cpu_ms: compile_cpu_ms,
            },
            link: link_step,
        },
        totals: Totals {
            wall_ms: totals_wall_ms,
            cpu_ms: totals_cpu_ms,
            crate_count,
            cache_hits: hits,
            cache_misses: misses,
        },
        history_source,
    };

    let filename = unique_filename(&dir, request.started_at_ms, request.cwd);
    let path = dir.join(&filename);
    let xml = render_xml(&doc);
    std::fs::write(&path, xml)?;
    Ok(path)
}

#[cfg(test)]
pub(crate) fn write_build_log_with_history_for_test(
    request: &BuildLogRequest<'_>,
    events: &[Event],
) -> Result<PathBuf, SoldrError> {
    write_build_log_with_history(request, events, None, "test-fixture")
}

/// Delete all but the newest `keep` files in `dir` (sorted by filename,
/// descending — the timestamp-prefixed name makes lexical order
/// chronological). Matches both `*.xml` (current format) and `*.json`
/// (legacy files written by interim builds before the JSON->XML
/// conversion, issue #1790 follow-up) so old logs still get GC'd.
/// Best-effort: per-file delete errors are swallowed. Returns the
/// number of files actually deleted.
pub fn prune_build_logs(dir: &Path, keep: usize) -> usize {
    let mut entries: Vec<PathBuf> = match std::fs::read_dir(dir) {
        Ok(read_dir) => read_dir
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| {
                matches!(
                    path.extension().and_then(|ext| ext.to_str()),
                    Some("xml") | Some("json")
                )
            })
            .collect(),
        Err(_) => return 0,
    };
    entries.sort_by(|a, b| b.file_name().cmp(&a.file_name()));

    let mut deleted = 0;
    for path in entries.into_iter().skip(keep) {
        if std::fs::remove_file(&path).is_ok() {
            deleted += 1;
        }
    }
    deleted
}

// ---------------------------------------------------------------------
// XML schema types
// ---------------------------------------------------------------------

#[derive(Debug)]
struct BuildLogDocument {
    schema_version: u32,
    soldr_version: &'static str,
    cwd: String,
    args: Vec<String>,
    started_at_ms: i64,
    ended_at_ms: i64,
    duration_ms: u64,
    exit_code: i32,
    build: BuildMeta,
    steps: Steps,
    totals: Totals,
    /// Whether daemon-owned build history was available for this snapshot.
    history_source: &'static str,
    /// soldr#1799 -- see [`ToolchainHomes`].
    toolchain: Option<ToolchainHomes>,
    /// soldr#2545 -- see [`WrapperIdentity`].
    wrapper: Option<WrapperIdentity>,
    fingerprint_dirty: Vec<FingerprintDirty>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BuildMeta {
    target: String,
    profile: String,
    debug: bool,
    opt_level: String,
    lto: String,
}

#[derive(Debug)]
struct Steps {
    download: DownloadStep,
    compile: CompileStep,
    link: LinkStep,
}

#[derive(Debug)]
struct DownloadStep {
    items: Vec<DownloadItem>,
    wall_ms: u64,
    cpu_ms: u64,
}

#[derive(Debug)]
struct DownloadItem {
    name: String,
    source: String,
    started_at_ms: i64,
    duration_ms: u64,
}

#[derive(Debug)]
struct CompileStep {
    items: Vec<CompileItem>,
    wall_ms: u64,
    cpu_ms: u64,
}

#[derive(Debug, Clone)]
struct CompileItem {
    crate_name: String,
    duration_ms: u64,
    /// `"hit"` / `"miss"` / `"unknown"` — resolved from the zccache
    /// compile journal; `"unknown"` when the journal line was missing,
    /// unparseable, or didn't name this crate.
    cache: String,
}

#[derive(Debug)]
struct LinkStep {
    items: Vec<LinkItem>,
    wall_ms: u64,
    cpu_ms: u64,
    /// Always `true` in v1 — see the module doc's "derived-link"
    /// caveat: there is no independent link-phase measurement yet.
    derived: bool,
}

#[derive(Debug)]
struct LinkItem {
    crate_name: String,
    duration_ms: u64,
}

#[derive(Debug)]
struct Totals {
    wall_ms: u64,
    cpu_ms: u64,
    crate_count: usize,
    cache_hits: u64,
    cache_misses: u64,
}

// ---------------------------------------------------------------------
// XML rendering
// ---------------------------------------------------------------------

/// Escape a string for use as an XML attribute value (or `<arg>` text
/// content): `&`, `<`, `>`, `"`, and `'` become entity references, and
/// any control character below `0x20` other than tab (`\t`) and
/// newline (`\n`) becomes a numeric character reference (`&#xNN;`) so
/// the output stays well-formed even if a crate name or cwd path
/// somehow embeds one.
pub(crate) fn xml_escape_attr(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            '\t' | '\n' => out.push(ch),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("&#x{:02X};", c as u32));
            }
            c => out.push(c),
        }
    }
    out
}

/// Render one `name="escaped-value"` attribute, including the leading
/// space.
fn attr(name: &str, value: &str) -> String {
    format!(" {name}=\"{}\"", xml_escape_attr(value))
}

fn attr_disp(name: &str, value: impl std::fmt::Display) -> String {
    attr(name, &value.to_string())
}

fn render_xml(doc: &BuildLogDocument) -> String {
    let mut out = String::new();
    out.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");

    out.push_str("<build");
    out.push_str(&attr_disp("schema_version", doc.schema_version));
    out.push_str(&attr("soldr_version", doc.soldr_version));
    out.push_str(&attr("cwd", &doc.cwd));
    out.push_str(&attr_disp("started_at_ms", doc.started_at_ms));
    out.push_str(&attr_disp("ended_at_ms", doc.ended_at_ms));
    out.push_str(&attr_disp("duration_ms", doc.duration_ms));
    out.push_str(&attr_disp("exit_code", doc.exit_code));
    out.push_str(&attr("history_source", doc.history_source));
    out.push_str(">\n");

    render_args(&mut out, &doc.args);
    render_toolchain(&mut out, doc.toolchain.as_ref());
    render_wrapper(&mut out, doc.wrapper.as_ref());
    render_fingerprint_dirty(&mut out, &doc.fingerprint_dirty);
    render_steps(&mut out, &doc.steps, &doc.build);
    render_totals(&mut out, &doc.totals);

    out.push_str("</build>\n");
    out
}

fn render_args(out: &mut String, args: &[String]) {
    out.push_str("  <args>\n");
    for arg in args {
        out.push_str("    <arg>");
        out.push_str(&xml_escape_attr(arg));
        out.push_str("</arg>\n");
    }
    out.push_str("  </args>\n");
}

/// soldr#1799. Emitted as its own element rather than as attributes on
/// `<build>` so a later phase can add per-execution rows (passthrough,
/// dylint, wrapper) without changing the shape callers already parse.
/// soldr#2545. Same element-not-attribute rationale as `<toolchain>`.
fn render_wrapper(out: &mut String, wrapper: Option<&WrapperIdentity>) {
    let Some(wrapper) = wrapper else {
        return;
    };
    out.push_str("  <wrapper");
    out.push_str(&attr("origin", wrapper.origin));
    if let Some(effective) = wrapper.effective.as_ref() {
        out.push_str(&attr("effective", &effective.display().to_string()));
    }
    out.push_str(
        " />
",
    );
}

/// Cargo's own "why did this recompile?" answer, one `<unit>` per dirty
/// record. Omitted entirely when there are none, so logs from builds that
/// ran without fingerprint logging keep their prior shape byte-for-byte.
fn render_fingerprint_dirty(out: &mut String, dirty: &[FingerprintDirty]) {
    if dirty.is_empty() {
        return;
    }
    out.push_str("  <fingerprint_dirty>\n");
    for unit in dirty {
        out.push_str("    <unit");
        out.push_str(&attr("name", &unit.name));
        out.push_str(&attr("version", &unit.version));
        out.push_str(&attr("reason", &unit.reason));
        out.push_str(" />\n");
    }
    out.push_str("  </fingerprint_dirty>\n");
}

fn render_toolchain(out: &mut String, toolchain: Option<&ToolchainHomes>) {
    let Some(toolchain) = toolchain else {
        return;
    };
    out.push_str("  <toolchain");
    out.push_str(&attr("home_origin", toolchain.home_origin));
    out.push_str(&attr("binary", &toolchain.binary.display().to_string()));
    out.push_str(
        " />
",
    );
}

fn render_steps(out: &mut String, steps: &Steps, meta: &BuildMeta) {
    out.push_str("  <steps>\n");
    render_download(out, &steps.download);
    render_compile(out, &steps.compile, meta);
    render_link(out, &steps.link, meta);
    out.push_str("  </steps>\n");
}

fn render_download(out: &mut String, step: &DownloadStep) {
    let head = format!(
        "    <download{}{}",
        attr_disp("wall_ms", step.wall_ms),
        attr_disp("cpu_ms", step.cpu_ms)
    );
    if step.items.is_empty() {
        out.push_str(&head);
        out.push_str("/>\n");
        return;
    }
    out.push_str(&head);
    out.push_str(">\n");
    for item in &step.items {
        out.push_str("      <item");
        out.push_str(&attr("name", &item.name));
        out.push_str(&attr("source", &item.source));
        out.push_str(&attr_disp("started_at_ms", item.started_at_ms));
        out.push_str(&attr_disp("duration_ms", item.duration_ms));
        out.push_str("/>\n");
    }
    out.push_str("    </download>\n");
}

fn render_build_meta_attrs(out: &mut String, meta: &BuildMeta) {
    out.push_str(&attr("target", &meta.target));
    out.push_str(&attr("profile", &meta.profile));
    out.push_str(&attr_disp("debug", meta.debug));
    out.push_str(&attr("opt_level", &meta.opt_level));
    out.push_str(&attr("lto", &meta.lto));
}

fn render_compile(out: &mut String, step: &CompileStep, meta: &BuildMeta) {
    let mut head = String::from("    <compile");
    head.push_str(&attr_disp("wall_ms", step.wall_ms));
    head.push_str(&attr_disp("cpu_ms", step.cpu_ms));
    render_build_meta_attrs(&mut head, meta);
    if step.items.is_empty() {
        out.push_str(&head);
        out.push_str("/>\n");
        return;
    }
    out.push_str(&head);
    out.push_str(">\n");
    for item in &step.items {
        out.push_str("      <item");
        out.push_str(&attr("crate", &item.crate_name));
        out.push_str(&attr_disp("duration_ms", item.duration_ms));
        out.push_str(&attr("cache", &item.cache));
        out.push_str("/>\n");
    }
    out.push_str("    </compile>\n");
}

fn render_link(out: &mut String, step: &LinkStep, meta: &BuildMeta) {
    let mut head = String::from("    <link");
    head.push_str(&attr_disp("wall_ms", step.wall_ms));
    head.push_str(&attr_disp("cpu_ms", step.cpu_ms));
    head.push_str(&attr_disp("derived", step.derived));
    render_build_meta_attrs(&mut head, meta);
    if step.items.is_empty() {
        out.push_str(&head);
        out.push_str("/>\n");
        return;
    }
    out.push_str(&head);
    out.push_str(">\n");
    for item in &step.items {
        out.push_str("      <item");
        out.push_str(&attr("crate", &item.crate_name));
        out.push_str(&attr_disp("duration_ms", item.duration_ms));
        out.push_str("/>\n");
    }
    out.push_str("    </link>\n");
}

fn render_totals(out: &mut String, totals: &Totals) {
    out.push_str("  <totals");
    out.push_str(&attr_disp("wall_ms", totals.wall_ms));
    out.push_str(&attr_disp("cpu_ms", totals.cpu_ms));
    out.push_str(&attr_disp("crate_count", totals.crate_count));
    out.push_str(&attr_disp("cache_hits", totals.cache_hits));
    out.push_str(&attr_disp("cache_misses", totals.cache_misses));
    out.push_str("/>\n");
}

// ---------------------------------------------------------------------
// Filename resolution
// ---------------------------------------------------------------------

fn unique_filename(dir: &Path, started_at_ms: i64, cwd: &Path) -> String {
    let ts = utc_compact_timestamp(started_at_ms);
    let slug = sanitize_cwd_slug(cwd);
    let base = format!("{ts}-{slug}");
    let mut candidate = format!("{base}.xml");
    let mut suffix = 2;
    while dir.join(&candidate).exists() {
        candidate = format!("{base}-{suffix}.xml");
        suffix += 1;
    }
    candidate
}

// ---------------------------------------------------------------------
// Download section
// ---------------------------------------------------------------------

fn build_download_step(items: Vec<fetch_timing::FetchTiming>) -> DownloadStep {
    let wall_ms = if items.is_empty() {
        0
    } else {
        let min_start = items
            .iter()
            .map(|item| item.started_at_ms)
            .min()
            .unwrap_or(0);
        let max_end = items
            .iter()
            .map(|item| item.started_at_ms + item.duration_ms as i64)
            .max()
            .unwrap_or(min_start);
        (max_end - min_start).max(0) as u64
    };
    let cpu_ms = items.iter().map(|item| item.duration_ms).sum();
    let download_items = items
        .into_iter()
        .map(|item| DownloadItem {
            name: item.name,
            source: item.source,
            started_at_ms: item.started_at_ms,
            duration_ms: item.duration_ms,
        })
        .collect();
    DownloadStep {
        items: download_items,
        wall_ms,
        cpu_ms,
    }
}

// ---------------------------------------------------------------------
// Compile / link sections
// ---------------------------------------------------------------------

/// Read the compile journal tail starting at `start_offset` and map
/// `crate_name -> "hit" | "miss" | "unknown"`. Later lines win on a
/// duplicate crate name (last-outcome-wins). Field names mirror the
/// zccache journal schema: `outcome` (one of `hit`,
/// `miss`, `error`, `cached_error`, `link_hit`, `link_miss`) and
/// `crate_name` (present only when the session opted into
/// `--profile` journaling). Unparseable lines or lines missing
/// `crate_name` are skipped — the crate simply falls back to
/// `"unknown"` in [`build_compile_items`].
fn read_compile_cache_outcomes(
    journal_path: Option<&Path>,
    start_offset: u64,
) -> HashMap<String, String> {
    let mut outcomes = HashMap::new();
    let Some(journal_path) = journal_path else {
        return outcomes;
    };
    let Ok(mut file) = std::fs::File::open(journal_path) else {
        return outcomes;
    };
    let Ok(metadata) = file.metadata() else {
        return outcomes;
    };
    if metadata.len() <= start_offset {
        return outcomes;
    }
    if file.seek(SeekFrom::Start(start_offset)).is_err() {
        return outcomes;
    }
    let mut body = String::new();
    if file.read_to_string(&mut body).is_err() {
        return outcomes;
    }

    for line in body.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let Some(crate_name) = value.get("crate_name").and_then(|v| v.as_str()) else {
            continue;
        };
        let outcome = value.get("outcome").and_then(|v| v.as_str()).unwrap_or("");
        let mapped = match outcome {
            "hit" | "link_hit" => "hit",
            "miss" | "link_miss" => "miss",
            _ => "unknown",
        };
        outcomes.insert(crate_name.to_string(), mapped.to_string());
    }
    outcomes
}

/// Pair `CompileStart` / `CompileEnd` events per issue #1790's compile
/// section semantics. Returns `(items, wall_ms, cpu_ms)`:
/// - `wall_ms` spans `min(CompileStart.ts_ms) .. max(CompileEnd.ts_ms)`
///   (`0` when either side is absent).
/// - `cpu_ms` is the sum of every `CompileEnd.duration_us`, converted
///   to milliseconds (aggregate busy time, not OS CPU time — see the
///   module doc).
fn build_compile_items(
    events: &[Event],
    cache_outcomes: &HashMap<String, String>,
) -> (Vec<CompileItem>, u64, u64) {
    let mut items = Vec::new();
    let mut starts = Vec::new();
    let mut ends = Vec::new();
    let mut cpu_ms_total = 0u64;

    for event in events {
        match event.kind {
            EventKind::CompileStart => starts.push(event.ts_ms),
            EventKind::CompileEnd => {
                ends.push(event.ts_ms);
                let duration_ms = event.duration_us.unwrap_or(0) / 1000;
                cpu_ms_total += duration_ms;
                let crate_name = event
                    .crate_name
                    .clone()
                    .unwrap_or_else(|| "<unknown>".to_string());
                let cache = cache_outcomes
                    .get(&crate_name)
                    .cloned()
                    .unwrap_or_else(|| "unknown".to_string());
                items.push(CompileItem {
                    crate_name,
                    duration_ms,
                    cache,
                });
            }
            _ => {}
        }
    }

    let wall_ms = match (starts.iter().min(), ends.iter().max()) {
        (Some(min_start), Some(max_end)) => (max_end - min_start).max(0) as u64,
        _ => 0,
    };

    (items, wall_ms, cpu_ms_total)
}

/// v1 derived link section — see the module doc's "derived-link"
/// caveat. The compile event with the latest `CompileEnd` timestamp is
/// treated as the linking crate.
fn build_link_step(events: &[Event]) -> LinkStep {
    let latest = events
        .iter()
        .filter(|event| matches!(event.kind, EventKind::CompileEnd))
        .max_by_key(|event| event.ts_ms);

    match latest {
        Some(event) => {
            let crate_name = event
                .crate_name
                .clone()
                .unwrap_or_else(|| "<unknown>".to_string());
            let duration_ms = event.duration_us.unwrap_or(0) / 1000;
            LinkStep {
                items: vec![LinkItem {
                    crate_name,
                    duration_ms,
                }],
                wall_ms: duration_ms,
                cpu_ms: duration_ms,
                derived: true,
            }
        }
        None => LinkStep {
            items: Vec::new(),
            wall_ms: 0,
            cpu_ms: 0,
            derived: true,
        },
    }
}

// ---------------------------------------------------------------------
// Build metadata derivation
// ---------------------------------------------------------------------

/// Derive `[profile.*]`-driven build metadata from the invoked argv and
/// the target `Cargo.toml`. RUSTFLAGS / `CARGO_PROFILE_*` env overrides
/// are intentionally out of scope for v1 — see the module doc.
pub(crate) fn derive_build_meta(args: &[String], cwd: &Path) -> BuildMeta {
    let profile = derive_profile(args);
    let target = derive_target(args, cwd);

    let is_debug_profile = profile == "debug";
    let default_opt_level = if is_debug_profile { "0" } else { "3" };
    let default_debug = is_debug_profile;
    let default_lto = "off";

    let toml_profile = read_cargo_toml_profile(cwd, &profile);

    let opt_level = toml_profile
        .as_ref()
        .and_then(|table| table.get("opt-level"))
        .map(opt_level_to_string)
        .unwrap_or_else(|| default_opt_level.to_string());
    let debug = toml_profile
        .as_ref()
        .and_then(|table| table.get("debug"))
        .and_then(|value| value.as_bool())
        .unwrap_or(default_debug);
    let lto = toml_profile
        .as_ref()
        .and_then(|table| table.get("lto"))
        .map(normalize_lto)
        .unwrap_or_else(|| default_lto.to_string());

    BuildMeta {
        target,
        profile,
        debug,
        opt_level,
        lto,
    }
}

fn derive_profile(args: &[String]) -> String {
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "--release" {
            return "release".to_string();
        }
        if arg == "--profile" {
            if let Some(name) = iter.next() {
                return name.clone();
            }
        }
        if let Some(rest) = arg.strip_prefix("--profile=") {
            return rest.to_string();
        }
    }
    "debug".to_string()
}

fn derive_target(args: &[String], cwd: &Path) -> String {
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "--target" {
            if let Some(triple) = iter.next() {
                return triple.clone();
            }
        }
        if let Some(rest) = arg.strip_prefix("--target=") {
            return rest.to_string();
        }
    }
    TargetTriple::detect_in_dir(cwd)
        .map(|triple| triple.triple())
        .unwrap_or_else(|_| "unknown".to_string())
}

fn read_cargo_toml_profile(cwd: &Path, profile: &str) -> Option<toml::Value> {
    let manifest_path = cwd.join("Cargo.toml");
    let text = std::fs::read_to_string(manifest_path).ok()?;
    let value: toml::Value = toml::from_str(&text).ok()?;
    value.get("profile")?.get(profile).cloned()
}

fn opt_level_to_string(value: &toml::Value) -> String {
    match value {
        toml::Value::Integer(n) => n.to_string(),
        toml::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn normalize_lto(value: &toml::Value) -> String {
    match value {
        toml::Value::Boolean(false) => "off".to_string(),
        toml::Value::Boolean(true) => "fat".to_string(),
        toml::Value::String(s) => match s.as_str() {
            "off" | "false" => "off".to_string(),
            "true" | "fat" => "fat".to_string(),
            "thin" => "thin".to_string(),
            other => other.to_string(),
        },
        _ => "off".to_string(),
    }
}

#[cfg(test)]
#[path = "build_log_tests.rs"]
mod tests;
