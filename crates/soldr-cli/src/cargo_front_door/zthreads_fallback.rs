//! Conservative fallback for stable rustc rejecting `-Zthreads=<N>`.
//!
//! The flag is deliberately handled only when it comes from an environment
//! variable that we can rewrite for a child process. Cargo config values are
//! opaque to the front door, so those failures remain failures with a hint.

use std::collections::BTreeMap;

pub(crate) const ATTEMPTED_ENV: &str = "SOLDR_INTERNAL_ZTHREADS_FALLBACK_ATTEMPTED";
const NIGHTLY_ONLY_DIAGNOSTIC: &str = "the option `Z` is only accepted on the nightly compiler";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FallbackPlan {
    pub(crate) value: String,
    /// `None` means remove the variable from the child environment.
    pub(crate) env: BTreeMap<String, Option<String>>,
}

pub(crate) fn diagnostic_matches(stderr: &str) -> bool {
    stderr.contains(NIGHTLY_ONLY_DIAGNOSTIC)
}

pub(crate) fn environment_mentions_zthreads() -> bool {
    std::env::vars().any(|(key, value)| {
        is_supported_key(&key)
            && value
                .split(|c: char| c.is_ascii_whitespace() || c == '\x1f')
                .any(|token| token.starts_with("-Zthreads="))
    })
}

pub(crate) fn plan_from_environment() -> Option<FallbackPlan> {
    if super::env_flag_truthy(ATTEMPTED_ENV) || super::foreign_env_flag("RUSTC_BOOTSTRAP") {
        return None;
    }

    let mut env = BTreeMap::new();
    let mut value = None;
    let mut saw_other_z = false;

    for (key, raw) in std::env::vars() {
        if !is_supported_key(&key) {
            continue;
        }
        let encoded = key == "CARGO_ENCODED_RUSTFLAGS";
        let tokens: Vec<&str> = if encoded {
            raw.split('\x1f').collect()
        } else {
            raw.split_ascii_whitespace().collect()
        };
        let mut changed = false;
        let mut kept = Vec::with_capacity(tokens.len());
        for token in tokens {
            if token.starts_with("-Z") {
                if let Some(candidate) = token.strip_prefix("-Zthreads=") {
                    if candidate.is_empty()
                        || value
                            .as_deref()
                            .is_some_and(|existing| existing != candidate)
                    {
                        saw_other_z = true;
                    } else {
                        value = Some(candidate.to_string());
                        changed = true;
                        continue;
                    }
                } else {
                    saw_other_z = true;
                }
            }
            kept.push(token);
        }
        if changed {
            let replacement = if encoded {
                kept.join("\x1f")
            } else {
                kept.join(" ")
            };
            env.insert(
                key,
                if replacement.is_empty() {
                    None
                } else {
                    Some(replacement)
                },
            );
        }
    }

    if saw_other_z || value.is_none() || env.is_empty() {
        return None;
    }
    Some(FallbackPlan {
        value: value.expect("checked above"),
        env,
    })
}

pub(crate) fn render_warning(value: &str, github_actions: bool, use_color: bool) -> String {
    let message = format!(
        "soldr: stable Rust rejected -Zthreads={value}; retrying once without it. Build output is unchanged, but compilation may be slower."
    );
    if github_actions {
        format!("::warning::{message}")
    } else if use_color {
        format!("\x1b[33m{message}\x1b[0m")
    } else {
        message
    }
}

pub(crate) fn render_config_hint() -> &'static str {
    "soldr: stable Rust rejected -Zthreads, but the flag was not supplied through a removable Rust flags environment variable; remove it from Cargo config or provide it via RUSTFLAGS/CARGO_ENCODED_RUSTFLAGS."
}

fn is_supported_key(key: &str) -> bool {
    key == "RUSTFLAGS"
        || key == "CARGO_ENCODED_RUSTFLAGS"
        || (key.starts_with("CARGO_TARGET_") && key.ends_with("_RUSTFLAGS"))
}
