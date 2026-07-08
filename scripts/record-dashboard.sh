#!/usr/bin/env bash
# Record a GIF of the live dashboard — equalizer dancing while demo traffic
# streams in — using vhs (brew install vhs). The real ~/.claude is never
# touched. Output defaults to assets/dashboard-demo.gif (+ .mp4).
set -euo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
OUT="${1:-$REPO/assets/dashboard-demo.gif}"
SB="${SCORCHTOP_DEMO_DIR:-${TMPDIR:-/tmp}/scorchtop-dashboard-demo}"

command -v vhs >/dev/null || { echo "vhs not found: brew install vhs" >&2; exit 1; }

cargo build --release --manifest-path "$REPO/Cargo.toml"
mkdir -p "$(dirname "$OUT")"

rm -rf "$SB"
# demo-traffic.sh creates the sandbox layout and streams bursts into it.
SCORCHTOP_DEMO_DIR="$SB" "$REPO/scripts/demo-traffic.sh" >/dev/null 2>&1 &
TRAFFIC_PID=$!
trap 'kill "$TRAFFIC_PID" 2>/dev/null || true' EXIT
sleep 3 # let a little history accumulate so bars have length at launch

TAPE="$SB/dashboard.tape"
cat > "$TAPE" <<EOF
Output dashboard-demo.gif
Output dashboard-demo.mp4
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
Type "scorchtop"
Sleep 500ms
Enter
Sleep 9s
Type "q"
Sleep 300ms
EOF

(cd "$SB" && vhs "$TAPE")
mv "$SB/dashboard-demo.gif" "$OUT"
mv "$SB/dashboard-demo.mp4" "${OUT%.gif}.mp4"
echo "wrote $OUT and ${OUT%.gif}.mp4"
