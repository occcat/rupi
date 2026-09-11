#!/usr/bin/env bash
# 用 hyperfine 量 rupi 冷启动（--version / --help）。
# 用法：先 `cargo build --release -p rupi-cli`，再 `./scripts/bench-startup.sh [bin]`
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN="${1:-$ROOT/target/release/rupi}"
if [[ ! -x "$BIN" ]]; then
  echo "missing $BIN — build with: cargo build --release -p rupi-cli" >&2
  exit 1
fi
if ! command -v hyperfine >/dev/null 2>&1; then
  echo "hyperfine not found (cargo install hyperfine / apt install hyperfine)" >&2
  exit 1
fi
OUT_DIR="${2:-$ROOT/target}"
mkdir -p "$OUT_DIR"
hyperfine --warmup 3 --runs 25 \
  --export-json "$OUT_DIR/bench-startup.json" \
  --export-markdown "$OUT_DIR/bench-startup.md" \
  "$BIN --version" \
  "$BIN --help"
