#!/usr/bin/env bash
# Release builds embed the pinned platform-specific CLI, not Chrome itself.
set -euo pipefail
browser_dir="${RUNNER_TEMP:?RUNNER_TEMP is required}/kathamo-agent-browser"
npm install --prefix "$browser_dir" --ignore-scripts --no-audit --no-fund agent-browser@0.43.0
browser_binary=$(find "$browser_dir/node_modules/@agent-browser" -type f \( -name agent-browser -o -name agent-browser.exe \) -path '*/bin/*' -print -quit)
test -n "$browser_binary"
if command -v cygpath >/dev/null 2>&1; then
  browser_binary=$(cygpath -w "$browser_binary")
fi
printf 'KATHAMO_AGENT_BROWSER_BIN=%s\n' "$browser_binary" >> "${GITHUB_ENV:?GITHUB_ENV is required}"
