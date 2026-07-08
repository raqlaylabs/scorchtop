#!/usr/bin/env bash
# Stream fake Claude Code traffic into a sandbox so the dashboard dances.
# Never touches the real ~/.claude. Usage:
#
#   terminal 1:  ./scripts/demo-traffic.sh            # writes data + prints the run command
#   terminal 2:  HOME=<sandbox>/home XDG_DATA_HOME=<sandbox>/xdg ./target/debug/scorchtop
#
set -euo pipefail

SANDBOX="${SCORCHTOP_DEMO_DIR:-${TMPDIR:-/tmp}/scorchtop-demo}"
ROOT="$SANDBOX/home/.claude/projects"
PROJECTS=(mira-app scorchtop web-frontend)
MODELS=(claude-opus-4-8 claude-sonnet-5 claude-haiku-4-5)

mkdir -p "$SANDBOX/xdg"
for p in "${PROJECTS[@]}"; do
  mkdir -p "$ROOT/-tmp-$p"
done

echo "sandbox: $SANDBOX"
echo
echo "run the dashboard in another terminal:"
echo
echo "  HOME=$SANDBOX/home XDG_DATA_HOME=$SANDBOX/xdg ./target/debug/scorchtop"
echo
echo "streaming fake traffic (ctrl-c to stop)…"

emit() {
  local project="$1"
  local model="${MODELS[$((RANDOM % ${#MODELS[@]}))]}"
  local ts in out cw cr
  ts=$(date -u +%Y-%m-%dT%H:%M:%S.000Z)
  in=$((RANDOM % 20000 + 1000))
  out=$((RANDOM % 8000 + 200))
  cw=$((RANDOM % 30000))
  cr=$((RANDOM % 400000 + 20000))
  printf '{"type":"assistant","requestId":"req_%s%s","timestamp":"%s","cwd":"/tmp/%s","sessionId":"demo","message":{"id":"msg_%s%s","model":"%s","usage":{"input_tokens":%d,"output_tokens":%d,"cache_creation_input_tokens":%d,"cache_read_input_tokens":%d}}}\n' \
    "$RANDOM" "$RANDOM" "$ts" "$project" "$RANDOM" "$RANDOM" "$model" "$in" "$out" "$cw" "$cr" \
    >> "$ROOT/-tmp-$project/demo.jsonl"
}

while true; do
  # One project bursts hard, the others trickle — feels like real sessions.
  hot="${PROJECTS[$((RANDOM % ${#PROJECTS[@]}))]}"
  burst=$((RANDOM % 6 + 2))
  for _ in $(seq "$burst"); do
    emit "$hot"
    if (( RANDOM % 3 == 0 )); then
      emit "${PROJECTS[$((RANDOM % ${#PROJECTS[@]}))]}"
    fi
    sleep "0.$((RANDOM % 6 + 2))"
  done
  sleep "$((RANDOM % 3)).$((RANDOM % 9))"
done
