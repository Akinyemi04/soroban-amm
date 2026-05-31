#!/usr/bin/env bash
set -euo pipefail

# Build and optimize all contracts producing wasm artifacts.
# Requires: cargo, wasm-opt (binaryen) optional but recommended.

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CONTRACTS_DIR="$ROOT_DIR/contracts"
TARGET=wasm32-unknown-unknown

echo "[optimize] Starting contract optimization"

for d in "$CONTRACTS_DIR"/*/; do
  if [ -f "$d/Cargo.toml" ]; then
    name=$(basename "$d")
    echo "\n[optimize] Building contract: $name"
    pushd "$d" >/dev/null
    cargo build --release --target "$TARGET" || true
    # locate wasm
    wasm_path="$(find target -path "*/release/*.wasm" -print -quit 2>/dev/null || true)"
    if [ -n "$wasm_path" ]; then
      echo "[optimize] found wasm: $wasm_path"
      if command -v wasm-opt >/dev/null 2>&1; then
        out="$(dirname "$wasm_path")/$(basename "$wasm_path" .wasm).opt.wasm"
        echo "[optimize] running wasm-opt -O3 --strip-dwarf"
        wasm-opt -O3 --strip-dwarf -o "$out" "$wasm_path" || true
        echo "[optimize] optimized wasm written to $out"
      else
        echo "[optimize] wasm-opt not found; skipping binary-level optimization"
      fi
    else
      echo "[optimize] no wasm found for $name (skipping wasm-opt)"
    fi
    popd >/dev/null
  fi
done

echo "[optimize] Done. Run scripts/size_report.sh to view sizes."
