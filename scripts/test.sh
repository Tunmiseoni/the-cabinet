#!/bin/bash
# test — typecheck/build the frontend, then run Rust fmt, tests and clippy.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

[ -f "$HOME/.cargo/env" ] && source "$HOME/.cargo/env"
command -v cargo >/dev/null 2>&1 || { echo "cargo not found — run the rustup installer (see AGENTS.md)" >&2; exit 1; }

echo "==> frontend build (tsc + vite)"
npm run build

echo "==> cargo fmt"
(cd src-tauri && cargo fmt --all --check)

echo "==> cargo test"
(cd src-tauri && cargo test --workspace)

echo "==> cargo clippy"
(cd src-tauri && cargo clippy --workspace --all-targets -- -D warnings)

echo
echo "All checks passed."
echo "Live hardware tests (Tailscale, ROM scan, emulator launch):"
echo "  (cd src-tauri && cargo test -- --ignored --nocapture)"
