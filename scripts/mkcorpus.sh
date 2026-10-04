#!/usr/bin/env bash
# 生成跨 Node 版本语料：workspace/corpus/<node>/<fixture>[-module|-brotli].jsc
# 依赖 mise（mise.toml 已钉住版本）。用法: scripts/mkcorpus.sh [node-version...]
set -euo pipefail
cd "$(dirname "$0")/.."

VERSIONS=("$@")
if [ ${#VERSIONS[@]} -eq 0 ]; then
  VERSIONS=(16.20.2 18.20.8 20.20.2 22.12.0 24.12.0)
fi

FIXTURES_DIR=tests/fixtures
OUT_ROOT=workspace/corpus
mkdir -p "$OUT_ROOT"

for v in "${VERSIONS[@]}"; do
  out_dir="$OUT_ROOT/node$v"
  mkdir -p "$out_dir"
  for js in "$FIXTURES_DIR"/*.js; do
    base=$(basename "$js" .js)
    mise exec "node@$v" -- node scripts/mkcorpus.js "$js" "$out_dir/$base.jsc"
    mise exec "node@$v" -- node scripts/mkcorpus.js "$js" "$out_dir/$base-module.jsc" --module
    if [ "$base" = "features" ]; then
      mise exec "node@$v" -- node scripts/mkcorpus.js "$js" "$out_dir/$base-brotli.jsc" --brotli
    fi
  done
done
echo "corpus ready under $OUT_ROOT"