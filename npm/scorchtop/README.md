# scorchtop

**btop for AI coding agents.** A live terminal dashboard for your Claude Code
token usage — every project, every model, streaming in real time.

![scorchtop live dashboard](https://raw.githubusercontent.com/raqlaylabs/scorchtop/main/assets/dashboard-demo.gif)

```sh
npx scorchtop            # live dashboard
npx scorchtop wrapped    # monthly shareable scorecard (b to blur names, r to record a GIF)
```

- Live per-project equalizer, gradient bars, burn rate, turns panel
- `wrapped`: GitHub-style daily heatmap, top projects, biggest session
- `~/.claude/` is strictly read-only; no polling, no network calls
- Dollar figures are estimated API value, not your subscription price

This package is a thin launcher; the Rust binary ships in a platform package
(`scorchtop-darwin-arm64`, `scorchtop-darwin-x64`, `scorchtop-linux-x64`)
selected automatically via optionalDependencies.

Docs, source, and prebuilt installers: https://github.com/raqlaylabs/scorchtop
