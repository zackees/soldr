//! Short-lived storage for finished profiles (S16 / #645).
//!
//! Profiles are **ephemeral** by design. A crash record is evidence about
//! something that already happened and is worth keeping for a month; a profile
//! is a working artifact of an investigation someone is running right now, and
//! it is large — tens of thousands of stacks. Keeping them durably would
//! quietly turn a diagnostic tool into a disk-consumption problem on the
//! machine it is supposed to be helping.
//!
//! So they live in memory, bounded by both count and age, and the browser or
//! CLI saves the one it cares about. The daemon is a place to *fetch* a
//! profile from, not the place it is archived.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use super::SessionResult;

/// How long a finished profile stays fetchable.
///
/// Long enough to run a profile, look at the flame graph, and download the
/// export; short enough that a forgotten one is not still resident an hour
/// later.
pub const PROFILE_TTL: Duration = Duration::from_secs(15 * 60);

/// How many profiles are retained at once.
///
/// A hard bound, because each one can be tens of megabytes and the whole point
/// of holding them in memory is that the set stays small. The oldest is
/// evicted rather than the newest refused: an operator who just ran a profile
/// wants *that* one, and refusing it to preserve one they have finished with
/// would be backwards.
pub const MAX_PROFILES: usize = 8;

/// One retained profile.
#[derive(Debug)]
struct Entry {
    result: SessionResult,
    stored: Instant,
}

/// An in-memory, bounded set of finished profiles.
#[derive(Debug, Default)]
pub struct ProfileStore {
    entries: Mutex<HashMap<u64, Entry>>,
    next_id: AtomicU64,
}

impl ProfileStore {
    /// An empty store.
    pub fn new() -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
        }
    }

    /// Retain `result` and return the id it can be fetched by.
    pub fn insert(&self, result: SessionResult) -> u64 {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        entries.insert(
            id,
            Entry {
                result,
                stored: Instant::now(),
            },
        );
        evict(&mut entries);
        id
    }

    /// Fetch a retained profile, if it is still retained.
    pub fn get(&self, id: u64) -> Option<SessionResult> {
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        evict(&mut entries);
        entries.get(&id).map(|entry| entry.result.clone())
    }

    /// Ids currently retained, newest first.
    pub fn ids(&self) -> Vec<u64> {
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        evict(&mut entries);
        let mut ids: Vec<u64> = entries.keys().copied().collect();
        ids.sort_unstable_by(|a, b| b.cmp(a));
        ids
    }

    /// How many profiles are retained.
    pub fn len(&self) -> usize {
        self.ids().len()
    }

    /// Whether nothing is retained.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Drop expired entries, then the oldest until the count fits.
///
/// Age first, so a store under its count bound still forgets stale profiles —
/// otherwise a daemon that ran three profiles this morning would still be
/// holding them at midnight.
fn evict(entries: &mut HashMap<u64, Entry>) {
    entries.retain(|_, entry| entry.stored.elapsed() < PROFILE_TTL);
    while entries.len() > MAX_PROFILES {
        let Some(oldest) = entries
            .iter()
            .min_by_key(|(_, entry)| entry.stored)
            .map(|(id, _)| *id)
        else {
            break;
        };
        entries.remove(&oldest);
    }
}

/// A node of the tree a flame graph draws.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct FlameNode {
    /// Frame name.
    pub name: String,
    /// Samples in this subtree.
    pub value: u64,
    /// Callees.
    pub children: Vec<FlameNode>,
}

/// Fold collapsed-stack text into a tree.
///
/// The collapsed format is one line per unique stack with a count, so folding
/// is a prefix merge. Malformed lines are skipped rather than failing the
/// render: a profile is a sampled artifact, and losing one line of it is
/// strictly better than showing the operator nothing.
pub fn collapsed_to_tree(text: &str) -> FlameNode {
    let mut root = FlameNode {
        name: "root".to_string(),
        ..FlameNode::default()
    };

    for line in text.lines() {
        let line = line.trim();
        let Some((stack, count)) = line.rsplit_once(' ') else {
            continue;
        };
        let Ok(count) = count.parse::<u64>() else {
            continue;
        };
        root.value += count;

        let mut node = &mut root;
        for frame in stack.split(';').filter(|frame| !frame.is_empty()) {
            let index = match node.children.iter().position(|c| c.name == frame) {
                Some(index) => index,
                None => {
                    node.children.push(FlameNode {
                        name: frame.to_string(),
                        ..FlameNode::default()
                    });
                    node.children.len() - 1
                }
            };
            node = &mut node.children[index];
            node.value += count;
        }
    }

    sort_hot_first(&mut root);
    root
}

/// Order siblings by weight so the hottest path is the widest leftmost run.
///
/// Ordering is presentational, not semantic — but a flame graph is read by
/// eye, and an operator's eye goes left. Sorting by name instead would scatter
/// the hot path across the row.
fn sort_hot_first(node: &mut FlameNode) {
    node.children
        .sort_by(|a, b| b.value.cmp(&a.value).then_with(|| a.name.cmp(&b.name)));
    for child in &mut node.children {
        sort_hot_first(child);
    }
}

/// Fold a finished session straight into a tree.
pub fn session_to_tree(result: &SessionResult) -> FlameNode {
    let mut root = FlameNode {
        name: "root".to_string(),
        ..FlameNode::default()
    };
    for (stack, count) in result.folded() {
        root.value += count;
        let mut node = &mut root;
        for frame in &stack {
            let index = match node.children.iter().position(|c| &c.name == frame) {
                Some(index) => index,
                None => {
                    node.children.push(FlameNode {
                        name: frame.clone(),
                        ..FlameNode::default()
                    });
                    node.children.len() - 1
                }
            };
            node = &mut node.children[index];
            node.value += count;
        }
    }
    sort_hot_first(&mut root);
    root
}
