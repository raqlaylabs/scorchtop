# agentop — project rules

**agentop** is a Rust TUI that visualizes Claude Code token usage across all projects in real time — "btop for AI coding agents". Screenshot-worthy tool, not an analytics suite.

These rules are **binding across all sessions**. If a task would violate one of them, stop and say so instead of proceeding.

## Identity

- Single binary. Rust 2021 edition, latest stable toolchain.
- v0.1 supports Claude Code only, but ALL data-source code lives behind a `Source` trait so Codex/Gemini adapters can be added later without touching the UI.
- Priorities, in order: (1) never interfere with Claude Code, (2) correct numbers, (3) visual polish, (4) features. When these conflict, the lower number wins.

## Hard runtime constraints (never violate)

- `~/.claude/` is **strictly read-only**. Never write, create, rename, or delete anything inside it — not even temp files or locks. All agentop state (byte offsets, caches) lives in `~/.local/share/agentop/` via the `directories` crate (`ProjectDirs`), so paths are correct per-platform.
- Never hold long-lived file handles on Claude Code's JSONL files. Per change event: open → seek to stored byte offset → read new bytes → close → update offset.
- JSONL files are appended to while we read. Only consume complete lines ending in `\n`; buffer any trailing partial line and prepend it on the next read. A torn final line must never produce a parse error or a dropped record.
- File watching is event-driven via the `notify` crate (FSEvents/inotify). No polling loops. Near-zero CPU when idle.
- Malformed line: skip it, increment a counter, continue. Never crash, never retry in a loop, never log spam.

## Hard architecture constraints (never violate)

- **Threading model:** one watcher/parser thread sends typed events over a `std::sync::mpsc` channel; the UI thread owns ALL mutable state. No `Arc<Mutex>`, no `RwLock`, no async, no tokio. If shared mutable state seems necessary, redesign so the data flows through the channel instead.
- **When ownership gets complicated, clone the data.** Never restructure a design to satisfy the borrow checker. We process a few KB per second; cloning is free.
- **The UI is dumb.** It renders an aggregate snapshot struct delivered over the channel. Zero business logic in rendering code. All logic lives in the parser/aggregation layer, which is pure and unit-tested.
- Allowed dependencies: `ratatui`, `crossterm`, `notify`, `serde`/`serde_json`, `directories`, `chrono` (or `jiff`), `clap`. **Ask the user before adding anything else.**

## Data correctness rules

- Source of truth: `~/.claude/projects/**/*.jsonl`. Lines with `"type":"assistant"` carry `message.usage` (`input_tokens`, `output_tokens`, `cache_creation_input_tokens`, `cache_read_input_tokens`), `message.model`, `message.id`, and top-level `requestId`, `timestamp`, `cwd`. Other line types (`user`, `attachment`, `file-history-snapshot`, …) are skipped, not errors.
- **Deduplicate** on (`message.id`, `requestId`). Streaming updates and retries produce duplicate records; without dedup all totals are wrong. Records missing an id are counted as-is.
- Project identity = directory name under `~/.claude/projects/` (it encodes the working directory path with `/` → `-`). Display name: prefer the last path component of the records' `cwd` field (the real path); fall back to best-effort decoding of the dir name.
- Dates are computed in the **local timezone** (matches ccusage defaults).
- Cost estimation uses a **static, hardcoded pricing table** keyed by model-name substring, per-million-token rates: input, output, cache write = 1.25× input, cache read = 0.1× input. Unknown model → count tokens, show cost as `—`, never guess. **No network calls, ever.** Label all dollar figures "est. API value" (subscription users don't pay per token).
- **Oracle check:** `scripts/verify.sh` runs `npx ccusage@latest daily --json` and diffs its per-day token totals against `agentop dump --json`. Milestone 1 is not done until totals match within rounding.

## Testing discipline

- Parser/aggregation is built test-first. Fixtures in `tests/fixtures/`:
  - Real JSONL files copied from `~/.claude/projects/` live in `tests/fixtures/real/` — **git-ignored until the user confirms they contain nothing sensitive**. Tests using them must skip gracefully when absent.
  - Hand-crafted fixtures (committed) for edge cases: torn last line, duplicate message ids, unknown model, malformed line mid-file, empty file.
- `cargo test` and `cargo clippy -- -D warnings` must pass before any milestone is called done. Run them yourself; don't ask the user to judge Rust code.
- Commit at every green-test checkpoint with conventional commit messages.

## Milestones (work strictly in order)

1. **Parser core (no UI).** Cargo scaffold, `Source` trait, Claude Code JSONL parser, dedup, aggregation by project/day/model, pricing table, fixtures + tests, ccusage oracle verification. Deliverable: `agentop dump --json`.
2. **Static dashboard.** Ratatui full-screen layout: header (today's est. cost, total tokens, burn-rate placeholder), main panel (projects as horizontal unicode-block bars sorted by tokens, per-project cost), footer (keybind hints). Keys: `d`/`w`/`m` period switch, `q` quit. One full parse at startup, no live updates. Must look good at 80×24 and 200×50.
3. **Live mode.** notify watcher thread + mpsc + incremental tailing per constraints above. Animated bars, tokens/min sparkline (last hour), live-ticking today's cost, active-session indicator. Dashboard must visibly react within ~1s of Claude Code streaming in another pane.
4. **`agentop wrapped`.** Monthly shareable summary: total tokens, est. API value, most expensive project, biggest session, GitHub-style per-day heatmap. Screenshot-worthy.
5. **Distribution.** cargo-dist + GitHub Actions release matrix (macOS arm64/x64, Linux x64), `curl | sh` installer, npm wrapper (`npx agentop`, esbuild-style optionalDependencies). README with install one-liners and GIF placeholder.

At each milestone boundary: state what was verified (tests, clippy, oracle diff) in 2–3 lines. No long Rust explanations unless asked.
