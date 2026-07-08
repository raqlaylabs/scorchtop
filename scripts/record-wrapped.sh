#!/usr/bin/env bash
# Record a shareable GIF of `agentop wrapped` using vhs (brew install vhs).
#
# Seeds a sandbox with a deterministic fake month of usage (the real
# ~/.claude is never read or written), then records: entrance animation on
# the current month, arrow-left to last month's full heatmap replay, and the
# blur toggle. Output defaults to assets/wrapped-demo.gif.
set -euo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
OUT="${1:-$REPO/assets/wrapped-demo.gif}"
SB="${AGENTOP_DEMO_DIR:-${TMPDIR:-/tmp}/agentop-wrapped-demo}"

command -v vhs >/dev/null || { echo "vhs not found: brew install vhs" >&2; exit 1; }

cargo build --release --manifest-path "$REPO/Cargo.toml"
mkdir -p "$(dirname "$OUT")"

# --- seed demo data ---------------------------------------------------------
rm -rf "$SB"
mkdir -p "$SB/xdg"

# project name | model | tokens-per-day scale
PROJECTS=(
  "nebula    claude-opus-4-8   9"
  "kestrel   claude-sonnet-5   5"
  "tidepool  claude-fable-5    2"
  "lantern   claude-haiku-4-5  1"
)

THIS_MONTH=$(date +%Y-%m)
LAST_MONTH=$(date -v-1m +%Y-%m 2>/dev/null || date -d "1 month ago" +%Y-%m)
TODAY=$(date +%d | sed 's/^0//')
DAYS_LAST=$(date -v-1m -v+1m -v1d -v-1d +%d 2>/dev/null || date -d "$THIS_MONTH-01 -1 day" +%d)

emit() { # $1 month(YYYY-MM) $2 day $3 name $4 model $5 scale
  local day; day=$(printf '%02d' "$2")
  local dir="$SB/home/.claude/projects/-tmp-$3"
  mkdir -p "$dir"
  # Deterministic per-day rhythm: weekly wave + per-project jitter, so the
  # heatmap has texture without $RANDOM flakiness.
  local wave=$(( ($2 * 13 + ${#3}) % 7 + 1 ))                # 1..7
  local input=$(( $5 * wave * 11000 ))
  local cache=$(( input * 12 ))
  local output=$(( input / 20 ))
  printf '{"type":"assistant","requestId":"req_%s_%s_%s","timestamp":"%s-%sT10:00:00.000Z","cwd":"/tmp/%s","sessionId":"s_%s_%s","message":{"id":"msg_%s_%s_%s","model":"%s","usage":{"input_tokens":%s,"output_tokens":%s,"cache_creation_input_tokens":0,"cache_read_input_tokens":%s}}}\n' \
    "$3" "$1" "$day" "$1" "$day" "$3" "$3" "$1$day" "$3" "$1" "$day" "$4" "$input" "$output" "$cache" \
    >> "$dir/demo.jsonl"
}

for spec in "${PROJECTS[@]}"; do
  read -r name model scale <<<"$spec"
  # Last month: most days active (skip a few for texture).
  for ((d = 1; d <= DAYS_LAST; d++)); do
    (( (d * 7 + scale) % 9 == 0 )) && continue
    emit "$LAST_MONTH" "$d" "$name" "$model" "$scale"
  done
  # This month so far.
  for ((d = 1; d <= TODAY; d++)); do
    (( (d + scale) % 5 == 0 )) && continue
    emit "$THIS_MONTH" "$d" "$name" "$model" "$scale"
  done
done
# One monster session mid-last-month for the "biggest session" highlight.
emit "$LAST_MONTH" 14 nebula claude-opus-4-8 60

# --- record -----------------------------------------------------------------
# vhs's tape parser rejects absolute Output paths; record relative to the
# sandbox and move the results afterwards. GIF for README inlining, MP4 for
# crisp full-color sharing (GitHub embeds mp4 natively).
TAPE="$SB/wrapped.tape"
cat > "$TAPE" <<EOF
Output wrapped-demo.gif
Output wrapped-demo.mp4
Set FontSize 22
Set Width 1600
Set Height 900
Set Padding 12
Set Framerate 60
Set TypingSpeed 40ms
Hide
Type "export HOME='$SB/home' XDG_DATA_HOME='$SB/xdg' PATH='$REPO/target/release':\$PATH; clear"
Enter
Show
Sleep 300ms
Type "agentop wrapped"
Sleep 600ms
Enter
Sleep 2.5s
Left
Sleep 2.5s
Type "b"
Sleep 2s
Type "q"
Sleep 300ms
EOF

(cd "$SB" && vhs "$TAPE")
mv "$SB/wrapped-demo.gif" "$OUT"
mv "$SB/wrapped-demo.mp4" "${OUT%.gif}.mp4"
echo "wrote $OUT and ${OUT%.gif}.mp4"
