//! "Did you mean?" suggestion engine for the external-subcommand
//! fallthrough path (issue #412).
//!
//! When clap doesn't recognize a soldr verb, the dispatch falls
//! through to `Commands::External` which fetches `<verb>` from
//! crates.io / GitHub Releases as an arbitrary tool. The same
//! happens inside the cargo front door: when the cargo subcommand
//! the user typed isn't in `known_tools`, soldr quietly skips the
//! prebuilt-binary path. Either way, an ordinary typo or a verb
//! rename that didn't ship a clap alias surfaces as either a
//! network error ("tool not found") or a cargo external-command
//! error ("no such subcommand: cago"). Neither names the actual
//! cause.
//!
//! This module wraps a tiny Levenshtein distance calculation and a
//! single user-facing call:
//!
//! ```ignore
//! let candidates = ["install-zccache", "update-zccache", "doctor"];
//! suggest_close_match("update-zccacheee", &candidates);
//! // => Some("update-zccache")
//! ```
//!
//! Threshold per issue #412 (acceptance criteria):
//!
//!   max(2, ceil(0.3 × len(query)))
//!
//! That gives `ntest` → `nextest` (dist 2, threshold 2) and
//! `update-zccacheee` → `update-zccache` (dist 3, threshold 5) while
//! rejecting `completely-made-up-name` (no neighbour within 10).
//!
//! The matcher is intentionally caller-agnostic. The two callsites
//! that consume it are:
//!
//! - `main.rs Commands::External` → matches against the soldr
//!   built-in verb list (including clap aliases).
//! - `cargo_front_door::ensure_known_subcommand_tool` → matches
//!   against the `cargo_subcommand` field of `KNOWN_TOOLS`.
//!
//! Both add an `eprintln!` hint and then continue with the fetch —
//! the suggestion is purely advisory.
//!
//! Cost: Levenshtein over a ~20-entry list at startup is
//! sub-microsecond on any host soldr runs on. No measurable
//! invocation overhead added (#412 acceptance criterion).

/// Compute the Levenshtein edit distance between two ASCII-ish
/// strings. Operates byte-by-byte, which is correct for the
/// command-name strings we feed it (kebab-case ASCII) and avoids
/// pulling in a unicode-aware crate just for this.
///
/// O(|a| × |b|) time and O(min(|a|, |b|)) space via the standard
/// rolling-row trick.
pub(crate) fn levenshtein(a: &str, b: &str) -> usize {
    // Cheap shortcuts.
    if a == b {
        return 0;
    }
    if a.is_empty() {
        return b.chars().count();
    }
    if b.is_empty() {
        return a.chars().count();
    }

    // Work on byte length — exact for the ASCII command names we use.
    // Falls back to a still-correct (just slightly looser) bound for
    // any incidental non-ASCII input.
    let a_bytes = a.as_bytes();
    let b_bytes = b.as_bytes();
    let (shorter, longer) = if a_bytes.len() <= b_bytes.len() {
        (a_bytes, b_bytes)
    } else {
        (b_bytes, a_bytes)
    };

    // Rolling-row DP: previous row + current row indexed by `shorter`.
    let mut prev: Vec<usize> = (0..=shorter.len()).collect();
    let mut curr: Vec<usize> = vec![0; shorter.len() + 1];

    for (i, &lb) in longer.iter().enumerate() {
        curr[0] = i + 1;
        for (j, &sb) in shorter.iter().enumerate() {
            let cost = if lb == sb { 0 } else { 1 };
            curr[j + 1] = (prev[j] + cost)
                .min(prev[j + 1] + 1) // deletion from `longer`
                .min(curr[j] + 1); //   insertion into `longer`
        }
        std::mem::swap(&mut prev, &mut curr);
    }

    prev[shorter.len()]
}

/// Threshold for "close enough to suggest" per issue #412:
/// `max(2, ceil(0.3 × len(query)))`. Always returns at least 2 so
/// 1-3 character queries don't reject typos. Capped above 2 by 30%
/// of the query length for longer strings.
fn distance_threshold(query: &str) -> usize {
    let len = query.chars().count();
    // ceil(0.3 × len) without floating-point: `(len * 3).div_ceil(10)`
    // gives the same value as `(len as f64 * 0.3).ceil() as usize`
    // for non-negative integer len up to usize::MAX / 4.
    let scaled = (len * 3).div_ceil(10);
    scaled.max(2)
}

/// Return the candidate from `candidates` with the smallest edit
/// distance to `query`, provided that distance is within the threshold
/// AND `query` is not an exact match for any candidate.
///
/// Returns `None` when:
/// - `query` exactly matches a candidate (no suggestion needed — the
///   caller's normal dispatch path will handle it).
/// - No candidate sits within the threshold (no false suggestions).
/// - `candidates` is empty.
///
/// Tie-breaking: ascending distance, then candidate order in the input
/// list. That's deterministic for tests; the input order in production
/// is the clap declaration order, which is fine.
pub fn suggest_close_match<'a>(query: &str, candidates: &'a [&'a str]) -> Option<&'a str> {
    if query.is_empty() || candidates.is_empty() {
        return None;
    }
    // Exact match → no suggestion (the regular dispatch handles it).
    if candidates.contains(&query) {
        return None;
    }

    let threshold = distance_threshold(query);
    let mut best: Option<(usize, &'a str)> = None;
    for &candidate in candidates {
        let d = levenshtein(query, candidate);
        if d > threshold {
            continue;
        }
        match best {
            None => best = Some((d, candidate)),
            Some((bd, _)) if d < bd => best = Some((d, candidate)),
            _ => {} // keep first occurrence on tie
        }
    }
    best.map(|(_, c)| c)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levenshtein_handles_trivial_cases() {
        assert_eq!(levenshtein("", ""), 0);
        assert_eq!(levenshtein("a", ""), 1);
        assert_eq!(levenshtein("", "abc"), 3);
        assert_eq!(levenshtein("abc", "abc"), 0);
    }

    #[test]
    fn levenshtein_known_distances() {
        // Classic kitten→sitting: 3 (k→s, e→i, +g).
        assert_eq!(levenshtein("kitten", "sitting"), 3);
        // Verb rename in soldr's own history. `update-` ↔ `install-`
        // differs in 6 of 7 characters; one trailing `h` adds 1
        // length difference. Net edit distance = 6.
        assert_eq!(levenshtein("update-zccache", "install-zccache"), 6);
        // Typical typo distance (one missing char).
        assert_eq!(levenshtein("installzccache", "install-zccache"), 1);
        // Common short typo.
        assert_eq!(levenshtein("ntest", "nextest"), 2);
    }

    #[test]
    fn distance_threshold_floor_is_two() {
        // ceil(0.3 × 1) = 1 → floor to 2 so 1-char queries still
        // tolerate a single edit.
        assert_eq!(distance_threshold("a"), 2);
        assert_eq!(distance_threshold("ab"), 2);
        assert_eq!(distance_threshold("ntest"), 2);
    }

    #[test]
    fn distance_threshold_scales_with_length() {
        // 0.3 × 10 = 3 (ceil).
        assert_eq!(distance_threshold("0123456789"), 3);
        // 0.3 × 14 = 4.2 → ceil to 5.
        assert_eq!(distance_threshold("installzccache"), 5);
        // 0.3 × 16 = 4.8 → ceil to 5.
        assert_eq!(distance_threshold("update-zccacheee"), 5);
    }

    #[test]
    fn suggest_returns_none_on_exact_match() {
        // No suggestion when the verb actually exists. The caller's
        // normal dispatch handles it.
        let cands = ["install-zccache", "update-zccache", "doctor"];
        assert_eq!(suggest_close_match("install-zccache", &cands), None);
        assert_eq!(suggest_close_match("doctor", &cands), None);
    }

    #[test]
    fn suggest_recognizes_the_update_zccacheee_typo_from_issue_412() {
        // Acceptance criterion: `soldr update-zccacheee` prints a hint
        // pointing at install-zccache (or its update-zccache alias).
        let cands = ["install-zccache", "update-zccache", "doctor"];
        let suggestion = suggest_close_match("update-zccacheee", &cands);
        // Either branch of the OR is acceptable per #412; both are
        // within the threshold and lead the user to the right place.
        assert!(
            matches!(suggestion, Some("update-zccache" | "install-zccache")),
            "expected install-zccache or update-zccache, got {suggestion:?}",
        );
    }

    #[test]
    fn suggest_handles_unhyphenated_typo() {
        // `soldr installzccache` (missing the dash) is one edit away
        // from the canonical `install-zccache`.
        let cands = ["install-zccache", "update-zccache", "doctor"];
        assert_eq!(
            suggest_close_match("installzccache", &cands),
            Some("install-zccache"),
        );
    }

    #[test]
    fn suggest_recognizes_cargo_ntest_typo_from_issue_412() {
        // Acceptance criterion: `soldr cargo ntest` prints a hint
        // pointing at nextest. Threshold is 2 for a 5-char query;
        // `ntest`→`nextest` is exactly distance 2.
        let cargo_subs = ["nextest", "deny", "audit", "llvm-cov"];
        assert_eq!(suggest_close_match("ntest", &cargo_subs), Some("nextest"),);
    }

    #[test]
    fn suggest_returns_none_for_far_from_everything() {
        // Acceptance criterion: `soldr completely-made-up-name` must
        // NOT produce a false suggestion. The query is far enough from
        // every soldr verb that the threshold is never reached.
        let cands = ["install-zccache", "doctor", "cargo", "cook"];
        assert_eq!(suggest_close_match("completely-made-up-name", &cands), None,);
    }

    #[test]
    fn suggest_returns_none_on_empty_inputs() {
        assert_eq!(suggest_close_match("", &["doctor"]), None);
        assert_eq!(suggest_close_match("doctor", &[]), None);
    }

    #[test]
    fn suggest_picks_closest_when_multiple_candidates_within_threshold() {
        // `cago` is within threshold of both `cargo` (dist 1) and
        // `cago-` (hypothetical, dist 1) — but `cargo` should win on
        // first-occurrence tie-break since we always prefer earlier
        // entries on equal distance.
        let cands = ["cargo", "carbo", "cook"];
        assert_eq!(suggest_close_match("cago", &cands), Some("cargo"));
    }

    #[test]
    fn suggest_handles_typo_that_drops_a_character() {
        // `cago build` was the example from #412's body — must
        // suggest `cargo`.
        let cands = ["cargo", "clean", "cook"];
        assert_eq!(suggest_close_match("cago", &cands), Some("cargo"));
    }

    #[test]
    fn suggest_rejects_very_short_substring_of_long_verb() {
        // A 4-char query against a 15-char verb: threshold is 2 for
        // the query side. Distance |a|-|b| = 11, way over → no false
        // match.
        let cands = ["install-zccache"];
        assert_eq!(suggest_close_match("inst", &cands), None);
    }
}
