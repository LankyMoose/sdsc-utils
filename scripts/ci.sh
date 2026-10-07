#!/usr/bin/env bash
# Shared local + GitHub Actions checklist (fmt, clippy, test, build).
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

if ! rustfmt --version >/dev/null 2>&1; then
  echo "error: rustfmt not found. Install with: rustup component add rustfmt" >&2
  exit 1
fi
if ! cargo clippy --version >/dev/null 2>&1; then
  echo "error: clippy not found. Install with: rustup component add clippy" >&2
  exit 1
fi

echo "==> cargo fmt --all -- --check"
cargo fmt --all -- --check

echo "==> cargo clippy --all-targets -- -D warnings"
cargo clippy --all-targets -- -D warnings

echo "==> cargo test --all-targets"
cargo test --all-targets

# Test builds don't refresh target/debug/*.exe; build every binary so a green
# run leaves runnable sdsc-utils + sdsc-shell that match the checked source.
echo "==> cargo build"
cargo build

echo "CI checks passed."
