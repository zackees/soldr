//! Version floor and allow-list enforcement for service definitions.

use std::cmp::Ordering;

use crate::broker::protocol::ServiceDefinition;

/// Reason a requested service version is blocked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VersionPolicyBlock {
    /// The requested version is below `ServiceDefinition.min_version`.
    BelowMinVersion,
    /// The service definition has a strict allow-list and the request is absent.
    OutsideAllowList,
}

/// Check `wanted_version` against the service definition's frozen policy.
pub fn check_version_allowed(
    wanted_version: &str,
    service: &ServiceDefinition,
) -> Result<(), VersionPolicyBlock> {
    if !service.min_version.is_empty()
        && compare_semver_core(wanted_version, &service.min_version) == Some(Ordering::Less)
    {
        return Err(VersionPolicyBlock::BelowMinVersion);
    }
    if !service.version_allow_list.is_empty()
        && !service
            .version_allow_list
            .iter()
            .any(|allowed| allowed == wanted_version)
    {
        return Err(VersionPolicyBlock::OutsideAllowList);
    }
    Ok(())
}

fn compare_semver_core(left: &str, right: &str) -> Option<Ordering> {
    let left = parse_semver_core(left)?;
    let right = parse_semver_core(right)?;
    Some(left.cmp(&right))
}

fn parse_semver_core(version: &str) -> Option<[u64; 3]> {
    let core = version.split_once('-').map_or(version, |(core, _)| core);
    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some([major, minor, patch])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn service(min_version: &str, allow: &[&str]) -> ServiceDefinition {
        ServiceDefinition {
            service_name: "zccache".into(),
            binary_path: "/usr/bin/zccache".into(),
            isolation: 0,
            explicit_instance: String::new(),
            per_version_binary_dir: String::new(),
            min_version: min_version.into(),
            version_allow_list: allow.iter().map(|v| (*v).into()).collect(),
            labels: Default::default(),
        }
    }

    #[test]
    fn blocks_version_below_floor() {
        assert_eq!(
            check_version_allowed("1.9.9", &service("1.10.0", &[])),
            Err(VersionPolicyBlock::BelowMinVersion)
        );
    }

    #[test]
    fn allows_version_at_floor() {
        assert_eq!(
            check_version_allowed("1.10.0", &service("1.10.0", &[])),
            Ok(())
        );
    }

    #[test]
    fn blocks_version_outside_allow_list() {
        assert_eq!(
            check_version_allowed("1.12.0", &service("1.10.0", &["1.11.20"])),
            Err(VersionPolicyBlock::OutsideAllowList)
        );
    }

    #[test]
    fn allows_version_inside_allow_list() {
        assert_eq!(
            check_version_allowed("1.11.20", &service("1.10.0", &["1.11.20"])),
            Ok(())
        );
    }
}
