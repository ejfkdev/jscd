#!/usr/bin/env bash
# 真实代码回归：把 node 自带的大文件编成 jsc，与官方反汇编逐指令对拍。
# 注意：每个版本用独立的源文件（不同 tag 的源码不同，混用会导致假差异）。
set -euo pipefail
cd "$(dirname "$0")/.."
VERSIONS=(16.20.2 18.20.8 20.20.2 22.12.0 24.12.0)
FILES=(lib/internal/util/inspect.js lib/net.js lib/internal/util/types.js)
NODE_SRC=${NODE_SRC:-/Users/e/Documents/github/node}
mkdir -p workspace/real
fail=0
for v in "${VERSIONS[@]}"; do
  for f in "${FILES[@]}"; do
    base=$(basename "$f" .js)
    src="workspace/real/$base-$v.js"
    jsc="workspace/real/$base-$v.jsc"
    git -C "$NODE_SRC" show "v$v:$f" > "$src" 2>/dev/null || continue
    mise exec "node@$v" -- node scripts/mkcorpus.js "$src" "$jsc" --module >/dev/null
    out=$(python3 scripts/golden_diff.py "$src" "$jsc" "$v" 2>&1 || true)
    echo "$out" | sed -n '2p' | sed "s/^/[node$v $base] /"
    echo "$out" | grep -q "mismatched: 0" || { fail=1; echo "$out" | sed -n '4,12p'; }
  done
done
exit $fail
