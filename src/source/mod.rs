//! Data-source abstraction. v0.1 ships Claude Code only, but everything the
//! UI consumes flows through this trait so Codex/Gemini adapters can be added
//! later without touching the UI.

pub mod claude_code;

use chrono::{DateTime, Utc};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TokenUsage {
    pub input: u64,
    pub output: u64,
    pub cache_create: u64,
    pub cache_read: u64,
}

impl TokenUsage {
    pub fn total(&self) -> u64 {
        self.input + self.output + self.cache_create + self.cache_read
    }

    pub fn add(&mut self, other: &TokenUsage) {
        self.input += other.input;
        self.output += other.output;
        self.cache_create += other.cache_create;
        self.cache_read += other.cache_read;
    }
}

/// One assistant API call's worth of usage.
#[derive(Debug, Clone)]
pub struct UsageRecord {
    /// Stable per-project key (for Claude Code: the encoded directory name
    /// under `~/.claude/projects/`).
    pub project_key: String,
    /// Real working directory, when the record carries one. Preferred for
    /// display names.
    pub cwd: Option<String>,
    pub session_id: Option<String>,
    pub timestamp: DateTime<Utc>,
    pub model: String,
    pub message_id: Option<String>,
    pub request_id: Option<String>,
    pub usage: TokenUsage,
}

#[derive(Debug, Default, Clone)]
pub struct ScanStats {
    pub files_scanned: usize,
    pub malformed_lines: u64,
}

#[derive(Debug, Default)]
pub struct ScanResult {
    pub records: Vec<UsageRecord>,
    pub stats: ScanStats,
}

pub trait Source {
    fn name(&self) -> &'static str;
    /// Full scan of all usage data. Records are NOT deduplicated here; the
    /// aggregation layer owns dedup.
    fn scan(&self) -> ScanResult;
}
