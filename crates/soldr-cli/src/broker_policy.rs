//! Fail-closed Soldr-owned policy for the one broker-retirement exception.
//!
//! The broker remains a stable singleton by default. soldr#2920 permits a
//! strictly newer client to retire only the demonstrated, incompatible 0.9.0
//! generation; lifecycle ownership and process verification stay elsewhere.

use semver::{Version, VersionReq};

/// The sole initial incompatible generation named in soldr#2920. Keep this
/// exact SemVer requirement separate from the ordinary stable-singleton rule;
/// widening it requires new incident evidence and an issue decision.
pub(crate) const KNOWN_BAD_BROKER_RANGE: &str = "=0.9.0";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct KnownBadBrokerRetirement {
    pub(crate) observed_version: Version,
    pub(crate) client_version: Version,
    pub(crate) matched_range: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BrokerInstanceDecision {
    RetireKnownBadOlder(KnownBadBrokerRetirement),
    WarnOnly,
}

/// Classify a reported broker identity against this client's package version.
///
/// Identities are deliberately strict: `soldr-<semver>-<64 lowercase hex>`.
/// A malformed identity, an unparseable local package version, a same/newer
/// broker, or an older version outside the explicit range all fail closed.
pub(crate) fn classify_broker_instance(
    observed_identity: &str,
    client_version: &str,
) -> BrokerInstanceDecision {
    let Some(observed_version) = parse_broker_instance(observed_identity) else {
        return BrokerInstanceDecision::WarnOnly;
    };
    let Ok(client_version) = Version::parse(client_version) else {
        return BrokerInstanceDecision::WarnOnly;
    };
    let known_bad = VersionReq::parse(KNOWN_BAD_BROKER_RANGE)
        .expect("the compiled known-bad broker range is valid SemVer");
    if observed_version < client_version && known_bad.matches(&observed_version) {
        BrokerInstanceDecision::RetireKnownBadOlder(KnownBadBrokerRetirement {
            observed_version,
            client_version,
            matched_range: KNOWN_BAD_BROKER_RANGE,
        })
    } else {
        BrokerInstanceDecision::WarnOnly
    }
}

fn parse_broker_instance(value: &str) -> Option<Version> {
    let value = value.strip_prefix("soldr-")?;
    let (version, digest) = value.rsplit_once('-')?;
    (digest.len() == 64
        && digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f')))
    .then_some(())?;
    let version = Version::parse(version).ok()?;
    // SemVer precedence deliberately ignores build metadata, but broker
    // identity is an ownership boundary rather than a release comparison.
    // Accepting `0.9.0+anything` here would silently widen soldr#2920's
    // incident-backed exact generation to unproven images.
    version.build.is_empty().then_some(version)
}

#[cfg(test)]
mod tests {
    use super::{classify_broker_instance, BrokerInstanceDecision, KNOWN_BAD_BROKER_RANGE};

    fn identity(version: &str) -> String {
        format!("soldr-{version}-{}", "a".repeat(64))
    }

    #[test]
    fn only_a_strictly_newer_client_retires_the_explicit_known_bad_release() {
        assert_eq!(KNOWN_BAD_BROKER_RANGE, "=0.9.0");
        let BrokerInstanceDecision::RetireKnownBadOlder(candidate) =
            classify_broker_instance(&identity("0.9.0"), "0.9.1")
        else {
            panic!("0.9.1 must retire the declared incompatible 0.9.0 broker");
        };
        assert_eq!(candidate.observed_version.to_string(), "0.9.0");
        assert_eq!(candidate.client_version.to_string(), "0.9.1");
        assert_eq!(candidate.matched_range, KNOWN_BAD_BROKER_RANGE);
    }

    #[test]
    fn equal_newer_safe_older_prerelease_build_and_malformed_identities_warn_only() {
        for (observed, client) in [
            (identity("0.9.0"), "0.9.0"),
            (identity("0.9.1"), "0.9.0"),
            (identity("0.9.2"), "0.9.1"),
            (identity("0.8.9"), "0.9.1"),
            (identity("0.9.0-alpha.1"), "0.9.1"),
            (identity("0.9.0+build"), "0.9.1"),
            (identity("0.9.0+legacy"), "0.9.0+current"),
            ("soldr-0.9.0-not-a-digest".into(), "0.9.1"),
            (format!("soldr-0.9.0-{}", "A".repeat(64)), "0.9.1"),
            ("foreign-process".into(), "0.9.1"),
            (identity("0.9.0"), "not-semver"),
        ] {
            assert_eq!(
                classify_broker_instance(&observed, client),
                BrokerInstanceDecision::WarnOnly,
                "must remain warning-only: observed={observed}, client={client}"
            );
        }
    }

    #[test]
    fn semver_precedence_allows_a_newer_prerelease_but_never_build_only_drift() {
        assert!(matches!(
            classify_broker_instance(&identity("0.9.0"), "0.9.1-alpha.1"),
            BrokerInstanceDecision::RetireKnownBadOlder(_)
        ));
        assert_eq!(
            classify_broker_instance(&identity("0.9.0+old"), "0.9.0+new"),
            BrokerInstanceDecision::WarnOnly
        );
    }
}
