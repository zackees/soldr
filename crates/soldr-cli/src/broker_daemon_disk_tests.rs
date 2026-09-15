//! Unit tests for broker daemon-disk reclamation (soldr#3251).

use super::*;
use filetime::{set_file_mtime, FileTime};
use std::path::PathBuf;

const MINUTE: u64 = 60;
const HOUR: u64 = 60 * MINUTE;

struct Fixture {
    _temp: tempfile::TempDir,
    routes: PathBuf,
    services: PathBuf,
    now: SystemTime,
}

fn fixture() -> Fixture {
    let temp = tempfile::tempdir().expect("tempdir");
    let routes = temp.path().join("routes");
    let services = temp.path().join("services");
    fs::create_dir_all(&routes).expect("routes root");
    fs::create_dir_all(&services).expect("services root");
    Fixture {
        _temp: temp,
        routes,
        services,
        // A fixed clock, so every age below is exact rather than racing the
        // real one.
        now: UNIX_EPOCH + Duration::from_secs(1_800_000_000),
    }
}

impl Fixture {
    fn ago(&self, seconds: u64) -> SystemTime {
        self.now - Duration::from_secs(seconds)
    }

    /// A route with one staged image whose ledger was last renewed
    /// `idle_seconds` before `now`, laid out exactly as the broker stages it.
    fn route(&self, service: &str, idle_seconds: u64) -> PathBuf {
        let route = self.routes.join(service);
        let image_dir = route.join("runtime").join("soldr-daemon").join("v0.9.16");
        fs::create_dir_all(&image_dir).expect("image dir");
        fs::write(image_dir.join("soldr-daemon"), vec![0_u8; 4096]).expect("image");
        let used = self.ago(idle_seconds);
        let seconds = used.duration_since(UNIX_EPOCH).unwrap().as_secs();
        fs::write(image_dir.join("last-used"), seconds.to_string()).expect("ledger");
        set_file_mtime(&route, FileTime::from_system_time(used)).expect("route mtime");
        route
    }

    /// A route directory with no staged image, created `age_seconds` ago.
    fn empty_route(&self, service: &str, age_seconds: u64) -> PathBuf {
        let route = self.routes.join(service);
        fs::create_dir_all(&route).expect("route dir");
        set_file_mtime(&route, FileTime::from_system_time(self.ago(age_seconds)))
            .expect("route mtime");
        route
    }

    fn registration(&self, service: &str, age_seconds: u64) -> PathBuf {
        let path = self.services.join(format!("{service}.servicedef.v2"));
        fs::write(&path, b"definition").expect("registration");
        set_file_mtime(&path, FileTime::from_system_time(self.ago(age_seconds)))
            .expect("registration mtime");
        path
    }

    fn sweep(&self, live: &[&str]) -> DaemonDiskReport {
        sweep_daemon_disk(
            &self.routes,
            &self.services,
            &live_set(live),
            self.now,
            DaemonDiskPolicy::default(),
        )
    }
}

fn live_set(live: &[&str]) -> BTreeSet<String> {
    live.iter().map(|service| (*service).to_owned()).collect()
}

/// The leak itself: a route nobody has used for longer than the idle TTL
/// held its ~107 MiB image forever. It must go, whole.
#[test]
fn an_idle_route_past_the_ttl_is_removed_whole() {
    let fx = fixture();
    let route = fx.route("soldr-daemon-idle", 7 * HOUR);

    let report = fx.sweep(&[]);

    assert!(
        !route.exists(),
        "the route directory itself must be removed"
    );
    assert_eq!(report.routes_removed, 1);
    assert!(report.bytes_reclaimed >= 4096, "{report:?}");
    assert_eq!(fs::read_dir(&fx.routes).unwrap().count(), 0);
}

#[test]
fn a_live_route_and_its_registration_are_never_removed() {
    let fx = fixture();
    let route = fx.route("soldr-daemon-live", 30 * 24 * HOUR);
    let registration = fx.registration("soldr-daemon-live", 30 * 24 * HOUR);

    let report = fx.sweep(&["soldr-daemon-live"]);

    assert!(route.exists());
    assert!(registration.exists());
    assert!(report.is_empty(), "{report:?}");
}

/// A running daemon renews its ledger, so recent activity is proof enough of
/// use even when the broker's registry does not list the daemon.
#[test]
fn recently_active_routes_are_protected_beyond_the_warm_set() {
    let fx = fixture();
    let keep = DaemonDiskPolicy::default().keep_idle_routes;
    let routes: Vec<PathBuf> = (0..keep + 3)
        .map(|index| fx.route(&format!("soldr-daemon-recent-{index}"), 10 * MINUTE))
        .collect();

    let report = fx.sweep(&[]);

    assert!(routes.iter().all(|route| route.exists()));
    assert!(report.is_empty(), "{report:?}");
}

/// Inside the idle TTL only the most recently used idle routes stay warm.
#[test]
fn only_the_newest_idle_routes_stay_warm_inside_the_ttl() {
    let fx = fixture();
    assert_eq!(DaemonDiskPolicy::default().keep_idle_routes, 4);
    let idle = [
        HOUR,
        2 * HOUR,
        3 * HOUR,
        4 * HOUR,
        5 * HOUR,
        5 * HOUR + 30 * MINUTE,
    ];
    let routes: Vec<PathBuf> = idle
        .iter()
        .enumerate()
        .map(|(index, idle)| fx.route(&format!("soldr-daemon-warm-{index}"), *idle))
        .collect();

    let report = fx.sweep(&[]);

    assert_eq!(report.routes_removed, 2, "{report:?}");
    for route in &routes[..4] {
        assert!(route.exists(), "{} should stay warm", route.display());
    }
    for route in &routes[4..] {
        assert!(!route.exists(), "{} should be reclaimed", route.display());
    }
}

/// 113 route directories on the reporting host had no image at all. With no
/// ledger, the directory's own modification time is the only sign of use.
#[test]
fn a_route_without_an_image_ages_by_its_directory_time() {
    let fx = fixture();
    let stale = fx.empty_route("soldr-daemon-empty-stale", 7 * HOUR);
    let fresh = fx.empty_route("soldr-daemon-empty-fresh", MINUTE);

    let report = fx.sweep(&[]);

    assert!(!stale.exists());
    assert!(fresh.exists());
    assert_eq!(report.routes_removed, 1, "{report:?}");
}

#[test]
fn a_registration_goes_only_with_no_route_no_liveness_and_no_recent_renewal() {
    let fx = fixture();
    let orphaned = fx.registration("soldr-daemon-orphaned", 2 * HOUR);
    let warm_route = fx.route("soldr-daemon-warm", HOUR);
    let warm = fx.registration("soldr-daemon-warm", 2 * HOUR);
    let live = fx.registration("soldr-daemon-live", 2 * HOUR);
    let renewed = fx.registration("soldr-daemon-renewed", 5 * MINUTE);
    let foreign = fx.registration("other-service", 30 * 24 * HOUR);
    let unknown = fx.services.join("notes.txt");
    fs::write(&unknown, b"not a registration").unwrap();
    // A route reclaimed by this sweep takes its registration with it.
    let reclaimed_route = fx.route("soldr-daemon-reclaimed", 7 * HOUR);
    let reclaimed = fx.registration("soldr-daemon-reclaimed", 7 * HOUR);

    let report = fx.sweep(&["soldr-daemon-live"]);

    assert!(!orphaned.exists());
    assert!(!reclaimed_route.exists());
    assert!(!reclaimed.exists());
    assert!(warm_route.exists() && warm.exists());
    assert!(live.exists(), "a live service keeps its registration");
    assert!(
        renewed.exists(),
        "a recently renewed registration is protected"
    );
    assert!(
        foreign.exists(),
        "only soldr daemon registrations are reclaimed"
    );
    assert!(unknown.exists());
    assert_eq!(report.registrations_removed, 2, "{report:?}");
    assert_eq!(report.routes_removed, 1, "{report:?}");
}

#[test]
fn entries_that_are_not_daemon_routes_are_untouched() {
    let fx = fixture();
    let foreign_route = fx.empty_route("other-route", 30 * 24 * HOUR);
    let stray = fx.routes.join("soldr-daemon-stray-file");
    fs::write(&stray, b"x").unwrap();
    set_file_mtime(&stray, FileTime::from_system_time(fx.ago(30 * 24 * HOUR))).unwrap();

    let report = fx.sweep(&[]);

    assert!(foreign_route.exists());
    assert!(stray.exists(), "only real route directories are reclaimed");
    assert!(report.is_empty(), "{report:?}");
}

#[test]
fn a_removal_failure_is_counted_and_the_sweep_continues() {
    let fx = fixture();
    let stuck = fx.route("soldr-daemon-a-stuck", 7 * HOUR);
    let other = fx.route("soldr-daemon-b-other", 8 * HOUR);

    let report = sweep_daemon_disk_with(
        &fx.routes,
        &fx.services,
        &live_set(&[]),
        fx.now,
        DaemonDiskPolicy::default(),
        |path| {
            if path == stuck {
                Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "image in use",
                ))
            } else {
                remove_entry(path)
            }
        },
    );

    assert!(
        stuck.exists(),
        "the failed route is retried by a later sweep"
    );
    assert!(!other.exists(), "one failure must not stop the sweep");
    assert_eq!(report.failed, 1, "{report:?}");
    assert_eq!(report.routes_removed, 1, "{report:?}");
}

#[test]
fn missing_roots_are_nothing_to_do() {
    let fx = fixture();
    let report = sweep_daemon_disk(
        &fx.routes.join("absent"),
        &fx.services.join("absent"),
        &live_set(&[]),
        fx.now,
        DaemonDiskPolicy::default(),
    );
    assert!(report.is_empty(), "{report:?}");
}
