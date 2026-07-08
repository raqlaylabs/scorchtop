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
use crate::source::claude_code::{display_name_from_key, parse_line, LineMeta, LineOutcome, TurnSignal};
use crate::source::{ScanStats, UsageRecord};
use crate::view::TurnRow;

/// Per-project turn state. `working` follows the transcript's own protocol —
/// a user line (prompt or tool result) starts a turn, an assistant
/// `end_turn` finishes it — so "live" means Claude is actually working, not
/// "the user replied recently". `last_write` (file mtime) is kept as a crash
/// guard: a session that died mid-turn stops counting once writes go stale.
#[derive(Debug, Clone)]
pub struct ProjectActivity {
    pub name: String,
    pub working: bool,
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
    /// Recent prompt→reply turns, newest first, for the turns panel.
    pub turns: Vec<TurnRow>,
    pub stats: ScanStats,
    pub duplicates_skipped: u64,
}

/// One typed-prompt→end_turn span in a session file. Records that arrive
/// while the turn is open carry its `id`, so per-turn tokens survive dedup.
#[derive(Debug, Clone)]
pub struct TurnMeta {
    /// Tailer-wide unique id (never reused across files).
    pub id: u32,
    pub prompt_chars: u64,
    pub started: DateTime<Utc>,
    /// `None` while the turn is still streaming.
    pub ended: Option<DateTime<Utc>>,
}

/// Turns kept per session file; older ones fall off (the panel shows fewer).
const MAX_TURNS_PER_FILE: usize = 12;

struct TailState {
    project_key: String,
    /// Bytes consumed so far (including any buffered partial line).
    offset: u64,
    buf: LineBuffer,
    /// Whether this session is mid-turn: set by user lines (prompts, tool
    /// results), cleared by an assistant `end_turn` or interrupt marker.
    working: bool,
    /// This file's own mtime — the crash guard must be per session, or one
    /// stale mid-turn file keeps its whole project "live" forever.
    last_write: DateTime<Utc>,
    /// Id of the turn currently open in this file, if any.
    current_turn: Option<u32>,
    /// Recent turns of this file, oldest first.
    turns: Vec<TurnMeta>,
}

impl TailState {
    /// Fold one parsed line's turn information into this file's turn list.
    /// `next_turn` is the tailer-wide id counter.
    fn apply(&mut self, meta: &LineMeta, next_turn: &mut u32, now: DateTime<Utc>) {
        let ts = meta.timestamp.unwrap_or(now);
        if let Some(chars) = meta.prompt_chars {
            // A new prompt while a turn is still open (e.g. an interrupt that
            // never hit the transcript) closes the old turn.
            if let Some(open) = self.open_turn() {
                open.ended = Some(ts);
            }
            *next_turn += 1;
            self.current_turn = Some(*next_turn);
            self.turns.push(TurnMeta { id: *next_turn, prompt_chars: chars, started: ts, ended: None });
            if self.turns.len() > MAX_TURNS_PER_FILE {
                let excess = self.turns.len() - MAX_TURNS_PER_FILE;
                self.turns.drain(..excess);
            }
        }
        match meta.signal {
            TurnSignal::Working => self.working = true,
            TurnSignal::TurnEnded => {
                self.working = false;
                if let Some(open) = self.open_turn() {
                    open.ended = Some(ts);
                }
                self.current_turn = None;
            }
            TurnSignal::Neutral => {}
        }
    }

    fn open_turn(&mut self) -> Option<&mut TurnMeta> {
        let id = self.current_turn?;
        self.turns.iter_mut().find(|t| t.id == id)
    }
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
    /// Tailer-wide turn id counter; see `TurnMeta::id`.
    next_turn: u32,
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
            next_turn: 0,
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
            working: false,
            last_write: DateTime::<Utc>::MIN_UTC,
            current_turn: None,
            turns: Vec::new(),
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
            if mtime > state.last_write {
                state.last_write = mtime;
            }
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

        let now = Utc::now();
        for line in state.buf.push(&bytes) {
            match parse_line(&line, &state.project_key) {
                LineOutcome::Record(mut r, meta) => {
                    // Tag before apply: the end_turn record belongs to the
                    // turn it closes (records never open turns themselves).
                    r.turn = state.current_turn;
                    state.apply(&meta, &mut self.next_turn, now);
                    if let Some(cwd) = &r.cwd {
                        if let Some(name) = cwd.rsplit('/').find(|s| !s.is_empty()) {
                            self.names.insert(state.project_key.clone(), name.to_string());
                        }
                    }
                    self.records.push(*r);
                }
                LineOutcome::Skipped(meta) => state.apply(&meta, &mut self.next_turn, now),
                LineOutcome::Malformed => self.malformed_lines += 1,
            }
        }
        if probe_tail {
            if let Some(tail) = state.buf.partial_str() {
                if let LineOutcome::Record(mut r, meta) = parse_line(&tail, &state.project_key) {
                    // The torn line stays buffered and is parsed again once
                    // complete, so only the idempotent working flag is
                    // applied here — turn/line counting would double.
                    match meta.signal {
                        TurnSignal::Working => state.working = true,
                        TurnSignal::TurnEnded => state.working = false,
                        TurnSignal::Neutral => {}
                    }
                    r.turn = state.current_turn;
                    self.records.push(*r);
                }
            }
        }
        true
    }

    /// Build a snapshot: dedup, merge with history, slice recent records.
    pub fn snapshot(&self, stored_history: &Cube) -> Snapshot {
        let now = chrono::Utc::now();
        let (survivors, duplicates_skipped) = dedup(&self.records);
        let cube = history::merge(stored_history.clone(), &cube_from(survivors.iter().copied()));
        let cutoff = now - chrono::Duration::minutes(75);
        let recent = survivors
            .iter()
            .filter(|r| r.timestamp >= cutoff)
            .map(|r| (*r).clone())
            .collect();
        // A project is mid-turn if any of its sessions is. Report the newest
        // write among its *working* sessions, so the staleness guard judges
        // the file that claims to be working — one abandoned mid-turn
        // transcript must not ride on a sibling session's fresh mtime.
        let mut working: HashMap<&str, DateTime<Utc>> = HashMap::new();
        for state in self.states.values() {
            if state.working {
                let newest = working.entry(state.project_key.as_str()).or_insert(state.last_write);
                if state.last_write > *newest {
                    *newest = state.last_write;
                }
            }
        }
        let activity = self
            .arrivals
            .iter()
            .map(|(key, last_write)| {
                let work = working.get(key.as_str()).copied();
                ProjectActivity {
                    name: self
                        .names
                        .get(key)
                        .cloned()
                        .unwrap_or_else(|| display_name_from_key(key)),
                    working: work.is_some(),
                    last_write: work.unwrap_or(*last_write),
                }
            })
            .collect();
        let mut turn_src: Vec<(String, DateTime<Utc>, TurnMeta)> = Vec::new();
        for state in self.states.values() {
            let name = self
                .names
                .get(&state.project_key)
                .cloned()
                .unwrap_or_else(|| display_name_from_key(&state.project_key));
            for meta in &state.turns {
                turn_src.push((name.clone(), state.last_write, meta.clone()));
            }
        }
        let turns = crate::view::turn_rows(&turn_src, &survivors, now);
        Snapshot {
            cube,
            recent,
            activity,
            turns,
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
    fn turn_state_follows_user_lines_and_end_turn() {
        let root = temp_root("activity");
        let file = root.join("-u-alpha").join("s.jsonl");
        // Assistant record without stop_reason == mid-turn.
        fs::write(&file, format!("{LINE_A}\n")).unwrap();

        let mut tailer = Tailer::new(root.clone());
        tailer.scan_all(false);
        let before = tailer.snapshot(&Cube::new());
        assert_eq!(before.activity.len(), 1, "startup seeds activity from mtime");
        assert!(before.activity[0].working, "assistant mid-turn record => working");
        let t0 = before.activity[0].last_write;

        // The reply completes: assistant end_turn => the human is reading.
        std::thread::sleep(Duration::from_millis(1100)); // mtime granularity
        let done = LINE_B.replace(r#""model""#, r#""stop_reason":"end_turn","model""#);
        let mut f = fs::OpenOptions::new().append(true).open(&file).unwrap();
        writeln!(f, "{done}").unwrap();
        drop(f);
        assert!(tailer.read_path(&file, false));
        let after = tailer.snapshot(&Cube::new());
        assert!(!after.activity[0].working, "end_turn must clear working");
        assert!(after.activity[0].last_write > t0, "last_write must advance");
        assert_eq!(after.activity[0].name, "alpha", "display name comes from records' cwd");

        // A tool-result/prompt write: parser skips it, but a turn is running.
        let mut f = fs::OpenOptions::new().append(true).open(&file).unwrap();
        writeln!(f, r#"{{"type":"user","message":{{"role":"user"}}}}"#).unwrap();
        drop(f);
        assert!(tailer.read_path(&file, false), "skipped-only write must report a change");
        assert!(tailer.snapshot(&Cube::new()).activity[0].working, "user line => working");

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn stale_mid_turn_session_cannot_ride_a_siblings_fresh_mtime() {
        let root = temp_root("stale");
        let dir = root.join("-u-alpha");

        // Session 1: abandoned mid-turn (user line, no end_turn), 20 min old.
        let stale = dir.join("stale.jsonl");
        fs::write(&stale, format!("{LINE_A}\n{{\"type\":\"user\",\"message\":{{}}}}\n")).unwrap();
        let f = fs::OpenOptions::new().write(true).open(&stale).unwrap();
        f.set_modified(std::time::SystemTime::now() - Duration::from_secs(1200)).unwrap();
        drop(f);

        // Session 2: finished cleanly just now.
        let done = LINE_B.replace(r#""model""#, r#""stop_reason":"end_turn","model""#);
        fs::write(dir.join("fresh.jsonl"), format!("{done}\n")).unwrap();

        let mut tailer = Tailer::new(root.clone());
        tailer.scan_all(false);
        let snap = tailer.snapshot(&Cube::new());
        assert_eq!(snap.activity.len(), 1);
        let a = &snap.activity[0];
        assert!(a.working, "the stale session is still nominally mid-turn");
        let age = chrono::Utc::now() - a.last_write;
        assert!(
            age >= chrono::Duration::minutes(19),
            "last_write must come from the working file, not the fresh sibling (age {age})"
        );

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
