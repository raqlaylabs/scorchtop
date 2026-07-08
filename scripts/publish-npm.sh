#!/usr/bin/env bash
# Publish scorchtop to npm from a tagged GitHub release.
#
#   ./scripts/publish-npm.sh 0.1.0
#
# Downloads the release tarballs, wraps each binary in a platform package
# (scorchtop-darwin-arm64, …), publishes those, then publishes the main
# "scorchtop" wrapper that references them via optionalDependencies.
# Requires: npm (logged in), curl, tar with xz support.
set -euo pipefail

VERSION="${1:?usage: publish-npm.sh <version, e.g. 0.1.0>}"
REPO="$(cd "$(dirname "$0")/.." && pwd)"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

# rust target triple -> npm platform suffix / os / cpu
TARGETS=(
  "aarch64-apple-darwin darwin-arm64 darwin arm64"
  "x86_64-apple-darwin darwin-x64 darwin x64"
  "x86_64-unknown-linux-gnu linux-x64 linux x64"
)

BASE="https://github.com/raqlaylabs/scorchtop/releases/download/v$VERSION"
for spec in "${TARGETS[@]}"; do
  read -r triple suffix os cpu <<<"$spec"
  curl -fLsS -o "$WORK/scorchtop-$triple.tar.xz" "$BASE/scorchtop-$triple.tar.xz"
  pkg="$WORK/pkg-$suffix"
  mkdir -p "$pkg/bin"
  tar -xJf "$WORK/scorchtop-$triple.tar.xz" -C "$WORK"
  cp "$WORK/scorchtop-$triple/scorchtop" "$pkg/bin/scorchtop"
  chmod +x "$pkg/bin/scorchtop"
  cat > "$pkg/package.json" <<EOF
{
  "name": "scorchtop-$suffix",
  "version": "$VERSION",
  "description": "scorchtop binary for $os $cpu",
  "repository": "github:raqlaylabs/scorchtop",
  "license": "MIT",
  "os": ["$os"],
  "cpu": ["$cpu"]
}
EOF
  (cd "$pkg" && npm publish --access public)
done

# Main wrapper: pin its own version and the platform package versions.
MAIN="$WORK/scorchtop"
cp -R "$REPO/npm/scorchtop" "$MAIN"
node -e "
  const fs = require('fs');
  const p = JSON.parse(fs.readFileSync('$MAIN/package.json', 'utf8'));
  p.version = '$VERSION';
  for (const k of Object.keys(p.optionalDependencies)) {
    p.optionalDependencies[k] = '$VERSION';
  }
  fs.writeFileSync('$MAIN/package.json', JSON.stringify(p, null, 2) + '\n');
"
(cd "$MAIN" && npm publish --access public)
echo "published scorchtop@$VERSION and platform packages"
