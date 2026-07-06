#!/usr/bin/env bash
# Oracle check: diff agentop's per-day token totals against ccusage.
# Milestone 1 is not done until totals match within rounding.
set -euo pipefail
cd "$(dirname "$0")/.."

echo "==> building agentop"
cargo build --quiet --release

echo "==> running agentop dump --json"
./target/release/agentop dump --json > /tmp/agentop-dump.json

echo "==> running ccusage oracle (npx ccusage@latest daily --json)"
npx -y ccusage@latest daily --json > /tmp/ccusage-daily.json

echo "==> diffing per-day token totals"
python3 - <<'EOF'
import json, sys
from datetime import date

# Today's data is still being appended (including by the session running this
# script), so the two snapshots race each other — compare closed days only.
today = date.today().isoformat()

ours = {d["date"]: d for d in json.load(open("/tmp/agentop-dump.json"))["days"]}
theirs = {d["period"]: d for d in json.load(open("/tmp/ccusage-daily.json"))["daily"]}

fields = [
    ("input_tokens", "inputTokens"),
    ("output_tokens", "outputTokens"),
    ("cache_creation_tokens", "cacheCreationTokens"),
    ("cache_read_tokens", "cacheReadTokens"),
]

bad = 0
checked = 0
for day in sorted(set(ours) | set(theirs)):
    if day == today:
        print(f"  {day}: skipped (still being written)")
        continue
    o, t = ours.get(day), theirs.get(day)
    if o is None:
        print(f"  {day}: only in ccusage (missing from agentop)")
        bad += 1
        continue
    if t is None:
        # agentop merges persisted history, so it keeps days whose JSONL
        # transcripts Claude Code has already pruned. Not an error.
        print(f"  {day}: only in agentop (history of pruned transcripts) — ok")
        continue
    checked += 1
    for of, tf in fields:
        if o[of] != t[tf]:
            print(f"  {day}: {of} agentop={o[of]} ccusage={t[tf]} (diff {o[of]-t[tf]:+d})")
            bad += 1

if bad:
    print(f"\nFAIL: {bad} mismatches")
    sys.exit(1)
print(f"OK: {checked} closed days match ccusage exactly")
EOF
