//! Shared process-environment scope for catalogue integration tests.
//!
//! Catalogue and syslib origin overrides are read by production code, so
//! callers must serialize mutations within their consolidated test binary and
//! restore the caller's environment even when an assertion panics.

use std::ffi::OsString;
use std::sync::{Mutex, MutexGuard};

const MANIFEST_ENV_VARS: [&str; 5] = [
    "SOLDR_MANIFEST_DISABLE",
    "SOLDR_TOOLCHAIN_CATALOGUE_URL",
    "SOLDR_TOOLCHAIN_CATALOGUE_V2_URL",
    "SOLDR_TOOLCHAIN_ORIGIN",
    "SOLDR_SYSLIB_ASSET_ORIGIN",
];

static MANIFEST_ENV_LOCK: Mutex<()> = Mutex::new(());

pub(crate) fn lock() -> MutexGuard<'static, ()> {
    MANIFEST_ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub(crate) struct EnvScope {
    saved: Vec<(&'static str, Option<OsString>)>,
}

impl EnvScope {
    pub(crate) fn capture() -> Self {
        Self {
            saved: MANIFEST_ENV_VARS
                .into_iter()
                .map(|name| (name, std::env::var_os(name)))
                .collect(),
        }
    }
}

impl Drop for EnvScope {
    fn drop(&mut self) {
        for (name, value) in &self.saved {
            if let Some(value) = value {
                std::env::set_var(name, value);
            } else {
                std::env::remove_var(name);
            }
        }
    }
}
