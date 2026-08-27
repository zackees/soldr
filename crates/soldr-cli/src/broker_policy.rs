//! Pure, fail-closed policy for the single reviewed broker-retirement exception.
//!
//! Broker lifecycle stays in `broker_spawn`; this module only classifies a
//! status identity so malformed, same-version, and newer identities remain
//! warning-only. The exact exception is soldr#2920: a `0.9.0` broker older
//! than this client may be retired through the separately fenced lifecycle.

/// The only lifecycle decision the broker policy may authorize.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BrokerInstanceDecision {
    RetireKnownBadOlder,
    WarnOnly,
}

/// Classify a running broker identity against the current client's identity.
///
/// Identity syntax is deliberately strict: `soldr-<semver>-<64 lowercase hex>`.
/// Any malformed identity is fail-closed and remains warning-only.
pub(crate) fn classify_broker_instance(
    observed: &str,
    expected: &str,
) -> BrokerInstanceDecision {
    let Some(observed) = parse_broker_instance(observed) else {
        return BrokerInstanceDecision::WarnOnly;
    };
    let Some(expected) = parse_broker_instance(expected) else {
        return BrokerInstanceDecision::WarnOnly;
    };
    if observed == semver::Version::new(0, 9, 0) && observed < expected {
        BrokerInstanceDecision::RetireKnownBadOlder
    } else {
        BrokerInstanceDecision::WarnOnly
    }
}

fn parse_broker_instance(value: &str) -> Option<semver::Version> {
    let value = value.strip_prefix("soldr-")?;
    let (version, digest) = value.rsplit_once('-')?;
    (digest.len() == 64
        && digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f')))
    .then_some(())?;
    semver::Version::parse(version).ok()
}

#[cfg(test)]
mod tests {
    use super::{classify_broker_instance, BrokerInstanceDecision};

    #[test]
    fn known_bad_retirement_policy_is_strict_and_one_directional() {
        let digest = "0".repeat(64);
        let client = format!("soldr-0.9.1-{}", "1".repeat(64));
        assert_eq!(
            classify_broker_instance(&format!("soldr-0.9.0-{digest}"), &client),
            BrokerInstanceDecision::RetireKnownBadOlder
        );
        for observed in [
            format!("soldr-0.9.1-{digest}"),
            format!("soldr-0.9.2-{digest}"),
            format!("soldr-0.8.9-{digest}"),
            "soldr-0.9.0-not-a-digest".into(),
            format!("soldr-0.9.0-{}", "A".repeat(64)),
            "not-a-broker-instance".into(),
        ] {
            assert_eq!(
                classify_broker_instance(&observed, &client),
                BrokerInstanceDecision::WarnOnly,
                "must remain warning-only: {observed}"
            );
        }
    }
}
