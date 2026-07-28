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
use crate::daemon::db::{self, Event, EventKind};
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
/// Falls back to a direct state-DB read when the daemon is unreachable, so a
/// `--no-cache` or daemon-less build still produces a complete log. The
/// fallback is reported rather than silent: a build log missing its compile
/// timeline should say why.
fn daemon_build_log_inputs(
    request: &BuildLogRequest<'_>,
) -> (
    Vec<db::Event>,
    Option<Box<crate::daemon::protocol::BuildRecord>>,
) {
    let sock = crate::daemon::client::default_sock_path(request.paths);
    match crate::daemon::client::build_log_inputs(&sock, request.session_id) {
        Ok((events, record)) => (events, record),
        Err(error) => {
            tracing::debug!(
                event = "build_log_inputs_fallback",
                session_id = request.session_id,
                error = ?error,
                "daemon did not serve build-log inputs; reading the state DB directly"
            );
            let db_path = crate::cache_lib::data_db_path(request.paths);
            (
                db::list_events_for_session(&db_path, request.session_id).unwrap_or_default(),
                None,
            )
        }
    }
}

pub fn write_build_log(request: &BuildLogRequest<'_>) -> Result<PathBuf, SoldrError> {
    let dir = build_logs_dir(request.paths);
    std::fs::create_dir_all(&dir)?;

    let build_meta = derive_build_meta(request.args, request.cwd);
    let download_step = build_download_step(fetch_timing::drain());

    let db_path = crate::cache_lib::data_db_path(request.paths);
    // soldr#1814 slice 2a: ask the daemon, which owns these tables, instead of
    // becoming a second opener of state.redb. Two `Required` opens here (5 s
    // budget each) is what exceeded a 10 s test deadline under parallel test
    // processes. Falls back to a direct read only when the daemon is
    // unreachable, so a daemon-less build still gets a complete log.
    let (events, daemon_record) = daemon_build_log_inputs(request);

    let cache_outcomes = read_compile_cache_outcomes(
        request.compile_journal_path.as_deref(),
        request.compile_journal_start_len,
    );

    let (compile_items, compile_wall_ms, compile_cpu_ms) =
        build_compile_items(&events, &cache_outcomes);
    let link_step = build_link_step(&events);

    let mut hits = compile_items
        .iter()
        .filter(|item| item.cache == "hit")
        .count() as u64;
    let mut misses = compile_items
        .iter()
        .filter(|item| item.cache == "miss")
        .count() as u64;
    if hits == 0 && misses == 0 {
        // Prefer the record the daemon already handed us above; only reopen
        // the DB ourselves if it was unreachable (soldr#1814 slice 2a).
        let record = match daemon_record {
            Some(record) => Some(record),
            None => db::get_build(&db_path, request.session_id)
                .ok()
                .flatten()
                .map(Box::new),
        };
        if let Some(summary) = record.and_then(|record| record.cache_summary) {
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
    };

    let filename = unique_filename(&dir, request.started_at_ms, request.cwd);
    let path = dir.join(&filename);
    let xml = render_xml(&doc);
    std::fs::write(&path, xml)?;
    Ok(path)
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
    /// soldr#1799 -- see [`ToolchainHomes`].
    toolchain: Option<ToolchainHomes>,
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
    out.push_str(">\n");

    render_args(&mut out, &doc.args);
    render_toolchain(&mut out, doc.toolchain.as_ref());
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
/// zccache journal schema documented at
/// `_vender/zccache/docs/journal-schema.md`: `outcome` (one of `hit`,
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
mod tests {
    use super::*;
    use crate::timed_test;
    use std::time::Duration;

    fn sample_request<'a>(
        paths: &'a SoldrPaths,
        cwd: &'a Path,
        args: &'a [String],
    ) -> BuildLogRequest<'a> {
        BuildLogRequest {
            paths,
            session_id: 42,
            cwd,
            args,
            started_at_ms: 1_700_000_000_000,
            ended_at_ms: 1_700_000_005_000,
            exit_code: 0,
            compile_journal_path: None,
            compile_journal_start_len: 0,
            // soldr#1799: absent by default so the existing cases keep
            // asserting the shape of a log without toolchain telemetry --
            // `None` must stay renderable, since it is what a build whose
            // soldr root failed to resolve produces.
            toolchain: None,
        }
    }

    timed_test!(
        toolchain_homes_render_when_present_and_vanish_when_absent,
        {
            let tmp = tempfile::tempdir().expect("tempdir");
            let paths = SoldrPaths::with_root(tmp.path().to_path_buf());
            let args = vec!["cargo".to_string(), "build".to_string()];

            // Absent: no element at all, rather than an element claiming an
            // origin nobody established. soldr#1799's CI check treats a missing
            // <toolchain> as "not asserted"; a fabricated one would read as a
            // pass.
            let request = sample_request(&paths, tmp.path(), &args);
            let without = write_build_log(&request).expect("write");
            let raw = std::fs::read_to_string(&without).expect("read");
            assert!(
                !raw.contains("<toolchain"),
                "absent telemetry must emit no element, got:
{raw}"
            );

            // Present: origin and the binary that justifies it.
            let mut request = sample_request(&paths, tmp.path(), &args);
            request.toolchain = Some(ToolchainHomes {
                home_origin: "caller",
                binary: PathBuf::from("/usr/bin/cargo"),
            });
            let with = write_build_log(&request).expect("write");
            let raw = std::fs::read_to_string(&with).expect("read");
            assert!(
                raw.contains("home_origin=\"caller\""),
                "expected the caller origin, got:
{raw}"
            );
            assert!(
                raw.contains("cargo"),
                "expected the resolved binary, got:
{raw}"
            );
        }
    );

    timed_test!(
        write_build_log_writes_file_with_expected_header,
        Duration::from_secs(10),
        {
            let tmp = tempfile::tempdir().expect("tmpdir");
            let paths = SoldrPaths::with_root(tmp.path().join("soldr-root"));
            let cwd_dir = tmp.path().join("project");
            std::fs::create_dir_all(&cwd_dir).expect("mkdir cwd");
            let args = vec!["cargo".to_string(), "build".to_string()];
            let request = sample_request(&paths, &cwd_dir, &args);

            let path = write_build_log(&request).expect("write_build_log");
            assert!(path.is_file(), "log file must exist: {}", path.display());
            assert_eq!(path.extension().and_then(|e| e.to_str()), Some("xml"));

            let raw = std::fs::read_to_string(&path).expect("read log");
            assert!(
                raw.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n"),
                "must start with the XML declaration: {raw}"
            );
            assert!(raw.contains("schema_version=\"1\""), "{raw}");
            assert!(
                raw.contains(&format!(
                    "cwd=\"{}\"",
                    xml_escape_attr(&cwd_dir.display().to_string())
                )),
                "{raw}"
            );
            assert!(raw.contains("<arg>cargo</arg>"), "{raw}");
            assert!(raw.contains("<arg>build</arg>"), "{raw}");
            assert!(raw.contains("wall_ms=\"5000\""), "totals wall_ms: {raw}");
            // Empty compile/download groups render self-closing (no
            // <item> children).
            assert!(!raw.contains("<item"), "no items expected: {raw}");
            assert!(raw.contains("derived=\"true\""), "{raw}");
            // The compile AND link group nodes both carry the derived
            // build-settings attributes (owner's load-bearing
            // requirement — settings stamped on both groups).
            for group in ["<compile", "<link"] {
                let start = raw
                    .find(group)
                    .unwrap_or_else(|| panic!("{group} missing: {raw}"));
                let end = raw[start..]
                    .find(['>', '/'])
                    .map(|i| start + i)
                    .unwrap_or(raw.len());
                let head = &raw[start..end];
                for attr_name in ["target=", "profile=", "debug=", "opt_level=", "lto="] {
                    assert!(
                        head.contains(attr_name),
                        "{group} node missing {attr_name}: {head}"
                    );
                }
            }
        }
    );

    timed_test!(filename_shape_starts_with_compact_timestamp_and_slug, {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let dir = tmp.path().join("builds");
        std::fs::create_dir_all(&dir).expect("mkdir");
        let cwd = PathBuf::from("C:\\Users\\niteris\\dev\\soldr2");
        let started_at_ms = 1_700_000_000_000_i64;

        let first = unique_filename(&dir, started_at_ms, &cwd);
        let ts = utc_compact_timestamp(started_at_ms);
        assert_eq!(ts.len(), 16, "compact timestamp must be 16 chars: {ts}");
        assert!(
            first.starts_with(&ts),
            "filename must start with the compact timestamp: {first}"
        );
        assert!(first.ends_with(".xml"));
        let slug = sanitize_cwd_slug(&cwd);
        assert!(
            first.contains(&slug),
            "filename must contain the sanitized cwd slug: {first}"
        );

        // Simulate a collision: create the exact filename and confirm
        // the next call appends "-2".
        std::fs::write(dir.join(&first), b"<build/>").expect("write collision file");
        let second = unique_filename(&dir, started_at_ms, &cwd);
        assert_ne!(first, second);
        assert!(
            second.ends_with("-2.xml"),
            "collision must append -2: {second}"
        );
    });

    timed_test!(derive_build_meta_reads_release_debug_and_target_flags, {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let cwd = tmp.path();

        let release_args = vec![
            "cargo".to_string(),
            "build".to_string(),
            "--release".to_string(),
        ];
        let meta = derive_build_meta(&release_args, cwd);
        assert_eq!(meta.profile, "release");
        assert_eq!(meta.opt_level, "3");
        assert!(!meta.debug);

        let bench_args = vec![
            "cargo".to_string(),
            "build".to_string(),
            "--profile".to_string(),
            "bench".to_string(),
        ];
        let meta = derive_build_meta(&bench_args, cwd);
        assert_eq!(meta.profile, "bench");

        let target_args = vec![
            "cargo".to_string(),
            "build".to_string(),
            "--target".to_string(),
            "x86_64-unknown-linux-gnu".to_string(),
        ];
        let meta = derive_build_meta(&target_args, cwd);
        assert_eq!(meta.target, "x86_64-unknown-linux-gnu");

        let default_args = vec!["cargo".to_string(), "build".to_string()];
        let meta = derive_build_meta(&default_args, cwd);
        assert_eq!(meta.profile, "debug");
        assert!(meta.debug);
        assert_eq!(meta.opt_level, "0");
    });

    timed_test!(derive_build_meta_reads_lto_from_cargo_toml, {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let cwd = tmp.path();
        std::fs::write(
            cwd.join("Cargo.toml"),
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n\n[profile.release]\nlto = \"thin\"\n",
        )
        .expect("write Cargo.toml");

        let args = vec![
            "cargo".to_string(),
            "build".to_string(),
            "--release".to_string(),
        ];
        let meta = derive_build_meta(&args, cwd);
        assert_eq!(meta.lto, "thin");
    });

    timed_test!(prune_build_logs_keeps_newest_n, Duration::from_secs(15), {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let dir = tmp.path().join("builds");
        std::fs::create_dir_all(&dir).expect("mkdir");

        let total = BUILD_LOG_KEEP + 5;
        let mut names = Vec::new();
        for i in 0..total {
            let name = format!("{:020}-project.json", i);
            std::fs::write(dir.join(&name), b"{}").expect("write fixture");
            names.push(name);
        }

        let deleted = prune_build_logs(&dir, BUILD_LOG_KEEP);
        assert_eq!(deleted, 5);

        let remaining: std::collections::HashSet<String> = std::fs::read_dir(&dir)
            .expect("read dir")
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(remaining.len(), BUILD_LOG_KEEP);

        // The newest BUILD_LOG_KEEP (highest-numbered) names must survive.
        for name in names.iter().skip(5) {
            assert!(
                remaining.contains(name),
                "newest file should survive prune: {name}"
            );
        }
        for name in names.iter().take(5) {
            assert!(
                !remaining.contains(name),
                "oldest file should be pruned: {name}"
            );
        }
    });

    timed_test!(prune_build_logs_matches_both_xml_and_legacy_json, {
        // Legacy `.json` files (written by interim builds before the
        // JSON->XML conversion) must still be swept alongside current
        // `.xml` files, and unrelated extensions must be left alone.
        let tmp = tempfile::tempdir().expect("tmpdir");
        let dir = tmp.path().join("builds");
        std::fs::create_dir_all(&dir).expect("mkdir");

        std::fs::write(dir.join("20260101T000000Z-a.xml"), b"<build/>").expect("write xml");
        std::fs::write(dir.join("20260101T000001Z-b.json"), b"{}").expect("write json");
        std::fs::write(dir.join("readme.txt"), b"not a log").expect("write txt");

        let deleted = prune_build_logs(&dir, 0);
        assert_eq!(
            deleted, 2,
            "both the xml and legacy json log must be pruned"
        );

        let remaining: Vec<String> = std::fs::read_dir(&dir)
            .expect("read dir")
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(
            remaining,
            vec!["readme.txt".to_string()],
            "non-log extensions must be left alone: {remaining:?}"
        );
    });

    timed_test!(xml_escape_attr_escapes_reserved_and_control_chars, {
        assert_eq!(xml_escape_attr("plain"), "plain");
        assert_eq!(xml_escape_attr("a & b"), "a &amp; b");
        assert_eq!(xml_escape_attr("<tag>"), "&lt;tag&gt;");
        assert_eq!(xml_escape_attr("say \"hi\""), "say &quot;hi&quot;");
        assert_eq!(xml_escape_attr("it's"), "it&apos;s");
        // Control char (0x01) other than tab/newline is escaped;
        // tab and newline pass through unescaped.
        assert_eq!(xml_escape_attr("a\u{1}b"), "a&#x01;b");
        assert_eq!(xml_escape_attr("a\tb\nc"), "a\tb\nc");
    });

    timed_test!(
        write_build_log_escapes_ampersand_and_quote_in_cwd_and_args,
        {
            let tmp = tempfile::tempdir().expect("tmpdir");
            let paths = SoldrPaths::with_root(tmp.path().join("soldr-root"));
            // Directory names can't literally contain `"` on Windows, so
            // exercise the escaper against the raw cwd string embedded in
            // the request rather than an actual created directory — the
            // writer only needs `request.cwd` for rendering the `cwd`
            // attribute and the filename slug, both of which tolerate a
            // synthetic (non-existent) path fine for this test.
            let raw_cwd = tmp.path().join("a & b");
            std::fs::create_dir_all(&raw_cwd).expect("mkdir cwd");
            let args = vec![
                "cargo".to_string(),
                "build".to_string(),
                "--message-format=\"json\"".to_string(),
            ];
            let request = sample_request(&paths, &raw_cwd, &args);

            let path = write_build_log(&request).expect("write_build_log");
            let raw = std::fs::read_to_string(&path).expect("read log");

            // The escaped forms are present...
            assert!(raw.contains("a &amp; b"), "{raw}");
            assert!(raw.contains("--message-format=&quot;json&quot;"), "{raw}");
            // ...and the raw, unescaped forms are not (would produce
            // malformed XML).
            assert!(!raw.contains("a & b\""), "{raw}");
            assert!(!raw.contains("--message-format=\"json\""), "{raw}");
        }
    );

    timed_test!(compile_journal_cache_outcomes_map_hit_and_miss, {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let journal_path = tmp.path().join("compile_journal.jsonl");
        let lines = [
            r#"{"ts":"2026-01-01T00:00:00Z","outcome":"hit","compiler":"/rustc","args":[],"cwd":"/repo","exit_code":0,"session_id":null,"latency_ns":1000,"crate_name":"hit-crate"}"#,
            r#"{"ts":"2026-01-01T00:00:01Z","outcome":"miss","compiler":"/rustc","args":[],"cwd":"/repo","exit_code":0,"session_id":null,"latency_ns":2000,"crate_name":"miss-crate","miss_reason":"context_not_found"}"#,
            r#"not json at all"#,
            r#"{"ts":"2026-01-01T00:00:02Z","outcome":"error","compiler":"/rustc","args":[],"cwd":"/repo","exit_code":1,"session_id":null,"latency_ns":500,"crate_name":"error-crate"}"#,
        ];
        let body = lines.join("\n") + "\n";
        std::fs::write(&journal_path, &body).expect("write journal");

        let outcomes = read_compile_cache_outcomes(Some(&journal_path), 0);
        assert_eq!(outcomes.get("hit-crate").map(String::as_str), Some("hit"));
        assert_eq!(outcomes.get("miss-crate").map(String::as_str), Some("miss"));
        assert_eq!(
            outcomes.get("error-crate").map(String::as_str),
            Some("unknown")
        );
        assert!(!outcomes.contains_key("never-seen-crate"));

        // Byte-offset support: entries before `compile_journal_start_len`
        // must be ignored (they belong to a previous session sharing
        // the journal file).
        let offset = body.find("miss-crate").map(|i| i as u64).unwrap_or(0);
        // Back up to the start of that line.
        let line_start = body[..offset as usize]
            .rfind('\n')
            .map(|i| i as u64 + 1)
            .unwrap_or(0);
        let tail_only = read_compile_cache_outcomes(Some(&journal_path), line_start);
        assert!(!tail_only.contains_key("hit-crate"));
        assert_eq!(
            tail_only.get("miss-crate").map(String::as_str),
            Some("miss")
        );
    });

    timed_test!(build_compile_items_pairs_start_and_end_events, {
        let events = vec![
            Event {
                ts_ms: 1_000,
                session_id: Some(7),
                kind: EventKind::CompileStart,
                crate_name: Some("crate-a".into()),
                duration_us: None,
                target_dir: None,
                exit_code: None,
            },
            Event {
                ts_ms: 1_500,
                session_id: Some(7),
                kind: EventKind::CompileEnd,
                crate_name: Some("crate-a".into()),
                duration_us: Some(500_000),
                target_dir: None,
                exit_code: None,
            },
            Event {
                ts_ms: 1_600,
                session_id: Some(7),
                kind: EventKind::CompileEnd,
                crate_name: Some("crate-b".into()),
                duration_us: Some(100_000),
                target_dir: None,
                exit_code: None,
            },
        ];
        let mut outcomes = HashMap::new();
        outcomes.insert("crate-a".to_string(), "hit".to_string());
        let (items, wall_ms, cpu_ms) = build_compile_items(&events, &outcomes);
        assert_eq!(items.len(), 2);
        assert_eq!(wall_ms, 600); // 1_600 - 1_000
        assert_eq!(cpu_ms, 600); // 500 + 100
        let crate_a = items
            .iter()
            .find(|item| item.crate_name == "crate-a")
            .expect("crate-a item");
        assert_eq!(crate_a.cache, "hit");
        let crate_b = items
            .iter()
            .find(|item| item.crate_name == "crate-b")
            .expect("crate-b item");
        assert_eq!(crate_b.cache, "unknown");

        let link = build_link_step(&events);
        assert_eq!(link.items.len(), 1);
        assert_eq!(link.items[0].crate_name, "crate-b");
        assert!(link.derived);
    });
}
