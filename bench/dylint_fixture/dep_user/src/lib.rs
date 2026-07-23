//! Small library crate pulling in a couple of real, moderately-sized
//! external deps (`anyhow`, `serde_json`) so dependency compilation in the
//! fixture is non-trivial without being huge.

use anyhow::Result;
use serde_json::Value;

/// A function the fixture's custom dylint lint (`lints/ban_forbidden_fn`)
/// forbids calling. Kept public so the lint has a real call-path target to
/// check, and so `app/src/violation.rs.disabled` can demonstrate the lint
/// firing when swapped in via `bench/dylint_perf.py --expect-fail`.
///
/// Nothing in the default fixture state (`app/src/main.rs`) calls this —
/// the default state must stay lint-clean so cold/warm timing runs are
/// green.
pub fn forbidden_marker_fn() -> &'static str {
    "forbidden"
}

/// Parse a JSON config blob. Exercises `serde_json` beyond a trivial
/// dependency pull.
pub fn parse_config(raw: &str) -> Result<Value> {
    let value: Value = serde_json::from_str(raw)?;
    Ok(value)
}

#[derive(Debug, Clone)]
pub struct Config {
    pub name: String,
    pub count: u32,
}

impl Config {
    pub fn from_value(value: &Value) -> Result<Self> {
        let name = value
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("default")
            .to_string();
        let count = value
            .get("count")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as u32;
        Ok(Self { name, count })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_config() {
        let value = parse_config(r#"{"name": "x", "count": 1}"#).unwrap();
        let config = Config::from_value(&value).unwrap();
        assert_eq!(config.name, "x");
        assert_eq!(config.count, 1);
    }
}
