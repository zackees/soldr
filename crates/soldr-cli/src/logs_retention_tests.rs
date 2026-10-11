//! Unit coverage for `soldr logs view` / `soldr logs prune` (soldr#3698).

use super::*;
use crate::cli_args::{Cli, Commands, LogsSubcommand};
use clap::Parser;

fn make_launch(history: &Path, id: u64, mtime_secs: u64, journal: &str) -> PathBuf {
    let dir = history.join(id.to_string());
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("last-session.jsonl"), journal).unwrap();
    let t = std::time::UNIX_EPOCH + std::time::Duration::from_secs(mtime_secs);
    std::fs::File::open(&dir).unwrap().set_modified(t).unwrap();
    dir
}

#[test]
fn cli_parses_logs_prune_keep_and_view() {
    let prune = Cli::try_parse_from(["soldr", "logs", "prune", "--keep", "1"]).unwrap();
    match prune.command {
        Commands::Logs {
            command:
                Some(LogsSubcommand::Prune {
                    keep,
                    dry_run,
                    json,
                }),
        } => {
            assert_eq!(keep, 1);
            assert!(!dry_run && !json);
        }
        _ => panic!("expected logs prune"),
    }
    let view = Cli::try_parse_from(["soldr", "logs", "view", "123"]).unwrap();
    match view.command {
        Commands::Logs {
            command: Some(LogsSubcommand::View { launch_id }),
        } => assert_eq!(launch_id, "123"),
        _ => panic!("expected logs view"),
    }
}

#[test]
fn history_root_is_the_logs_paths_build_history_entry() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = SoldrPaths::with_root(tmp.path().to_path_buf());
    let expected = crate::logs_cmd::build_log_paths_output(&paths)
        .paths
        .into_iter()
        .find(|e| e.name == "zccache-build-history")
        .unwrap()
        .path;
    assert_eq!(history_root_from_log_paths(&paths), expected);
}

#[test]
fn prune_keeps_newest_n_launches_and_nothing_else() {
    let tmp = tempfile::tempdir().unwrap();
    let history = tmp.path().join("history");
    let old = make_launch(&history, 11, 1_000, "{}\n");
    let mid = make_launch(&history, 22, 2_000, "{}\n");
    let new = make_launch(&history, 33, 3_000, "{}\n");
    // Non-launch entries inside the history dir are never touched.
    std::fs::write(history.join("README"), "keep").unwrap();
    std::fs::create_dir_all(history.join("not-a-launch")).unwrap();
    // A sibling outside the history dir must survive.
    let outside = tmp.path().join("44");
    std::fs::create_dir_all(&outside).unwrap();

    let dry = prune_history(&history, 1, true).unwrap();
    assert_eq!(dry.removed.len(), 2);
    assert!(old.exists() && mid.exists());

    let report = prune_history(&history, 1, false).unwrap();
    assert_eq!(report.kept, vec!["33".to_string()]);
    assert_eq!(report.removed.len(), 2);
    assert!(!old.exists() && !mid.exists() && new.exists());
    assert!(history.join("README").exists());
    assert!(history.join("not-a-launch").exists());
    assert!(outside.exists());
}

#[test]
fn prune_skips_launches_still_publishing() {
    let tmp = tempfile::tempdir().unwrap();
    let history = tmp.path().join("history");
    let active = make_launch(&history, 1, 1_000, "{}\n");
    std::fs::write(active.join(".publishing-v2"), "publishing\n").unwrap();
    let t = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_000);
    std::fs::File::open(&active)
        .unwrap()
        .set_modified(t)
        .unwrap();
    make_launch(&history, 2, 2_000, "{}\n");
    let report = prune_history(&history, 0, false).unwrap();
    assert!(active.exists());
    assert_eq!(report.skipped_active, vec!["1".to_string()]);
}

#[test]
fn prune_on_missing_history_root_is_a_noop() {
    let tmp = tempfile::tempdir().unwrap();
    let report = prune_history(&tmp.path().join("absent"), 1, false).unwrap();
    assert!(report.removed.is_empty() && report.kept.is_empty());
}

#[test]
fn view_streams_the_launch_journal_by_exact_or_unique_prefix() {
    let tmp = tempfile::tempdir().unwrap();
    let history = tmp.path().join("history");
    make_launch(&history, 4242, 1_000, "{\"a\":1}\n{\"b\":2}\n");
    make_launch(&history, 9999, 2_000, "{\"z\":0}\n");
    let mut out = Vec::new();
    view_launch(&history, "4242", &mut out).unwrap();
    assert_eq!(String::from_utf8(out).unwrap(), "{\"a\":1}\n{\"b\":2}\n");

    let mut out = Vec::new();
    view_launch(&history, "42", &mut out).unwrap();
    assert!(String::from_utf8(out).unwrap().contains("\"b\":2"));

    assert!(view_launch(&history, "1", &mut Vec::new()).is_err());
    assert!(view_launch(&history, "../x", &mut Vec::new()).is_err());
}
