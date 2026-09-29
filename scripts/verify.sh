#!/usr/bin/env bash
# 一键验证：生成语料 → 与 node --print-bytecode 逐指令对拍（全部已安装版本）
set -euo pipefail
cd "$(dirname "$0")/.."

VERSIONS=(16.20.2 18.20.8 20.20.2 22.12.0 24.12.0)
echo "== 生成语料 =="
bash scripts/mkcorpus.sh "${VERSIONS[@]}" >/dev/null
echo "== 黄金对拍（module 形态，与 node 的 CommonJS 包装一致）=="
fail=0
for v in "${VERSIONS[@]}"; do
  python3 scripts/golden_diff.py tests/fixtures/features.js "workspace/corpus/node$v/features-module.jsc" "$v" | head -3 || fail=1
done
echo "== 单元/集成测试 =="
cargo test --quiet
exit $fail
