//! `soldr config` (soldr#3696): read and write `~/.soldr/config.toml`.
//!
//! Keys are dotted paths (`linker`, `cook.max_total_gb`, `pins.zccache`).
//! Edits go through `toml_edit` so comments and layout survive, and every
//! `set` is validated by the one real loader (`SoldrConfig`) before the file
//! is atomically replaced, so `soldr config set` can never write a file that
//! the rest of soldr would refuse to load.

use std::path::Path;

use crate::core::{SoldrConfig, SoldrError, SoldrPaths};

/// Top-level keys `SoldrConfig` understands. The struct does not deny
/// unknown top-level fields, so this is what rejects `set typo.x 1`.
const KNOWN_ROOT_KEYS: &[&str] = &[
    "gc", "auto_gc", "linker", "cook", "pins", "jobs", "zccache", "install",
];

#[derive(Debug, Clone, clap::Subcommand)]
pub enum ConfigSubcommand {
    /// Print every key set in config.toml as `key = value` (default)
    List,
    /// Print the value of one dotted key (exit 1 when unset)
    Get { key: String },
    /// Set one dotted key; the value is parsed as TOML, else a string
    Set { key: String, value: String },
    /// Print the path of config.toml
    Path,
}

fn err(msg: impl Into<String>) -> SoldrError {
    SoldrError::Other(msg.into())
}

fn read_doc(path: &Path) -> Result<toml_edit::DocumentMut, SoldrError> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e.into()),
    };
    text.parse::<toml_edit::DocumentMut>().map_err(|e| {
        err(format!(
            "failed to parse soldr config {}: {e}",
            path.display()
        ))
    })
}

fn split_key(key: &str) -> Result<Vec<&str>, SoldrError> {
    let parts: Vec<&str> = key.split('.').collect();
    if parts.iter().any(|p| p.trim().is_empty()) {
        return Err(err(format!("invalid config key `{key}`")));
    }
    Ok(parts)
}

fn render(item: &toml_edit::Item) -> String {
    match item.as_value() {
        Some(toml_edit::Value::String(s)) => s.value().clone(),
        Some(v) => v.clone().decorated("", "").to_string(),
        None => item.to_string().trim().to_string(),
    }
}

/// Value of dotted `key`, or `None` when it is not set in the file.
pub fn config_get(path: &Path, key: &str) -> Result<Option<String>, SoldrError> {
    let doc = read_doc(path)?;
    let mut item = doc.as_item();
    for part in split_key(key)? {
        match item.get(part) {
            Some(next) => item = next,
            None => return Ok(None),
        }
    }
    Ok(Some(render(item)))
}

/// Set dotted `key` to `value` (parsed as a TOML value, falling back to a
/// plain string), validate with `SoldrConfig`, then atomically replace.
pub fn config_set(path: &Path, key: &str, value: &str) -> Result<(), SoldrError> {
    let parts = split_key(key)?;
    if !KNOWN_ROOT_KEYS.contains(&parts[0]) {
        return Err(err(format!(
            "unknown config key `{key}` (known sections: {})",
            KNOWN_ROOT_KEYS.join(", ")
        )));
    }
    let parsed: toml_edit::Value = value
        .parse()
        .unwrap_or_else(|_| toml_edit::Value::from(value));
    let mut doc = read_doc(path)?;
    let (last, parents) = parts.split_last().expect("split_key is non-empty");
    let mut table = doc.as_table_mut();
    for part in parents {
        let entry = table
            .entry(part)
            .or_insert_with(|| toml_edit::Item::Table(toml_edit::Table::new()));
        table = entry
            .as_table_mut()
            .ok_or_else(|| err(format!("`{part}` in `{key}` is not a table")))?;
    }
    table.insert(last, toml_edit::value(parsed));
    let text = doc.to_string();
    toml::from_str::<SoldrConfig>(&text)
        .map_err(|e| err(format!("refusing to set `{key}` = {value}: {e}")))?;
    write_atomic(path, &text)
}

fn write_atomic(path: &Path, text: &str) -> Result<(), SoldrError> {
    let dir = path
        .parent()
        .ok_or_else(|| err(format!("config path {} has no parent", path.display())))?;
    std::fs::create_dir_all(dir)?;
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    std::io::Write::write_all(&mut tmp, text.as_bytes())?;
    tmp.as_file().sync_all()?;
    tmp.persist(path).map_err(|e| SoldrError::Io(e.error))?;
    Ok(())
}

/// Every leaf key set in the file, in file order, as `(dotted key, value)`.
pub fn config_list(path: &Path) -> Result<Vec<(String, String)>, SoldrError> {
    fn walk(prefix: &str, table: &dyn toml_edit::TableLike, out: &mut Vec<(String, String)>) {
        for (k, item) in table.iter() {
            let key = if prefix.is_empty() {
                k.to_string()
            } else {
                format!("{prefix}.{k}")
            };
            match item.as_table_like() {
                Some(sub) => walk(&key, sub, out),
                None => out.push((key, render(item))),
            }
        }
    }
    let doc = read_doc(path)?;
    let mut out = Vec::new();
    walk("", doc.as_table(), &mut out);
    Ok(out)
}

pub fn run_config_command(command: Option<ConfigSubcommand>) -> Result<(), SoldrError> {
    let path = SoldrPaths::new()?.config_file;
    match command.unwrap_or(ConfigSubcommand::List) {
        ConfigSubcommand::List => {
            for (k, v) in config_list(&path)? {
                println!("{k} = {v}");
            }
        }
        ConfigSubcommand::Get { key } => match config_get(&path, &key)? {
            Some(v) => println!("{v}"),
            None => {
                eprintln!("soldr: config key `{key}` is not set in {}", path.display());
                std::process::exit(1);
            }
        },
        ConfigSubcommand::Set { key, value } => config_set(&path, &key, &value)?,
        ConfigSubcommand::Path => println!("{}", path.display()),
    }
    Ok(())
}

#[cfg(test)]
#[path = "config_cmd_tests.rs"]
mod tests;
