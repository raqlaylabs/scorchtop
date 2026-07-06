//! Persisted daily aggregates — agentop's ONLY write path.
//!
//! On every run the (project, day, model) cube is persisted as JSON under
//! `~/.local/share/agentop/history/` and merged with the live JSONL scan at
//! load. For overlapping keys the live scan wins (the transcripts are the
//! source of truth); history keeps days visible after Claude Code prunes old
//! transcripts. Nothing here ever touches `~/.claude/`.

use std::fs;
use std::path::{Path, PathBuf};

use chrono::NaiveDate;
use serde::{Deserialize, Serialize};

use crate::aggregate::{Cube, CubeEntry, CubeKey};
use crate::source::TokenUsage;

const FILE_NAME: &str = "daily.json";

/// `$XDG_DATA_HOME/agentop/history` or `~/.local/share/agentop/history`.
pub fn default_dir() -> Option<PathBuf> {
    if let Some(xdg) = std::env::var_os("XDG_DATA_HOME").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(xdg).join("agentop").join("history"));
    }
    let home = directories::BaseDirs::new()?.home_dir().to_path_buf();
    Some(home.join(".local").join("share").join("agentop").join("history"))
}

#[derive(Serialize, Deserialize)]
struct StoredEntry {
    project: String,
    name: String,
    date: NaiveDate,
    model: String,
    input_tokens: u64,
    output_tokens: u64,
    cache_creation_tokens: u64,
    cache_read_tokens: u64,
    records: u64,
    est_cost_usd: Option<f64>,
}

fn to_stored(key: &CubeKey, entry: &CubeEntry) -> StoredEntry {
    StoredEntry {
        project: key.project.clone(),
        name: entry.display_name.clone(),
        date: key.date,
        model: key.model.clone(),
        input_tokens: entry.tokens.input,
        output_tokens: entry.tokens.output,
        cache_creation_tokens: entry.tokens.cache_create,
        cache_read_tokens: entry.tokens.cache_read,
        records: entry.records,
        est_cost_usd: entry.est_cost,
    }
}

fn from_stored(s: StoredEntry) -> (CubeKey, CubeEntry) {
    (
        CubeKey { project: s.project, date: s.date, model: s.model },
        CubeEntry {
            display_name: s.name,
            tokens: TokenUsage {
                input: s.input_tokens,
                output: s.output_tokens,
                cache_create: s.cache_creation_tokens,
                cache_read: s.cache_read_tokens,
            },
            records: s.records,
            est_cost: s.est_cost_usd,
        },
    )
}

/// Load persisted history. Missing or unreadable history is an empty cube,
/// never an error — the live scan always works without it.
pub fn load(dir: &Path) -> Cube {
    let Ok(bytes) = fs::read(dir.join(FILE_NAME)) else {
        return Cube::new();
    };
    let Ok(entries) = serde_json::from_slice::<Vec<StoredEntry>>(&bytes) else {
        return Cube::new();
    };
    entries.into_iter().map(from_stored).collect()
}

/// Union of stored and live entries; live wins on overlapping keys.
pub fn merge(stored: Cube, live: &Cube) -> Cube {
    let mut merged = stored;
    for (key, entry) in live {
        merged.insert(key.clone(), entry.clone());
    }
    merged
}

/// Atomically persist the cube (write temp file, then rename).
pub fn persist(dir: &Path, cube: &Cube) -> std::io::Result<()> {
    fs::create_dir_all(dir)?;
    let entries: Vec<StoredEntry> = cube.iter().map(|(k, e)| to_stored(k, e)).collect();
    let json = serde_json::to_vec_pretty(&entries).expect("cube serializes");
    let tmp = dir.join(format!("{FILE_NAME}.tmp.{}", std::process::id()));
    fs::write(&tmp, json)?;
    fs::rename(&tmp, dir.join(FILE_NAME))
}

/// Load + merge + persist in one step. Persistence failures degrade to the
/// merged in-memory cube (correct numbers beat durable history).
pub fn sync(dir: &Path, live: &Cube) -> Cube {
    let merged = merge(load(dir), live);
    if let Err(e) = persist(dir, &merged) {
        eprintln!("agentop: could not persist history to {}: {e}", dir.display());
    }
    merged
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(project: &str, date: &str, model: &str) -> CubeKey {
        CubeKey {
            project: project.into(),
            date: date.parse().unwrap(),
            model: model.into(),
        }
    }

    fn entry(name: &str, input: u64, cost: Option<f64>) -> CubeEntry {
        CubeEntry {
            display_name: name.into(),
            tokens: TokenUsage { input, output: 0, cache_create: 0, cache_read: 0 },
            records: 1,
            est_cost: cost,
        }
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("agentop-test-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn round_trips_through_disk() {
        let dir = temp_dir("roundtrip");
        let mut cube = Cube::new();
        cube.insert(key("p1", "2026-07-01", "claude-opus-4-8"), entry("alpha", 100, Some(1.5)));
        cube.insert(key("p2", "2026-07-02", "mystery"), entry("beta", 50, None));

        persist(&dir, &cube).unwrap();
        let loaded = load(&dir);

        assert_eq!(loaded.len(), 2);
        let e = &loaded[&key("p1", "2026-07-01", "claude-opus-4-8")];
        assert_eq!(e.tokens.input, 100);
        assert_eq!(e.est_cost, Some(1.5));
        assert_eq!(loaded[&key("p2", "2026-07-02", "mystery")].est_cost, None);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn live_wins_on_overlap_history_fills_pruned_days() {
        let k_overlap = key("p1", "2026-07-01", "claude-opus-4-8");
        let k_pruned = key("p1", "2026-06-01", "claude-opus-4-8");

        let mut stored = Cube::new();
        stored.insert(k_overlap.clone(), entry("alpha", 999, Some(9.9))); // stale
        stored.insert(k_pruned.clone(), entry("alpha", 42, Some(0.4))); // pruned from JSONL

        let mut live = Cube::new();
        live.insert(k_overlap.clone(), entry("alpha", 100, Some(1.0)));

        let merged = merge(stored, &live);
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[&k_overlap].tokens.input, 100, "live scan wins");
        assert_eq!(merged[&k_pruned].tokens.input, 42, "pruned day survives");
    }

    #[test]
    fn missing_or_corrupt_history_is_empty_not_an_error() {
        let dir = temp_dir("corrupt");
        assert!(load(&dir).is_empty());
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(FILE_NAME), b"{ not json").unwrap();
        assert!(load(&dir).is_empty());
        fs::remove_dir_all(&dir).unwrap();
    }
}
