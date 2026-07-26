use dep_user::{parse_config, Config};
use macros::Greet;
use serde::Serialize;

/// Default fixture state: this file must PASS the `ban_forbidden_fn` dylint
/// lint so cold/warm bench timing runs are green. The live trigger site
/// lives in `violation.rs.disabled`, which `bench/dylint_perf.py
/// --expect-fail` swaps in temporarily to prove diagnostics still fire
/// after cache restores, then restores this file.
#[derive(Serialize, Greet)]
struct Fixture {
    name: String,
}

fn main() {
    let fixture = Fixture {
        name: "dylint-fixture".to_string(),
    };
    println!("{}", fixture.greet());

    let raw = r#"{"name": "demo", "count": 3}"#;
    let value = parse_config(raw).expect("valid json");
    let config = Config::from_value(&value).expect("config parses");
    println!("config: {} x{}", config.name, config.count);
}
