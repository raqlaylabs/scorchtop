//! Live mode: one watcher/parser thread tails the JSONL files and sends
//! aggregate `Snapshot`s over a `std::sync::mpsc` channel. The UI thread owns
//! all of its own mutable state; nothing here is shared — data flows one way
//! through the channel.
//!
//! Runtime constraints honored here:
//! - `~/.claude/` is read-only; per change we open -> seek to the stored byte
//!   offset -> read new bytes -> close -> update the offset. No handle is
//!   held between events.
//! - Only complete `\n`-terminated lines are consumed; a trailing partial
//!   line stays buffered and is prepended on the next read.
//! - Watching is event-driven via `notify` (FSEvents/inotify); the thread
//!   blocks on the event channel, so idle CPU is ~zero.

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::time::Duration;

use chrono::{DateTime, Utc};
use notify::{RecursiveMode, Watcher};

use crate::aggregate::{cube_from, dedup, Cube};
use crate::history;
use crate::lines::LineBuffer;
use crate::source::claude_code::{display_name_from_key, parse_line, LineOutcome};
use crate::source::{ScanStats, UsageRecord};

/// Last observed write per project. Activity is keyed off file mtimes, not
/// usage-record timestamps: an agent mid-task can go minutes between usage
/// records (long tool runs) while still writing tool-result lines, and at
/// startup mtimes tell us who was just active before we launched.
#[derive(Debug, Clone)]
pub struct ProjectActivity {
    pub name: String,
    pub last_write: DateTime<Utc>,
}

/// What the UI receives: everything it needs to render, precomputed except
/// for period filtering (a pure `view()` call).
pub struct Snapshot {
    /// (project, day, model) cube, merged with persisted history.
    pub cube: Cube,
    /// Deduplicated records from the last ~75 minutes, for the tokens/min
    /// sparkline and burn rate.
    pub recent: Vec<UsageRecord>,
    /// Per-project last write time, for the live/idle indicators.
    pub activity: Vec<ProjectActivity>,
    pub stats: ScanStats,
    pub duplicates_skipped: u64,
}

struct TailState {
    project_key: String,
    /// Bytes consumed so far (including any buffered partial line).
    offset: u64,
    buf: LineBuffer,
}

/// Incremental tailer over `<root>/<project>/**/*.jsonl`. Accumulates raw
/// (pre-dedup) records; dedup happens per snapshot.
pub struct Tailer {
    root: PathBuf,
    states: HashMap<PathBuf, TailState>,
    pub records: Vec<UsageRecord>,
    pub malformed_lines: u64,
    /// project key -> newest file mtime seen. Any write counts, including
    /// line types the parser skips (tool results, attachments).
    arrivals: HashMap<String, DateTime<Utc>>,
    /// project key -> display name, learned from parsed records' `cwd`.
    names: HashMap<String, String>,
}

impl Tailer {
    pub fn new(root: PathBuf) -> Self {
        // FSEvents/inotify report resolved real paths; if the root contains a
        // symlink (macOS: /tmp -> /private/tmp), event paths would never match
        // it and every live update would be silently dropped.
        let root = root.canonicalize().unwrap_or(root);
        Self {
            root,
            states: HashMap::new(),
            records: Vec::new(),
            malformed_lines: 0,
            arrivals: HashMap::new(),
            names: HashMap::new(),
        }
    }

    pub fn files_seen(&self) -> usize {
        self.states.len()
    }

    /// Discover and read every JSONL file from its current offset. Used once
    /// at startup; `probe_tails` additionally counts a parseable
    /// unterminated final line (kept buffered — if it later completes, dedup
    /// reconciles the two copies).
    pub fn scan_all(&mut self, probe_tails: bool) {
        for path in discover(&self.root) {
            self.read_path(&path, probe_tails);
        }
    }

    /// Tail one file: read bytes past the stored offset, consume complete
    /// lines. Handles truncation (offset beyond EOF) by restarting the file.
    /// Returns true if anything observable changed (new bytes or a newer
    /// mtime), so the caller knows a fresh snapshot is worth sending.
    pub fn read_path(&mut self, path: &Path, probe_tail: bool) -> bool {
        // Callers may hand us the same file under a symlinked spelling;
        // resolve so it maps to the root and keys a single TailState.
        let resolved;
        let path = if project_key_of(&self.root, path).is_some() {
            path
        } else {
            let Ok(p) = path.canonicalize() else {
                return false;
            };
            resolved = p;
            &resolved
        };
        let Some(project_key) = project_key_of(&self.root, path) else {
            return false;
        };
        let state = self.states.entry(path.to_path_buf()).or_insert(TailState {
            project_key: project_key.clone(),
            offset: 0,
            buf: LineBuffer::new(),
        });

        // open -> seek -> read -> close; never keep a handle on Claude Code's files.
        let Ok(mut file) = File::open(path) else {
            return false;
        };
        let meta = file.metadata().ok();
        let len = meta.as_ref().map(|m| m.len()).unwrap_or(0);

        // Any write to the file marks its project active, even if every new
        // line is a type the parser skips.
        let mut changed = false;
        if let Some(mtime) = meta.and_then(|m| m.modified().ok()) {
            let mtime = DateTime::<Utc>::from(mtime);
            let slot = self.arrivals.entry(project_key).or_insert(mtime);
            if mtime > *slot {
                *slot = mtime;
                changed = true;
            }
        }

        if len < state.offset {
            // Truncated/rewritten: restart from the top.
            state.offset = 0;
            state.buf.clear();
        }
        if len == state.offset {
            return changed;
        }
        if file.seek(SeekFrom::Start(state.offset)).is_err() {
            return changed;
        }
        let mut bytes = Vec::new();
        if file.read_to_end(&mut bytes).is_err() {
            return changed;
        }
        drop(file);
        state.offset += bytes.len() as u64;

        for line in state.buf.push(&bytes) {
            match parse_line(&line, &state.project_key) {
                LineOutcome::Record(r) => {
                    if let Some(cwd) = &r.cwd {
                        if let Some(name) = cwd.rsplit('/').find(|s| !s.is_empty()) {
                            self.names.insert(state.project_key.clone(), name.to_string());
                        }
                    }
                    self.records.push(*r);
                }
                LineOutcome::Skipped => {}
                LineOutcome::Malformed => self.malformed_lines += 1,
            }
        }
        if probe_tail {
            if let Some(tail) = state.buf.partial_str() {
                if let LineOutcome::Record(r) = parse_line(&tail, &state.project_key) {
                    self.records.push(*r);
                }
            }
        }
        true
    }

    /// Build a snapshot: dedup, merge with history, slice recent records.
    pub fn snapshot(&self, stored_history: &Cube) -> Snapshot {
        let (survivors, duplicates_skipped) = dedup(&self.records);
        let cube = history::merge(stored_history.clone(), &cube_from(survivors.iter().copied()));
        let cutoff = chrono::Utc::now() - chrono::Duration::minutes(75);
        let recent = survivors
            .iter()
            .filter(|r| r.timestamp >= cutoff)
            .map(|r| (*r).clone())
            .collect();
        let activity = self
            .arrivals
            .iter()
            .map(|(key, last_write)| ProjectActivity {
                name: self
                    .names
                    .get(key)
                    .cloned()
                    .unwrap_or_else(|| display_name_from_key(key)),
                last_write: *last_write,
            })
            .collect();
        Snapshot {
            cube,
            recent,
            activity,
            stats: ScanStats {
                files_scanned: self.files_seen(),
                malformed_lines: self.malformed_lines,
            },
            duplicates_skipped,
        }
    }
}

fn discover(root: &Path) -> Vec<PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, out);
            } else if path.extension().is_some_and(|e| e == "jsonl") {
                out.push(path);
            }
        }
    }
    let mut out = Vec::new();
    let Ok(projects) = std::fs::read_dir(root) else {
        return out;
    };
    for project in projects.flatten() {
        walk(&project.path(), &mut out);
    }
    out
}

/// Project key = first path component under the scan root.
fn project_key_of(root: &Path, path: &Path) -> Option<String> {
    path.strip_prefix(root)
        .ok()?
        .components()
        .next()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
}

/// Watcher/parser thread body. Performs the initial scan (merging and
/// persisting history once), then reacts to filesystem events, coalescing
/// bursts for ~200ms before sending a fresh snapshot. Exits when the UI
/// drops its receiver.
pub fn run(root: PathBuf, history_dir: Option<PathBuf>, tx: Sender<Snapshot>) {
    let root = root.canonicalize().unwrap_or(root);
    let mut tailer = Tailer::new(root.clone());
    tailer.scan_all(true);

    // History: load once, persist the merged cube once at startup (the only
    // write path; never under ~/.claude/).
    let stored = match &history_dir {
        Some(dir) => {
            let merged = history::merge(
                history::load(dir),
                &cube_from(dedup(&tailer.records).0),
            );
            if let Err(e) = history::persist(dir, &merged) {
                eprintln!("agentop: could not persist history: {e}");
            }
            history::load(dir)
        }
        None => Cube::new(),
    };

    if tx.send(tailer.snapshot(&stored)).is_err() {
        return;
    }

    let (ntx, nrx) = std::sync::mpsc::channel();
    let mut watcher = match notify::recommended_watcher(ntx) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("agentop: file watcher unavailable ({e}); showing startup data only");
            return; // UI keeps the startup snapshot
        }
    };
    if let Err(e) = watcher.watch(&root, RecursiveMode::Recursive) {
        eprintln!("agentop: cannot watch {}: {e}", root.display());
        return;
    }

    // Blocks between events: zero polling, near-zero idle CPU.
    while let Ok(event) = nrx.recv() {
        let mut changed = paths_of(event);
        // Coalesce the burst that streaming writes produce.
        let deadline = std::time::Instant::now() + Duration::from_millis(200);
        while let Some(left) = deadline.checked_duration_since(std::time::Instant::now()) {
            match nrx.recv_timeout(left) {
                Ok(event) => changed.extend(paths_of(event)),
                Err(_) => break,
            }
        }
        changed.sort();
        changed.dedup();

        // Send on any observable change — tool-result writes carry no usage
        // records but do flip a project to "live".
        let mut touched = false;
        for path in &changed {
            touched |= tailer.read_path(path, false);
        }
        if touched && tx.send(tailer.snapshot(&stored)).is_err() {
            return; // UI closed
        }
    }
}

fn paths_of(event: Result<notify::Event, notify::Error>) -> Vec<PathBuf> {
    match event {
        Ok(e) => e
            .paths
            .into_iter()
            .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
            .collect(),
        Err(_) => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;

    const LINE_A: &str = r#"{"type":"assistant","requestId":"req_A","timestamp":"2026-07-01T10:00:00.000Z","cwd":"/u/alpha","sessionId":"s1","message":{"id":"msg_A","model":"claude-opus-4-8","usage":{"input_tokens":100,"output_tokens":50,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}}}"#;
    const LINE_B: &str = r#"{"type":"assistant","requestId":"req_B","timestamp":"2026-07-01T11:00:00.000Z","cwd":"/u/alpha","sessionId":"s1","message":{"id":"msg_B","model":"claude-opus-4-8","usage":{"input_tokens":30,"output_tokens":7,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}}}"#;

    fn temp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("agentop-watch-{tag}-{}", std::process::id()))
            .join("-u-alpha");
        let _ = fs::remove_dir_all(dir.parent().unwrap());
        fs::create_dir_all(&dir).unwrap();
        dir.parent().unwrap().to_path_buf()
    }

    #[test]
    fn tails_appends_incrementally_across_torn_writes() {
        let root = temp_root("tail");
        let file = root.join("-u-alpha").join("s.jsonl");

        // Initial: one complete line + the first half of another.
        let (half1, half2) = LINE_B.split_at(120);
        fs::write(&file, format!("{LINE_A}\n{half1}")).unwrap();

        let mut tailer = Tailer::new(root.clone());
        tailer.scan_all(false);
        assert_eq!(tailer.records.len(), 1, "torn line must not be consumed");
        assert_eq!(tailer.malformed_lines, 0, "torn line must not be an error");

        // Append the second half; the buffered partial is prepended.
        let mut f = fs::OpenOptions::new().append(true).open(&file).unwrap();
        writeln!(f, "{half2}").unwrap();
        drop(f);
        tailer.read_path(&file, false);
        assert_eq!(tailer.records.len(), 2);
        assert_eq!(tailer.records[1].message_id.as_deref(), Some("msg_B"));
        assert_eq!(tailer.malformed_lines, 0);

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn truncated_file_restarts_from_zero() {
        let root = temp_root("trunc");
        let file = root.join("-u-alpha").join("s.jsonl");
        fs::write(&file, format!("{LINE_A}\n")).unwrap();

        let mut tailer = Tailer::new(root.clone());
        tailer.scan_all(false);
        assert_eq!(tailer.records.len(), 1);

        // Rewrite shorter than the old offset.
        fs::write(&file, format!("{LINE_B}\n")).unwrap();
        tailer.read_path(&file, false);
        assert_eq!(tailer.records.len(), 2);
        assert_eq!(tailer.records[1].message_id.as_deref(), Some("msg_B"));

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn event_paths_with_resolved_symlinks_still_match_the_root() {
        // macOS temp dirs live behind symlinks (/tmp -> /private/tmp); the
        // watcher delivers resolved paths, so the tailer must canonicalize.
        let root = temp_root("symlink");
        let file = root.join("-u-alpha").join("s.jsonl");
        fs::write(&file, format!("{LINE_A}\n")).unwrap();

        let mut tailer = Tailer::new(root.clone());
        let resolved = file.canonicalize().unwrap();
        assert!(tailer.read_path(&resolved, false));
        assert_eq!(tailer.records.len(), 1, "resolved event path must map to a project");

        fs::remove_dir_all(root.canonicalize().unwrap()).unwrap();
    }

    #[test]
    fn skipped_line_writes_still_mark_project_active() {
        let root = temp_root("activity");
        let file = root.join("-u-alpha").join("s.jsonl");
        fs::write(&file, format!("{LINE_A}\n")).unwrap();

        let mut tailer = Tailer::new(root.clone());
        tailer.scan_all(false);
        let before = tailer.snapshot(&Cube::new());
        assert_eq!(before.activity.len(), 1, "startup seeds activity from mtime");
        let t0 = before.activity[0].last_write;

        // A tool-result write: parser skips it, but the project is clearly live.
        std::thread::sleep(Duration::from_millis(1100)); // mtime granularity
        let mut f = fs::OpenOptions::new().append(true).open(&file).unwrap();
        writeln!(f, r#"{{"type":"user","message":{{"role":"user"}}}}"#).unwrap();
        drop(f);

        assert!(tailer.read_path(&file, false), "skipped-only write must report a change");
        let after = tailer.snapshot(&Cube::new());
        assert_eq!(after.activity.len(), 1);
        assert!(after.activity[0].last_write > t0, "last_write must advance");
        assert_eq!(after.activity[0].name, "alpha", "display name comes from records' cwd");

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn startup_probe_counts_unterminated_final_line_once() {
        let root = temp_root("probe");
        let file = root.join("-u-alpha").join("s.jsonl");
        // File ends without \n but the last line is complete JSON.
        fs::write(&file, format!("{LINE_A}\n{LINE_B}")).unwrap();

        let mut tailer = Tailer::new(root.clone());
        tailer.scan_all(true);
        assert_eq!(tailer.records.len(), 2);

        // The line later gets its newline plus a new record; dedup keeps one msg_B.
        let mut f = fs::OpenOptions::new().append(true).open(&file).unwrap();
        write!(f, "\n{LINE_A2}\n", LINE_A2 = LINE_A.replace("msg_A", "msg_C").replace("req_A", "req_C")).unwrap();
        drop(f);
        tailer.read_path(&file, false);

        let (survivors, dups) = dedup(&tailer.records);
        assert_eq!(survivors.len(), 3); // msg_A, msg_B, msg_C
        assert_eq!(dups, 1); // probed msg_B == completed msg_B

        fs::remove_dir_all(&root).unwrap();
    }
}
