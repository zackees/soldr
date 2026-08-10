//! v2 broker HTTP port mode resolution (slice 9 of #488).
//!
//! Implements the three broker port modes from #483 §3 plus the Docker
//! env overrides from #483 §3 ("Env override for Docker / sidecar
//! deployments"). Single resolution point — `BrokerHttpPort::resolve` —
//! so the env-override surface is visible exactly once in the code path
//! and the rest of the broker handles only the resolved enum.

use std::env;
use std::net::{IpAddr, Ipv4Addr};

/// Broker HTTP port mode declared in `BrokerConfig`.
///
/// Per #483 §3, the v2 broker's HTTP server picks its port via one of
/// these strategies. `BrokerHttpPort::resolve` overlays the env vars
/// from §3's table so container deployments can pin the port from
/// outside the binary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrokerHttpPort {
    /// Bind exactly this port; fail if unavailable.
    Static {
        /// The port the operator wants.
        port: u16,
    },
    /// Always bind whatever the OS gives us (`bind(0)` semantics).
    Dynamic,
    /// Try `preferred`; if EADDRINUSE, fall back to OS-allocated.
    StaticOrFallback {
        /// The preferred port. Falls back to OS-allocated when taken.
        preferred: u16,
    },
}

/// Env var that overrides the configured port — when set & parseable,
/// resolution collapses to [`BrokerHttpPort::Static`] regardless of
/// the surrounding `BrokerConfig`.
pub const PORT_OVERRIDE_ENV: &str = "RUNNING_PROCESS_BROKER_HTTP_PORT";

/// Env var that overrides the bound IP. Defaults to `127.0.0.1`.
pub const BIND_OVERRIDE_ENV: &str = "RUNNING_PROCESS_BROKER_HTTP_BIND";

/// Resolved bind state — single source of truth for the rest of the
/// broker after [`BrokerHttpPort::resolve`] runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedHttpBind {
    /// The port mode after env override.
    pub port: BrokerHttpPort,
    /// The IP to bind on (defaults to loopback).
    pub addr: IpAddr,
}

impl BrokerHttpPort {
    /// Resolve config + env into the canonical bind state.
    ///
    /// Precedence:
    /// 1. If `RUNNING_PROCESS_BROKER_HTTP_PORT` is set and parses as a
    ///    `u16` → return [`BrokerHttpPort::Static`] for the override
    ///    (no silent fallback — defeating the container port-mapping
    ///    is the user's whole reason for setting it).
    /// 2. Otherwise → return `config` unchanged.
    /// 3. If `RUNNING_PROCESS_BROKER_HTTP_BIND` is set and parses as
    ///    an `IpAddr` → use that; otherwise default `127.0.0.1`.
    /// 4. Empty / invalid env values are treated as unset (config wins).
    pub fn resolve(config: BrokerHttpPort) -> ResolvedHttpBind {
        let port = match parse_port_env() {
            Some(p) => BrokerHttpPort::Static { port: p },
            None => config,
        };
        let addr = parse_bind_env().unwrap_or(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)));
        ResolvedHttpBind { port, addr }
    }
}

fn parse_port_env() -> Option<u16> {
    let raw = env::var(PORT_OVERRIDE_ENV).ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    trimmed.parse::<u16>().ok()
}

fn parse_bind_env() -> Option<IpAddr> {
    let raw = env::var(BIND_OVERRIDE_ENV).ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    trimmed.parse::<IpAddr>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // The env mutation tests share global state (`std::env`). Serialize
    // them through a mutex so parallel test threads can't trample each
    // other's env state.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn with_env<F: FnOnce()>(port: Option<&str>, bind: Option<&str>, f: F) {
        let _g = ENV_LOCK.lock().expect("env mutex poisoned");
        // Save + clear.
        let prev_port = env::var(PORT_OVERRIDE_ENV).ok();
        let prev_bind = env::var(BIND_OVERRIDE_ENV).ok();
        match port {
            Some(p) => env::set_var(PORT_OVERRIDE_ENV, p),
            None => env::remove_var(PORT_OVERRIDE_ENV),
        }
        match bind {
            Some(b) => env::set_var(BIND_OVERRIDE_ENV, b),
            None => env::remove_var(BIND_OVERRIDE_ENV),
        }
        f();
        // Restore.
        match prev_port {
            Some(p) => env::set_var(PORT_OVERRIDE_ENV, p),
            None => env::remove_var(PORT_OVERRIDE_ENV),
        }
        match prev_bind {
            Some(b) => env::set_var(BIND_OVERRIDE_ENV, b),
            None => env::remove_var(BIND_OVERRIDE_ENV),
        }
    }

    #[test]
    fn no_env_returns_config_and_loopback_default() {
        with_env(None, None, || {
            let r = BrokerHttpPort::resolve(BrokerHttpPort::Dynamic);
            assert_eq!(r.port, BrokerHttpPort::Dynamic);
            assert_eq!(r.addr, IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)));
        });
    }

    #[test]
    fn port_env_set_overrides_to_static() {
        with_env(Some("8080"), None, || {
            let r = BrokerHttpPort::resolve(BrokerHttpPort::StaticOrFallback { preferred: 12_345 });
            assert_eq!(r.port, BrokerHttpPort::Static { port: 8080 });
        });
    }

    #[test]
    fn bind_env_set_overrides_addr() {
        with_env(None, Some("0.0.0.0"), || {
            let r = BrokerHttpPort::resolve(BrokerHttpPort::Static { port: 4242 });
            assert_eq!(r.port, BrokerHttpPort::Static { port: 4242 });
            assert_eq!(r.addr, IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)));
        });
    }

    #[test]
    fn invalid_port_env_falls_back_to_config() {
        with_env(Some("not-a-port"), None, || {
            let r = BrokerHttpPort::resolve(BrokerHttpPort::Dynamic);
            assert_eq!(r.port, BrokerHttpPort::Dynamic);
        });
    }

    #[test]
    fn empty_port_env_falls_back_to_config() {
        with_env(Some(""), None, || {
            let r = BrokerHttpPort::resolve(BrokerHttpPort::Dynamic);
            assert_eq!(r.port, BrokerHttpPort::Dynamic);
        });
    }

    #[test]
    fn invalid_bind_env_falls_back_to_loopback() {
        with_env(None, Some("not-an-ip"), || {
            let r = BrokerHttpPort::resolve(BrokerHttpPort::Dynamic);
            assert_eq!(r.addr, IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)));
        });
    }

    #[test]
    fn both_env_overrides_compose() {
        with_env(Some("9999"), Some("0.0.0.0"), || {
            let r = BrokerHttpPort::resolve(BrokerHttpPort::Dynamic);
            assert_eq!(r.port, BrokerHttpPort::Static { port: 9999 });
            assert_eq!(r.addr, IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)));
        });
    }
}
