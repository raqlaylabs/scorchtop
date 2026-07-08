//! Claude Code adapter: parses `~/.claude/projects/**/*.jsonl`.
//!
//! `~/.claude/` is strictly read-only. Files are opened, read, and closed;
//! no handle is kept across calls.

use std::fs;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::Deserialize;

use super::{ScanResult, ScanStats, Source, TokenUsage, UsageRecord};
use crate::lines::LineBuffer;

pub struct ClaudeCodeSource {
    root: PathBuf,
}

impl ClaudeCodeSource {
    /// Standard location: `~/.claude/projects`.
    pub fn new() -> Option<Self> {
        let home = directories::BaseDirs::new()?.home_dir().to_path_buf();
        Some(Self { root: home.join(".claude").join("projects") })
    }

    /// Alternate root, for tests and fixtures.
    pub fn with_root(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn jsonl_files(&self) -> Vec<(String, PathBuf)> {
        let mut files = Vec::new();
        let Ok(projects) = fs::read_dir(&self.root) else {
            return files;
        };
        for project in projects.flatten() {
            let project_key = project.file_name().to_string_lossy().into_owned();
            // Sessions can nest subagent transcripts in subdirectories
            // (`<project>/<session>/subagents/*.jsonl`) — walk recursively.
            collect_jsonl(&project.path(), &project_key, &mut files);
        }
        files
    }
}

fn collect_jsonl(dir: &Path, project_key: &str, files: &mut Vec<(String, PathBuf)>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_jsonl(&path, project_key, files);
        } else if path.extension().is_some_and(|e| e == "jsonl") {
            files.push((project_key.to_string(), path));
        }
    }
}

impl Source for ClaudeCodeSource {
    fn name(&self) -> &'static str {
        "claude-code"
    }

    fn scan(&self) -> ScanResult {
        let mut result = ScanResult::default();
        for (project_key, path) in self.jsonl_files() {
            // open -> read -> close; never hold a handle on Claude Code's files.
            let Ok(bytes) = fs::read(&path) else {
                continue;
            };
            result.stats.files_scanned += 1;
            scan_bytes(&bytes, &project_key, &mut result.records, &mut result.stats);
        }
        result
    }
}

/// Parse a full file's bytes, appending records and updating stats.
fn scan_bytes(bytes: &[u8], project_key: &str, records: &mut Vec<UsageRecord>, stats: &mut ScanStats) {
    let mut buf = LineBuffer::new();
    for line in buf.push(bytes) {
        match parse_line(&line, project_key) {
            LineOutcome::Record(r, _) => records.push(*r),
            LineOutcome::Skipped(_) => {}
            LineOutcome::Malformed => stats.malformed_lines += 1,
        }
    }
    // A file may legitimately end without a trailing newline, or Claude Code
    // may be mid-append. Parse the tail if it parses; a torn line is silently
    // ignored (never a parse error, and it will be complete on the next read).
    if let Some(tail) = buf.take_partial() {
        if let LineOutcome::Record(r, _) = parse_line(&tail, project_key) {
            records.push(*r);
        }
    }
}

pub enum LineOutcome {
    Record(Box<UsageRecord>, LineMeta),
    /// Valid JSON that isn't an assistant usage record (user turns,
    /// attachments, snapshots, ...). Not an error.
    Skipped(LineMeta),
    /// Not valid JSON.
    Malformed,
}

/// What a transcript line says about whether Claude is mid-turn. Drives the
/// live indicator: a user line (prompt or tool result) means work is starting
/// or continuing; an assistant `end_turn` means the reply is finished and the
/// human is reading/typing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TurnSignal {
    Working,
    TurnEnded,
    /// No turn information (snapshots, system lines, subagent sidechains).
    #[default]
    Neutral,
}

/// Everything one line says about the turn it belongs to, beyond usage.
#[derive(Debug, Clone, Copy, Default)]
pub struct LineMeta {
    pub signal: TurnSignal,
    /// `Some(chars)` when this is a human-typed prompt (`promptSource:
    /// "typed"`) — the start of a turn. Tool results, slash-command
    /// expansions, and skill injections are user lines too but carry `None`.
    pub prompt_chars: Option<u64>,
    /// The line's own timestamp, when it carries a parseable one.
    pub timestamp: Option<DateTime<Utc>>,
}

#[derive(Deserialize)]
struct RawLine {
    #[serde(rename = "type")]
    kind: Option<String>,
    #[serde(rename = "isSidechain")]
    is_sidechain: Option<bool>,
    message: Option<RawMessage>,
    #[serde(rename = "requestId")]
    request_id: Option<String>,
    timestamp: Option<String>,
    cwd: Option<String>,
    #[serde(rename = "sessionId")]
    session_id: Option<String>,
    #[serde(rename = "promptSource")]
    prompt_source: Option<String>,
}

#[derive(Deserialize)]
struct RawMessage {
    id: Option<String>,
    model: Option<String>,
    usage: Option<RawUsage>,
    stop_reason: Option<String>,
    /// Left as raw JSON: content shapes vary wildly across line types
    /// (string, block arrays, nested tool payloads) and a typed enum here
    /// would turn unexpected shapes into malformed-line counts.
    content: Option<serde_json::Value>,
}

#[derive(Deserialize, Default)]
struct RawUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    cache_creation_input_tokens: u64,
    #[serde(default)]
    cache_read_input_tokens: u64,
}

/// Parse one complete JSONL line.
pub fn parse_line(line: &str, project_key: &str) -> LineOutcome {
    let line = line.trim();
    if line.is_empty() {
        return LineOutcome::Skipped(LineMeta::default());
    }
    let raw: RawLine = match serde_json::from_str(line) {
        Ok(raw) => raw,
        Err(_) => return LineOutcome::Malformed,
    };
    // Subagent sidechains run inside the main turn; their signals would
    // flicker the state, so they carry none.
    let signal = if raw.is_sidechain == Some(true) {
        TurnSignal::Neutral
    } else {
        match raw.kind.as_deref() {
            // An interrupted request ends the turn: Claude was stopped and no
            // assistant end_turn will ever arrive.
            Some("user") if line.contains("[Request interrupted") => TurnSignal::TurnEnded,
            Some("user") => TurnSignal::Working,
            Some("assistant") => {
                let ended = raw
                    .message
                    .as_ref()
                    .and_then(|m| m.stop_reason.as_deref())
                    == Some("end_turn");
                if ended { TurnSignal::TurnEnded } else { TurnSignal::Working }
            }
            _ => TurnSignal::Neutral,
        }
    };
    let timestamp = raw
        .timestamp
        .as_deref()
        .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
        .map(|t| t.with_timezone(&Utc));
    let content = raw.message.as_ref().and_then(|m| m.content.as_ref());
    let meta = LineMeta {
        signal,
        prompt_chars: if raw.kind.as_deref() == Some("user")
            && raw.is_sidechain != Some(true)
            && raw.prompt_source.as_deref() == Some("typed")
        {
            Some(content.map_or(0, prompt_chars_of))
        } else {
            None
        },
        timestamp,
    };
    if raw.kind.as_deref() != Some("assistant") {
        return LineOutcome::Skipped(meta);
    }
    // Sidechain (subagent) edits included: they are real work product of the
    // enclosing turn.
    let lines_written = content.map_or(0, lines_written_of);
    let Some(message) = raw.message else {
        return LineOutcome::Skipped(meta);
    };
    let Some(usage) = message.usage else {
        return LineOutcome::Skipped(meta);
    };
    // Zero-usage records (e.g. `<synthetic>` placeholders) carry no signal
    // and would pollute model buckets / the unknown-pricing flag.
    if usage.input_tokens + usage.output_tokens + usage.cache_creation_input_tokens
        + usage.cache_read_input_tokens
        == 0
    {
        return LineOutcome::Skipped(meta);
    }
    let Some(timestamp) = timestamp else {
        return LineOutcome::Skipped(meta);
    };
    LineOutcome::Record(
        Box::new(UsageRecord {
            project_key: project_key.to_string(),
            cwd: raw.cwd,
            session_id: raw.session_id,
            timestamp,
            model: message.model.unwrap_or_else(|| "unknown".to_string()),
            message_id: message.id,
            request_id: raw.request_id,
            usage: TokenUsage {
                input: usage.input_tokens,
                output: usage.output_tokens,
                cache_create: usage.cache_creation_input_tokens,
                cache_read: usage.cache_read_input_tokens,
            },
            turn: None,
            lines_written,
        }),
        meta,
    )
}

/// Chars of human-typed prompt text: string content, or the text blocks of a
/// block array (images and other block types contribute nothing).
fn prompt_chars_of(content: &serde_json::Value) -> u64 {
    match content {
        serde_json::Value::String(s) => s.chars().count() as u64,
        serde_json::Value::Array(blocks) => blocks
            .iter()
            .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("text"))
            .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
            .map(|s| s.chars().count() as u64)
            .sum(),
        _ => 0,
    }
}

/// Lines *written* by an assistant message: newline-delimited lines of
/// `Write.content` and `Edit.new_string` tool inputs. Deliberately not
/// "final code" — later turns may rewrite these lines, and survival can't be
/// measured from transcripts alone.
fn lines_written_of(content: &serde_json::Value) -> u64 {
    let serde_json::Value::Array(blocks) = content else {
        return 0;
    };
    blocks
        .iter()
        .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("tool_use"))
        .filter_map(|b| {
            let field = match b.get("name").and_then(|n| n.as_str())? {
                "Write" => "content",
                "Edit" => "new_string",
                _ => return None,
            };
            let text = b.get("input")?.get(field)?.as_str()?;
            Some(text.lines().count() as u64)
        })
        .sum()
}

/// Best-effort decode of the encoded project directory name
/// (`-Users-naman-Documents-scorchtop` -> last path segment `scorchtop`).
/// Prefer `UsageRecord::cwd` when available; this is the fallback.
pub fn display_name_from_key(key: &str) -> String {
    key.rsplit('-')
        .find(|s| !s.is_empty())
        .unwrap_or(key)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    const ASSISTANT_LINE: &str = r#"{"type":"assistant","requestId":"req_1","timestamp":"2026-06-27T17:07:09.521Z","cwd":"/Users/naman/Documents/demo","sessionId":"s1","message":{"id":"msg_1","model":"claude-opus-4-8","role":"assistant","usage":{"input_tokens":100,"output_tokens":50,"cache_creation_input_tokens":10,"cache_read_input_tokens":5}}}"#;

    fn skipped_meta(line: &str) -> LineMeta {
        match parse_line(line, "p") {
            LineOutcome::Skipped(meta) => meta,
            _ => panic!("expected skipped line"),
        }
    }

    #[test]
    fn parses_assistant_record() {
        let LineOutcome::Record(r, meta) =
            parse_line(ASSISTANT_LINE, "-Users-naman-Documents-demo")
        else {
            panic!("expected record");
        };
        assert_eq!(r.model, "claude-opus-4-8");
        assert_eq!(r.message_id.as_deref(), Some("msg_1"));
        assert_eq!(r.request_id.as_deref(), Some("req_1"));
        assert_eq!(r.usage.total(), 165);
        assert_eq!(r.turn, None, "turn ids are assigned by the tailer");
        assert_eq!(meta.signal, TurnSignal::Working, "no stop_reason => mid-turn");
        assert!(meta.timestamp.is_some());
    }

    #[test]
    fn skips_non_assistant_lines() {
        let meta =
            skipped_meta(r#"{"type":"user","message":{"role":"user","content":"hi"}}"#);
        assert_eq!(meta.signal, TurnSignal::Working);
        assert_eq!(skipped_meta("").signal, TurnSignal::Neutral);
    }

    #[test]
    fn turn_signals_classify_end_turn_and_sidechains() {
        // end_turn on the assistant record finishes the turn.
        let done = ASSISTANT_LINE.replace(r#""role":"assistant""#, r#""stop_reason":"end_turn""#);
        assert!(matches!(
            parse_line(&done, "p"),
            LineOutcome::Record(_, LineMeta { signal: TurnSignal::TurnEnded, .. })
        ));
        // Sidechain (subagent) lines carry no turn signal.
        let side = ASSISTANT_LINE.replace(r#"{"type":"assistant""#, r#"{"isSidechain":true,"type":"assistant""#);
        assert!(matches!(
            parse_line(&side, "p"),
            LineOutcome::Record(_, LineMeta { signal: TurnSignal::Neutral, .. })
        ));
        // Non-record lines still classify: snapshots are neutral.
        assert_eq!(skipped_meta(r#"{"type":"file-history-snapshot"}"#).signal, TurnSignal::Neutral);
        // An interrupted request ends the turn even though it's a user line.
        assert_eq!(
            skipped_meta(r#"{"type":"user","message":{"content":"[Request interrupted by user]"}}"#)
                .signal,
            TurnSignal::TurnEnded
        );
    }

    #[test]
    fn typed_prompts_carry_char_counts_and_injected_user_lines_do_not() {
        // Human-typed prompt: promptSource "typed", string content.
        let typed = r#"{"type":"user","promptSource":"typed","timestamp":"2026-07-08T10:00:00.000Z","message":{"role":"user","content":"añade tests"}}"#;
        assert_eq!(skipped_meta(typed).prompt_chars, Some(11), "chars, not bytes");

        // Typed prompt with block content (text + image): text blocks only.
        let blocks = r#"{"type":"user","promptSource":"typed","message":{"content":[{"type":"text","text":"see this"},{"type":"image","source":{}}]}}"#;
        assert_eq!(skipped_meta(blocks).prompt_chars, Some(8));

        // Tool results, slash-command expansions, and sidechain prompts are
        // user lines but not typed prompts.
        let tool_result = r#"{"type":"user","message":{"content":[{"type":"tool_result","content":"ok"}]}}"#;
        assert_eq!(skipped_meta(tool_result).prompt_chars, None);
        let command = r#"{"type":"user","message":{"content":"<command-name>/model</command-name>"}}"#;
        assert_eq!(skipped_meta(command).prompt_chars, None);
        let side = r#"{"type":"user","isSidechain":true,"promptSource":"typed","message":{"content":"task"}}"#;
        assert_eq!(skipped_meta(side).prompt_chars, None);
    }

    #[test]
    fn counts_lines_written_by_write_and_edit_tools() {
        let content = r#"[
            {"type":"text","text":"editing now"},
            {"type":"tool_use","name":"Write","input":{"file_path":"/a","content":"one\ntwo\nthree\n"}},
            {"type":"tool_use","name":"Edit","input":{"file_path":"/b","old_string":"x\ny\nz","new_string":"x\nz"}},
            {"type":"tool_use","name":"Bash","input":{"command":"printf hi"}}
        ]"#
        .replace('\n', " ");
        let line =
            ASSISTANT_LINE.replace(r#""role":"assistant""#, &format!(r#""content":{content}"#));
        let LineOutcome::Record(r, _) = parse_line(&line, "p") else {
            panic!("expected record");
        };
        // Write: 3 lines, Edit new_string: 2 lines; Bash and old_string ignored.
        assert_eq!(r.lines_written, 5);

        // String content (no tool blocks) writes nothing.
        let plain = ASSISTANT_LINE.replace(r#""role":"assistant""#, r#""content":"just prose""#);
        let LineOutcome::Record(r, _) = parse_line(&plain, "p") else {
            panic!("expected record");
        };
        assert_eq!(r.lines_written, 0);
    }

    #[test]
    fn flags_invalid_json_as_malformed() {
        assert!(matches!(parse_line("{not json", "p"), LineOutcome::Malformed));
    }

    #[test]
    fn decodes_display_name() {
        assert_eq!(display_name_from_key("-Users-naman-Documents-scorchtop"), "scorchtop");
    }
}
