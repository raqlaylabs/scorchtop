---
name: verify
description: Build, launch, and drive the agentop TUI against sandboxed fake data to verify changes end-to-end.
---

# Verifying agentop

## Golden rule

Never write into the real `~/.claude/` — not even test fixtures. To exercise
the live dashboard, override `HOME` (the JSONL root is `$HOME/.claude/projects`)
and `XDG_DATA_HOME` (isolates the history write path).

## Quick demo / animation tuning

`./scripts/demo-traffic.sh` streams fake bursty traffic into a sandbox
(`AGENTOP_DEMO_DIR` overrides the location) and prints the matching
`HOME=... XDG_DATA_HOME=... ./target/debug/agentop` command to run in a second
terminal. Use it to eyeball the equalizer physics and record GIFs.

## Recipe

1. Build: `cargo build` (binary at `target/debug/agentop`).
2. Fake data: `mkdir -p $SANDBOX/home/.claude/projects/-tmp-demoapp` and write
   JSONL lines shaped like:
   ```json
   {"type":"assistant","requestId":"req_1","timestamp":"2026-07-07T10:00:00.000Z","cwd":"/tmp/demoapp","sessionId":"s1","message":{"id":"msg_1","model":"claude-opus-4-8","usage":{"input_tokens":100,"output_tokens":50,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}}}
   ```
   Use current timestamps (`date -u +%Y-%m-%dT%H:%M:%S.000Z`) so records land
   in "today" / trigger the live indicator (2-minute window).
3. Launch in an isolated tmux server:
   `tmux -L agentop-verify new-session -d -x 120 -y 30 "HOME=$SANDBOX/home XDG_DATA_HOME=$SANDBOX/xdg ./target/debug/agentop"`
4. Observe: `tmux -L agentop-verify capture-pane -p`.
5. Live reactivity: append a line to a jsonl file, `sleep 1`, capture again —
   header totals/burn must change and bars re-sort within ~1s.
6. Keys: `tmux -L agentop-verify send-keys d|w|m|q`. Resize with
   `resize-window -x 80 -y 24` (must stay legible at 80×24).
7. Cleanup: `tmux -L agentop-verify kill-server`.

## Probes worth repeating

- Torn write: append the first N bytes of a line without `\n` → totals must
  not change and nothing crashes; append the rest + `\n` → counted once.
- Malformed line (`not json\n`) → skipped silently, app stays alive.
- Live dedup: re-append an existing (`message.id`, `requestId`) with larger
  `output_tokens` → totals grow only by the output delta.
- Write isolation: after quit, `find $SANDBOX/home/.claude -type f` shows only
  your fixtures; history lands in `$SANDBOX/xdg/agentop/history/daily.json`.
- Activity signal: append a `{"type":"user",...}` line (no usage) → project
  must flip to `● live` within ~1s (activity is mtime-based, not record-based).
  Backdate a file with `touch -t` before launch → must NOT show live at startup.

## Oracle (data correctness)

`bash scripts/verify.sh` — builds, runs `agentop dump --json` against the real
`~/.claude` (read-only), diffs per-day totals vs `npx ccusage@latest daily --json`.
Closed days must match exactly; today is skipped (still being written).
