#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

echo "Contract WASM size report"
echo "-------------------------"

find "$ROOT_DIR"/contracts -type f -name '*.wasm' | while read -r f; do
  size=$(stat -c%s "$f" 2>/dev/null || true)
  human=$(numfmt --to=iec-i --suffix=B --format="%.1f" "$size" 2>/dev/null || echo "${size}B")
  echo "$(realpath --relative-to="$ROOT_DIR" "$f"): $human ($size bytes)"
done

echo "-------------------------"
