#!/usr/bin/env bash
# 行为对拍矩阵：对每个 Node 版本，把 fixture 编译成 jsc → jscd 反编译 → 运行结果与原函数逐用例比对
set -uo pipefail
cd "$(dirname "$0")/.."
VERSIONS=(${*:-16.20.2 18.20.8 20.20.2 22.12.0 24.12.0})
# fixture 列表直接取自 cases.json —— 新增用例自动纳入矩阵
FIXTURES=($(python3 -c "import json;print(' '.join(json.load(open('tests/fixtures/behav/cases.json')).keys()))"))
pass=0; partial=0; fail=0; other=0
for v in "${VERSIONS[@]}"; do
  map="workspace/ro/ro-map-$v.json"
  maparg=(); [ -f "$map" ] && maparg=(--ro-map "$map")
  for f in "${FIXTURES[@]}"; do
    line=$(node scripts/behav_diff.js "$f" "$v" "${maparg[@]}" 2>/dev/null | tail -1)
    st=$(printf '%s' "$line" | python3 -c "import sys,json;print(json.load(sys.stdin).get('status','?'))" 2>/dev/null || echo "?")
    case "$st" in
      pass) pass=$((pass+1));;
      partial) partial=$((partial+1));;
      fail) fail=$((fail+1));;
      *) other=$((other+1));;
    esac
    printf "%-10s %-12s %s\n" "$v" "$f" "$st"
  done
done
echo "---- 汇总: pass=$pass partial=$partial fail=$fail other=$other ----"
