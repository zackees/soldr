//! Historical root-local daemon endpoint derivation.
//!
//! Broker-owned daemons use private route endpoints. A daemon released before
//! that transition still holds the root-local Unix socket or Windows pipe, so
//! an upgraded client needs the exact old derivation to retire it gracefully.
//! The Windows pipe derivation (token SID + cache-root hash) lives in the
//! platform ipc endpoint leaf; the Unix filesystem derivation stays here.

use crate::core::SoldrPaths;
use std::path::PathBuf;

pub(super) fn resolve(paths: &SoldrPaths) -> Result<PathBuf, String> {
    if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows {
        Ok(PathBuf::from(
            crate::platform::ipc::endpoint::legacy_daemon_endpoint(&paths.cache)?,
        ))
    } else {
        use std::hash::{Hash as _, Hasher as _};

        let preferred = crate::cache_lib::soldr_daemon_dir(paths).join("sock");
        if preferred.as_os_str().len() <= 100 {
            return Ok(preferred);
        }
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        paths.cache.hash(&mut hasher);
        let suffix = format!("{:016x}", hasher.finish());
        let tmp = std::env::var_os("TMPDIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/tmp"));
        Ok(tmp.join(format!("sd-{}.sock", &suffix[..12])))
    }
}
