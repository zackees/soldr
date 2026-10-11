//! soldr#3696: `soldr config` get/set/list/path over `config.toml`.
//! Every test uses an explicit temp config path, never the real `~/.soldr`.

use super::*;
use crate::cli_args::{Cli, Commands};
use clap::Parser;

fn temp_config() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    (dir, path)
}

#[test]
fn cli_parses_config_get_known_key() {
    let cli = Cli::try_parse_from(["soldr", "config", "get", "linker"]).unwrap();
    match cli.command {
        Commands::Config {
            command: Some(ConfigSubcommand::Get { key, .. }),
        } => assert_eq!(key, "linker"),
        _ => panic!("`soldr config get linker` did not parse as Config::Get"),
    }
}

#[test]
fn get_known_key_returns_value_from_file() {
    let (_d, path) = temp_config();
    std::fs::write(&path, "linker = \"mold\"\n[cook]\nmax_total_gb = 7\n").unwrap();
    assert_eq!(
        config_get(&path, "linker").unwrap().as_deref(),
        Some("mold")
    );
    assert_eq!(
        config_get(&path, "cook.max_total_gb").unwrap().as_deref(),
        Some("7")
    );
    assert_eq!(config_get(&path, "cook.zstd_level").unwrap(), None);
}

#[test]
fn set_then_get_and_list_round_trip() {
    let (_d, path) = temp_config();
    std::fs::write(&path, "# keep me\n[gc]\n").unwrap();
    config_set(&path, "linker", "rust-lld").unwrap();
    config_set(&path, "cook.max_total_gb", "12").unwrap();
    config_set(&path, "cook.auto_hydrate", "false").unwrap();
    config_set(&path, "pins.zccache", "1.12.5").unwrap();
    assert_eq!(
        config_get(&path, "linker").unwrap().as_deref(),
        Some("rust-lld")
    );
    assert_eq!(
        config_get(&path, "cook.max_total_gb").unwrap().as_deref(),
        Some("12")
    );
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("# keep me"), "comments preserved: {text}");
    // The real loader must accept what we wrote, with typed values.
    let cfg = crate::core::SoldrConfig::load(&path).unwrap();
    assert_eq!(cfg.cook.max_total_gb, 12);
    assert!(!cfg.cook.auto_hydrate);
    assert_eq!(cfg.pins.pin_for("zccache"), Some("1.12.5"));
    let list = config_list(&path).unwrap();
    assert!(list.contains(&("linker".to_string(), "rust-lld".to_string())));
    assert!(list.contains(&("cook.max_total_gb".to_string(), "12".to_string())));
    assert!(list.contains(&("pins.zccache".to_string(), "1.12.5".to_string())));
}

#[test]
fn set_rejects_values_the_loader_would_refuse() {
    let (_d, path) = temp_config();
    assert!(config_set(&path, "cook.max_total_gb", "lots").is_err());
    assert!(config_set(&path, "cook.no_such_knob", "1").is_err());
    assert!(config_set(&path, "nosuchsection.x", "1").is_err());
    assert!(!path.exists(), "a rejected set must not write the file");
}

#[test]
fn list_on_missing_file_is_empty() {
    let (_d, path) = temp_config();
    assert!(config_list(&path).unwrap().is_empty());
    assert_eq!(config_get(&path, "linker").unwrap(), None);
}
