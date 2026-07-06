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
            LineOutcome::Record(r) => records.push(*r),
            LineOutcome::Skipped => {}
            LineOutcome::Malformed => stats.malformed_lines += 1,
        }
    }
    // A file may legitimately end without a trailing newline, or Claude Code
    // may be mid-append. Parse the tail if it parses; a torn line is silently
    // ignored (never a parse error, and it will be complete on the next read).
    if let Some(tail) = buf.take_partial() {
        if let LineOutcome::Record(r) = parse_line(&tail, project_key) {
            records.push(*r);
        }
    }
}

pub enum LineOutcome {
    Record(Box<UsageRecord>),
    /// Valid JSON that isn't an assistant usage record (user turns,
    /// attachments, snapshots, ...). Not an error.
    Skipped,
    /// Not valid JSON.
    Malformed,
}

#[derive(Deserialize)]
struct RawLine {
    #[serde(rename = "type")]
    kind: Option<String>,
    message: Option<RawMessage>,
    #[serde(rename = "requestId")]
    request_id: Option<String>,
    timestamp: Option<String>,
    cwd: Option<String>,
    #[serde(rename = "sessionId")]
    session_id: Option<String>,
}

#[derive(Deserialize)]
struct RawMessage {
    id: Option<String>,
    model: Option<String>,
    usage: Option<RawUsage>,
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
        return LineOutcome::Skipped;
    }
    let raw: RawLine = match serde_json::from_str(line) {
        Ok(raw) => raw,
        Err(_) => return LineOutcome::Malformed,
    };
    if raw.kind.as_deref() != Some("assistant") {
        return LineOutcome::Skipped;
    }
    let Some(message) = raw.message else {
        return LineOutcome::Skipped;
    };
    let Some(usage) = message.usage else {
        return LineOutcome::Skipped;
    };
    let Some(timestamp) = raw
        .timestamp
        .as_deref()
        .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
        .map(|t| t.with_timezone(&Utc))
    else {
        return LineOutcome::Skipped;
    };
    LineOutcome::Record(Box::new(UsageRecord {
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
    }))
}

/// Best-effort decode of the encoded project directory name
/// (`-Users-naman-Documents-agentop` -> last path segment `agentop`).
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

    #[test]
    fn parses_assistant_record() {
        let LineOutcome::Record(r) = parse_line(ASSISTANT_LINE, "-Users-naman-Documents-demo")
        else {
            panic!("expected record");
        };
        assert_eq!(r.model, "claude-opus-4-8");
        assert_eq!(r.message_id.as_deref(), Some("msg_1"));
        assert_eq!(r.request_id.as_deref(), Some("req_1"));
        assert_eq!(r.usage.total(), 165);
    }

    #[test]
    fn skips_non_assistant_lines() {
        assert!(matches!(
            parse_line(r#"{"type":"user","message":{"role":"user","content":"hi"}}"#, "p"),
            LineOutcome::Skipped
        ));
        assert!(matches!(parse_line("", "p"), LineOutcome::Skipped));
    }

    #[test]
    fn flags_invalid_json_as_malformed() {
        assert!(matches!(parse_line("{not json", "p"), LineOutcome::Malformed));
    }

    #[test]
    fn decodes_display_name() {
        assert_eq!(display_name_from_key("-Users-naman-Documents-agentop"), "agentop");
    }
}
