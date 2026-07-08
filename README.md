# scorchtop

**btop for AI coding agents.** A live terminal dashboard for your Claude Code
token usage — every project, every model, streaming in real time.

![scorchtop wrapped demo](assets/wrapped-demo.gif)

- **Live dashboard** — projects as gradient bars, a dancing per-project
  equalizer, tokens/min sparkline, burn rate, and a turns panel showing what
  each prompt cost. Reacts within ~1s of Claude Code streaming in another pane.
- **`scorchtop wrapped`** — a monthly shareable scorecard: GitHub-style daily
  heatmap, top projects, biggest session, busiest day. Press `r` to record a
  high-res GIF of it, `b` to blur project names for safe sharing.
- **Zero interference** — `~/.claude/` is strictly read-only. Event-driven
  file watching (no polling), near-zero CPU when idle, no network calls.

## Install

```sh
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/raqlaylabs/scorchtop/releases/latest/download/scorchtop-installer.sh | sh
```

or with npm:

```sh
npx scorchtop
```

or from source:

```sh
cargo install --git https://github.com/raqlaylabs/scorchtop
```

Prebuilt binaries: macOS (Apple Silicon + Intel) and Linux x64.

## Usage

```sh
scorchtop            # live dashboard
scorchtop wrapped    # monthly scorecard (add --blur for pseudonymous names)
scorchtop dump --json # aggregate totals, machine-readable
```

### Dashboard keys

| key | action |
| --- | ------ |
| `d` / `w` / `m` | today / last 7 days / last 30 days |
| `x` | color bars by model instead of rank |
| `t` | turns panel (prompt → tokens → lines written) |
| `q` | quit |

### Wrapped keys

| key | action |
| --- | ------ |
| `◂` `▸` | previous / next month |
| `b` | blur project names (stable pseudonyms) |
| `r` | record the entrance animation as a GIF |
| `q` | quit |

## Notes

- Dollar figures are **estimated API value** at public per-token rates — what
  the usage would cost via the API, not what your subscription charges.
- Data source: `~/.claude/projects/**/*.jsonl`, deduplicated by message and
  request id. Daily aggregates persist in `~/.local/share/scorchtop/` so
  history survives transcript pruning.
- Currently supports Claude Code; the source layer is a trait, adapters for
  other agents welcome.

## License

MIT. Bundled JetBrains Mono font is under the [OFL](assets/fonts/OFL.txt).
