use super::*;
use ZccacheModeSource::{Config, SoldrEnv, ZccacheEnv};

fn resolved(mode: ZccacheMode, source: ZccacheModeSource) -> Option<ResolvedZccacheMode> {
    Some(ResolvedZccacheMode { mode, source })
}

#[test]
fn parse_accepts_each_mode_case_and_whitespace_insensitively() {
    for mode in ZccacheMode::ALL {
        assert_eq!(ZccacheMode::parse(mode.as_str()), Ok(Some(mode)));
        let lower = format!("  {}\n", mode.as_str().to_ascii_lowercase());
        assert_eq!(ZccacheMode::parse(&lower), Ok(Some(mode)), "{lower:?}");
    }
}

#[test]
fn parse_treats_empty_as_unset_and_rejects_unknown_words() {
    assert_eq!(ZccacheMode::parse(""), Ok(None));
    assert_eq!(ZccacheMode::parse("   "), Ok(None));
    for raw in ["hardlink", "cow", "1", "on"] {
        assert_eq!(ZccacheMode::parse(raw), Err(UnknownZccacheMode), "{raw}");
    }
}

/// (SOLDR_ZCCACHE_MODE, config.toml, ZCCACHE_MODE, expected)
type PrecedenceRow = (
    Option<&'static str>,
    Option<&'static str>,
    Option<&'static str>,
    Option<ResolvedZccacheMode>,
);

/// Each tier wins only when every higher tier is absent.
#[test]
fn precedence_table() {
    let rows: [PrecedenceRow; 6] = [
        (
            Some("copy"),
            Some("link"),
            Some("reflink"),
            resolved(ZccacheMode::Copy, SoldrEnv),
        ),
        (
            None,
            Some("link"),
            Some("reflink"),
            resolved(ZccacheMode::Link, Config),
        ),
        (
            None,
            None,
            Some("reflink"),
            resolved(ZccacheMode::Reflink, ZccacheEnv),
        ),
        (None, None, None, None),
        (
            Some("auto"),
            None,
            None,
            resolved(ZccacheMode::Auto, SoldrEnv),
        ),
        (
            None,
            Some("REFLINK"),
            None,
            resolved(ZccacheMode::Reflink, Config),
        ),
    ];
    for (soldr, config, zccache, expected) in rows {
        assert_eq!(
            resolve_zccache_mode_from(soldr, config, zccache),
            Ok(expected),
            "soldr={soldr:?} config={config:?} zccache={zccache:?}"
        );
    }
}

#[test]
fn empty_values_fall_through_to_the_next_tier() {
    assert_eq!(
        resolve_zccache_mode_from(Some(""), Some(" "), Some("copy")),
        Ok(resolved(ZccacheMode::Copy, ZccacheEnv))
    );
}

#[test]
fn an_invalid_value_is_an_error_naming_its_source() {
    for (soldr, config, zccache, source) in [
        (Some("bogus"), None, None, SoldrEnv),
        (None, Some("bogus"), None, Config),
        (None, None, Some("bogus"), ZccacheEnv),
    ] {
        let error = resolve_zccache_mode_from(soldr, config, zccache).unwrap_err();
        assert_eq!(error.source, source);
        let message = error.to_string();
        assert!(message.contains(source.describe()), "{message}");
        assert!(message.contains("\"bogus\""), "{message}");
        assert!(message.contains("AUTO, LINK, COPY, REFLINK"), "{message}");
    }
}

/// A higher tier that is valid shadows an invalid lower tier: only the
/// value that would actually be used can fail the build.
#[test]
fn a_winning_tier_shadows_an_invalid_lower_one() {
    assert_eq!(
        resolve_zccache_mode_from(Some("copy"), Some("bogus"), Some("bogus")),
        Ok(resolved(ZccacheMode::Copy, SoldrEnv))
    );
}

#[test]
fn only_soldr_owned_sources_are_injected_on_the_child() {
    let soldr = ResolvedZccacheMode {
        mode: ZccacheMode::Copy,
        source: SoldrEnv,
    };
    let config = ResolvedZccacheMode {
        mode: ZccacheMode::Reflink,
        source: Config,
    };
    let user = ResolvedZccacheMode {
        mode: ZccacheMode::Link,
        source: ZccacheEnv,
    };
    assert_eq!(soldr.child_env_value(), Some("COPY"));
    assert_eq!(config.child_env_value(), Some("REFLINK"));
    assert_eq!(user.child_env_value(), None);
}

#[test]
fn config_toml_zccache_mode_parses() {
    let parsed: crate::core::SoldrConfig =
        toml::from_str("[zccache]\nmode = \"reflink\"\n").unwrap();
    assert_eq!(parsed.zccache.mode.as_deref(), Some("reflink"));
    let empty: crate::core::SoldrConfig = toml::from_str("").unwrap();
    assert_eq!(empty.zccache, ZccacheModeConfig::default());
}
