#!/usr/bin/env bash
# 行为对拍矩阵：对每个 Node 版本，把 fixture 编译成 jsc → jscd 反编译 → 运行结果与原函数逐用例比对
set -uo pipefail
cd "$(dirname "$0")/.."
VERSIONS=(${*:-16.20.2 18.20.8 20.20.2 22.12.0 24.12.0})
# fixture 列表直接取自 cases.json —— 新增用例自动纳入矩阵
FIXTURES=($(python3 -c "import json;print(' '.join(json.load(open('tests/fixtures/behav/cases.json')).keys()))"))
# 整脚本用例（比 stdout/退出码；覆盖 `target` 型用例照不到的顶层形态）
SCRIPTS=($(cd tests/fixtures/scripts 2>/dev/null && ls *.js 2>/dev/null | sed 's/\.js$//'))
pass=0; partial=0; fail=0; other=0; cfail=0; skip=0; known=0; spass=0; sfail=0
for v in "${VERSIONS[@]}"; do
  map="workspace/ro/ro-map-$v.json"
  # 注意 `set -u` 下空数组展开会报 unbound —— 没生成 ro-map 的版本（老族）会踩到
  maparg=(); [ -f "$map" ] && maparg=(--ro-map "$map")
  for f in "${FIXTURES[@]}"; do
    line=$(node scripts/behav_diff.js "$f" "$v" ${maparg[@]+"${maparg[@]}"} 2>/dev/null | tail -1)
    st=$(printf '%s' "$line" | python3 -c "import sys,json;print(json.load(sys.stdin).get('status','?'))" 2>/dev/null || echo "?")
    case "$st" in
      pass) pass=$((pass+1));;
      compile-fail) cfail=$((cfail+1));;
      skip) skip=$((skip+1));;    # fixture 语法要更高版本（cases.json 的 min_node）→ 不算失败
      partial) partial=$((partial+1));;
      known-fail) known=$((known+1));;
      fail) fail=$((fail+1));;
      *) other=$((other+1));;
    esac
    printf "%-10s %-12s %s\n" "$v" "$f" "$st"
  done
  for f in ${SCRIPTS[@]+"${SCRIPTS[@]}"}; do
    line=$(node scripts/script_diff.js "$f" "$v" ${maparg[@]+"${maparg[@]}"} 2>/dev/null | tail -1)
    st=$(printf '%s' "$line" | python3 -c "import sys,json;print(json.load(sys.stdin).get('status','?'))" 2>/dev/null || echo "?")
    case "$st" in
      pass) spass=$((spass+1));;
      compile-fail) cfail=$((cfail+1));;
      *) sfail=$((sfail+1));;
    esac
    printf "%-10s %-12s %s\n" "$v" "script:$f" "$st"
  done
done
echo "---- 汇总: pass=$pass partial=$partial fail=$fail other=$other compile-fail=$cfail skip=$skip known-fail=$known script-pass=$spass script-fail=$sfail ----"
