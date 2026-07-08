#!/usr/bin/env node
// Thin launcher: the real Rust binary ships in a platform-specific package
// pulled in via optionalDependencies (esbuild-style). This shim just finds
// it and hands over the terminal.
"use strict";
const { execFileSync } = require("child_process");

const PLATFORMS = {
  "darwin arm64": "scorchtop-darwin-arm64",
  "darwin x64": "scorchtop-darwin-x64",
  "linux x64": "scorchtop-linux-x64",
};

const key = `${process.platform} ${process.arch}`;
const pkg = PLATFORMS[key];
if (!pkg) {
  console.error(`scorchtop: unsupported platform ${key}`);
  console.error("supported: macOS arm64/x64, Linux x64");
  process.exit(1);
}

let bin;
try {
  bin = require.resolve(`${pkg}/bin/scorchtop`);
} catch {
  console.error(`scorchtop: platform package "${pkg}" is missing.`);
  console.error("Your package manager may have skipped optional dependencies;");
  console.error("try reinstalling without --no-optional / --omit=optional.");
  process.exit(1);
}

try {
  execFileSync(bin, process.argv.slice(2), { stdio: "inherit" });
} catch (e) {
  if (e.status === null || e.status === undefined) throw e;
  process.exit(e.status);
}
