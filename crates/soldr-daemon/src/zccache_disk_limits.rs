//! Embedded artifact-budget configuration (split out of zccache_embedded.rs).

use super::{DiskCacheLimits, EmbeddedDiskPolicy, EmbeddedServiceError};

/// Artifact-budget env names (upstream's are `pub(crate)`); the daemon spawn
/// allowlist forwards them by path (soldr#3503).
pub(crate) const CACHE_SIZE_BYTES_ENV: &str = "ZCCACHE_CACHE_SIZE_BYTES";
pub(crate) const CACHE_SIZE_PERCENT_ENV: &str = "ZCCACHE_CACHE_SIZE_PERCENT";

pub(super) fn disk_cache_limits_from_env(
) -> Result<(DiskCacheLimits, EmbeddedDiskPolicy), EmbeddedServiceError> {
    let bytes_raw = std::env::var(CACHE_SIZE_BYTES_ENV)
        .ok()
        .filter(|value| !value.trim().is_empty());
    let percent_raw = std::env::var(CACHE_SIZE_PERCENT_ENV)
        .ok()
        .filter(|value| !value.trim().is_empty());
    disk_cache_limits_from_values(bytes_raw.as_deref(), percent_raw.as_deref())
}

pub(super) fn disk_cache_limits_from_values(
    bytes_raw: Option<&str>,
    percent_raw: Option<&str>,
) -> Result<(DiskCacheLimits, EmbeddedDiskPolicy), EmbeddedServiceError> {
    if bytes_raw.is_some() && percent_raw.is_some() {
        return Err(EmbeddedServiceError::Start(format!(
            "{CACHE_SIZE_BYTES_ENV} and {CACHE_SIZE_PERCENT_ENV} are mutually exclusive"
        )));
    }
    let max_cache_bytes = bytes_raw
        .map(|value| {
            value.parse::<u64>().map_err(|_| {
                EmbeddedServiceError::Start(format!(
                    "{CACHE_SIZE_BYTES_ENV} must be a positive integer byte count"
                ))
            })
        })
        .transpose()?;
    if max_cache_bytes == Some(0) {
        return Err(EmbeddedServiceError::Start(format!(
            "{CACHE_SIZE_BYTES_ENV} must be greater than zero"
        )));
    }
    let max_cache_percent = percent_raw
        .map(|value| {
            value.parse::<u8>().map_err(|_| {
                EmbeddedServiceError::Start(format!(
                    "{CACHE_SIZE_PERCENT_ENV} must be an integer from 1 through 100"
                ))
            })
        })
        .transpose()?;
    if max_cache_percent.is_some_and(|percent| !(1..=100).contains(&percent)) {
        return Err(EmbeddedServiceError::Start(format!(
            "{CACHE_SIZE_PERCENT_ENV} must be an integer from 1 through 100"
        )));
    }
    let source = if max_cache_bytes.is_some() {
        "explicit_bytes"
    } else if max_cache_percent.is_some() {
        "explicit_percent"
    } else {
        "dynamic_5_percent_clamped_40_200_gib"
    };
    Ok((
        DiskCacheLimits {
            max_cache_bytes,
            max_cache_percent,
        },
        EmbeddedDiskPolicy {
            source: source.to_string(),
            max_cache_bytes,
            max_cache_percent,
        },
    ))
}

#[cfg(test)]
mod disk_limit_tests {
    use super::*;

    #[test]
    fn disk_limit_overrides_are_validated_and_mutually_exclusive() {
        let (_, dynamic) = disk_cache_limits_from_values(None, None).unwrap();
        assert_eq!(dynamic.source, "dynamic_5_percent_clamped_40_200_gib");
        let (_, bytes) = disk_cache_limits_from_values(Some("42949672960"), None).unwrap();
        assert_eq!(bytes.max_cache_bytes, Some(40 * 1024 * 1024 * 1024));
        let (_, percent) = disk_cache_limits_from_values(None, Some("7")).unwrap();
        assert_eq!(percent.max_cache_percent, Some(7));
        assert!(disk_cache_limits_from_values(Some("1"), Some("5")).is_err());
        assert!(disk_cache_limits_from_values(Some("0"), None).is_err());
        assert!(disk_cache_limits_from_values(None, Some("101")).is_err());
    }
}
