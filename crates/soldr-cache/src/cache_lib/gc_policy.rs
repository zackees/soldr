//! Shared GC decisions for the CLI sweeper and daemon maintenance loop.
//!
//! The policy is deliberately a small, pure planning layer.  The callers
//! still own locks, database handles, filesystem work, and result mapping;
//! this module owns the category registry, retention constants, gates, and
//! cross-category order (soldr#2312).

use crate::core::SoldrConfig;
use std::time::Duration;

/// The maintenance cadence selected by a driver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TickKind {
    Pressure,
    Full,
}

/// Which executor is asking for a plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Driver {
    Cli,
    Daemon,
    Both,
}

/// Static cost-to-restore ordering. Lower tiers are evicted first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum CostTier {
    Scratch,
    Trash,
    NetworkRefetch,
    EventLog,
    BuildHistory,
    Pep517Build,
    CookArtifact,
    WorkspaceTarget,
    CompileCache,
    LegacyRoot,
}

/// Where a category records its last use. This is descriptive in v1; the
/// executor remains responsible for reading the corresponding source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LastUseSource {
    Mtime,
    DbField,
    Registry,
    Internal,
}

/// Age, size, and keep-newest rules for one category.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetentionPolicy {
    pub pressure_age: Option<Duration>,
    pub full_age: Option<Duration>,
    pub size_cap: Option<u64>,
    pub min_age_floor: Duration,
    pub keep_newest: u32,
}

/// One registry entry. Adding a category is intentionally a single entry.
#[derive(Debug, Clone, Copy)]
pub struct GcCategory {
    pub id: &'static str,
    pub label: &'static str,
    pub cost_tier: CostTier,
    pub retention: RetentionPolicy,
    pub last_use: LastUseSource,
    pub driver: Driver,
    pub protect: fn(&GcContext) -> bool,
}

/// Inputs used by the pure decision layer.
#[derive(Debug, Clone)]
pub struct GcContext {
    pub driver: Driver,
    pub tick: TickKind,
    pub free_by_volume: Vec<(String, u64)>,
    pub config: SoldrConfig,
    /// The CLI only owns the event database when no daemon owns the root.
    pub daemon_events_available: bool,
    pub daemon_live: bool,
}

/// A concrete decision for an executor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvictionAction {
    pub category_id: &'static str,
    pub label: &'static str,
    pub cost_tier: CostTier,
    pub older_than: Option<Duration>,
    pub larger_than_bytes: u64,
    pub size_cap: Option<u64>,
    pub keep_newest: u32,
}

const DAY: u64 = 24 * 60 * 60;
const HOUR: u64 = 60 * 60;
const GIB: u64 = 1024 * 1024 * 1024;

fn always(_: &GcContext) -> bool {
    true
}

fn pressure_or_full(ctx: &GcContext) -> bool {
    matches!(ctx.tick, TickKind::Pressure | TickKind::Full)
}

fn daemon_events(ctx: &GcContext) -> bool {
    match ctx.driver {
        Driver::Daemon => ctx.tick == TickKind::Full,
        Driver::Cli => ctx.daemon_events_available && !ctx.daemon_live,
        Driver::Both => true,
    }
}

fn cook(ctx: &GcContext) -> bool {
    ctx.config.cook.max_total_gb > 0 || ctx.config.cook.max_age_days > 0
}

fn workspace_targets(ctx: &GcContext) -> bool {
    ctx.config.auto_gc.enabled && pressure_or_full(ctx)
}

/// The one source of truth for category metadata and ordering.
pub fn registry() -> Vec<GcCategory> {
    vec![
        GcCategory {
            id: "scratch",
            label: "scratch tmp",
            cost_tier: CostTier::Scratch,
            retention: RetentionPolicy {
                pressure_age: Some(Duration::from_secs(DAY)),
                full_age: Some(Duration::from_secs(DAY)),
                size_cap: None,
                min_age_floor: Duration::ZERO,
                keep_newest: 0,
            },
            last_use: LastUseSource::Mtime,
            driver: Driver::Cli,
            protect: always,
        },
        GcCategory {
            id: "trash",
            label: "release-worktree trash",
            cost_tier: CostTier::Trash,
            retention: RetentionPolicy {
                pressure_age: None,
                full_age: None,
                size_cap: None,
                min_age_floor: Duration::ZERO,
                keep_newest: 0,
            },
            last_use: LastUseSource::Mtime,
            driver: Driver::Both,
            protect: always,
        },
        GcCategory {
            id: "install_source",
            label: "install source cache",
            cost_tier: CostTier::NetworkRefetch,
            retention: RetentionPolicy {
                pressure_age: Some(Duration::from_secs(HOUR)),
                full_age: Some(Duration::from_secs(2 * DAY)),
                size_cap: None,
                min_age_floor: Duration::from_secs(HOUR),
                keep_newest: 0,
            },
            last_use: LastUseSource::Mtime,
            driver: Driver::Cli,
            protect: always,
        },
        GcCategory {
            id: "daemon_events",
            label: "daemon events",
            cost_tier: CostTier::EventLog,
            retention: RetentionPolicy {
                pressure_age: None,
                full_age: Some(Duration::from_secs(30 * DAY)),
                size_cap: None,
                min_age_floor: Duration::ZERO,
                keep_newest: 0,
            },
            last_use: LastUseSource::DbField,
            driver: Driver::Both,
            protect: daemon_events,
        },
        GcCategory {
            id: "history",
            label: "build history",
            cost_tier: CostTier::BuildHistory,
            retention: RetentionPolicy {
                pressure_age: Some(Duration::from_secs(4 * DAY)),
                full_age: Some(Duration::from_secs(4 * DAY)),
                size_cap: Some(GIB),
                min_age_floor: Duration::ZERO,
                keep_newest: 0,
            },
            last_use: LastUseSource::Mtime,
            driver: Driver::Daemon,
            protect: always,
        },
        GcCategory {
            id: "pep517_targets",
            label: "pep517 targets",
            cost_tier: CostTier::Pep517Build,
            retention: RetentionPolicy {
                pressure_age: Some(Duration::from_secs(4 * DAY)),
                full_age: Some(Duration::from_secs(30 * DAY)),
                size_cap: None,
                min_age_floor: Duration::ZERO,
                keep_newest: 3,
            },
            last_use: LastUseSource::Mtime,
            driver: Driver::Daemon,
            protect: pressure_or_full,
        },
        GcCategory {
            id: "pep517_wheels",
            label: "pep517 wheels",
            cost_tier: CostTier::Pep517Build,
            retention: RetentionPolicy {
                pressure_age: Some(Duration::from_secs(4 * DAY)),
                full_age: Some(Duration::from_secs(30 * DAY)),
                size_cap: None,
                min_age_floor: Duration::ZERO,
                keep_newest: 3,
            },
            last_use: LastUseSource::Mtime,
            driver: Driver::Daemon,
            protect: pressure_or_full,
        },
        GcCategory {
            id: "cook",
            label: "cook artifacts",
            cost_tier: CostTier::CookArtifact,
            retention: RetentionPolicy {
                pressure_age: None,
                full_age: Some(Duration::from_secs(30 * DAY)),
                size_cap: Some(10 * GIB),
                min_age_floor: Duration::ZERO,
                keep_newest: 3,
            },
            last_use: LastUseSource::DbField,
            driver: Driver::Both,
            protect: cook,
        },
        GcCategory {
            id: "workspace_targets",
            label: "workspace target dirs",
            cost_tier: CostTier::WorkspaceTarget,
            retention: RetentionPolicy {
                pressure_age: Some(Duration::from_secs(4 * DAY)),
                full_age: Some(Duration::from_secs(30 * DAY)),
                size_cap: None,
                min_age_floor: Duration::from_secs(60 * 60),
                keep_newest: 0,
            },
            last_use: LastUseSource::Registry,
            driver: Driver::Both,
            protect: workspace_targets,
        },
        GcCategory {
            id: "zccache_compile",
            label: "embedded compile cache",
            cost_tier: CostTier::CompileCache,
            retention: RetentionPolicy {
                pressure_age: None,
                full_age: None,
                size_cap: None,
                min_age_floor: Duration::ZERO,
                keep_newest: 0,
            },
            last_use: LastUseSource::Internal,
            driver: Driver::Daemon,
            protect: always,
        },
        GcCategory {
            id: "legacy_zccache",
            label: "legacy cache roots",
            cost_tier: CostTier::LegacyRoot,
            retention: RetentionPolicy {
                pressure_age: Some(Duration::from_secs(4 * DAY)),
                full_age: Some(Duration::from_secs(30 * DAY)),
                size_cap: None,
                min_age_floor: Duration::ZERO,
                keep_newest: 0,
            },
            last_use: LastUseSource::Mtime,
            driver: Driver::Daemon,
            protect: pressure_or_full,
        },
    ]
}

/// Build actions in canonical cost-to-restore order.
pub fn plan(categories: &[GcCategory], ctx: &GcContext) -> Vec<EvictionAction> {
    let mut actions: Vec<_> = categories
        .iter()
        .filter(|category| category.driver == ctx.driver || category.driver == Driver::Both)
        .filter(|category| (category.protect)(ctx))
        .map(|category| {
            let raw_age = match ctx.tick {
                TickKind::Pressure => category.retention.pressure_age,
                TickKind::Full => category.retention.full_age,
            };
            EvictionAction {
                category_id: category.id,
                label: category.label,
                cost_tier: category.cost_tier,
                older_than: raw_age.map(|age| age.max(category.retention.min_age_floor)),
                larger_than_bytes: if category.id == "workspace_targets" {
                    256 * 1024 * 1024
                } else {
                    0
                },
                size_cap: category.retention.size_cap,
                keep_newest: category.retention.keep_newest,
            }
        })
        .collect();
    actions.sort_by_key(|action| action.cost_tier);
    actions
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context(driver: Driver, tick: TickKind) -> GcContext {
        GcContext {
            driver,
            tick,
            free_by_volume: Vec::new(),
            config: SoldrConfig::default(),
            daemon_events_available: true,
            daemon_live: false,
        }
    }

    crate::timed_test!(plan_full_tick_matches_registry_order, {
        let actions = plan(&registry(), &context(Driver::Daemon, TickKind::Full));
        let ids: Vec<_> = actions.iter().map(|action| action.category_id).collect();
        assert_eq!(
            ids,
            vec![
                "trash",
                "daemon_events",
                "history",
                "pep517_targets",
                "pep517_wheels",
                "cook",
                "workspace_targets",
                "zccache_compile",
                "legacy_zccache",
            ]
        );
    });

    crate::timed_test!(plan_encodes_current_constants_exactly, {
        let actions = plan(&registry(), &context(Driver::Daemon, TickKind::Full));
        let by = |id| {
            actions
                .iter()
                .find(|action| action.category_id == id)
                .unwrap()
        };
        assert_eq!(by("history").older_than, Some(Duration::from_secs(4 * DAY)));
        assert_eq!(by("history").size_cap, Some(GIB));
        assert_eq!(
            by("pep517_targets").older_than,
            Some(Duration::from_secs(30 * DAY))
        );
        assert_eq!(by("pep517_targets").keep_newest, 3);
        assert_eq!(
            by("daemon_events").older_than,
            Some(Duration::from_secs(30 * DAY))
        );
        assert_eq!(
            by("workspace_targets").older_than,
            Some(Duration::from_secs(30 * DAY))
        );
        assert_eq!(by("workspace_targets").larger_than_bytes, 256 * 1024 * 1024);
    });

    crate::timed_test!(plan_pressure_tick_uses_pressure_ages, {
        let actions = plan(&registry(), &context(Driver::Daemon, TickKind::Pressure));
        let by = |id| {
            actions
                .iter()
                .find(|action| action.category_id == id)
                .unwrap()
        };
        assert_eq!(
            by("pep517_targets").older_than,
            Some(Duration::from_secs(4 * DAY))
        );
        assert_eq!(
            by("workspace_targets").older_than,
            Some(Duration::from_secs(4 * DAY))
        );
        assert!(actions
            .iter()
            .all(|action| action.category_id != "daemon_events"));
    });

    crate::timed_test!(plan_gates_workspace_targets_when_auto_gc_is_disabled, {
        let mut ctx = context(Driver::Daemon, TickKind::Full);
        ctx.config.auto_gc.enabled = false;
        assert!(plan(&registry(), &ctx)
            .iter()
            .all(|action| action.category_id != "workspace_targets"));
    });

    crate::timed_test!(plan_cli_keeps_event_and_install_order, {
        let actions = plan(&registry(), &context(Driver::Cli, TickKind::Pressure));
        let ids: Vec<_> = actions.iter().map(|action| action.category_id).collect();
        assert_eq!(
            ids,
            vec![
                "scratch",
                "trash",
                "install_source",
                "daemon_events",
                "cook",
                "workspace_targets",
            ]
        );
    });
}
