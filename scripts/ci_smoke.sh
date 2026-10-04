#!/usr/bin/env bash
# CI 端到端冒烟：只用 PATH 里的 node（不装任何 npm 包）+ 已构建的 jscd，把仓库里
# 的 fixture 走一遍真实链路 —— 编译成 .jsc → jscd 反编译（--runtime）→
# `node --check` 语法门禁；"整脚本"型 fixture 再把原脚本与产物各跑一遍，
# 比对 stdout 与退出码。
#
# 用法：
#   cargo build --release && scripts/ci_smoke.sh
#   JSCD=/path/to/jscd scripts/ci_smoke.sh     # 用别的二进制
#
# 这是 tag 发版门禁的一部分（.github/workflows/release.yml 的 checks job）。
set -euo pipefail

cd "$(dirname "$0")/.."

jscd="${JSCD:-target/release/jscd}"
if [ ! -x "$jscd" ]; then
  echo "找不到 jscd：${jscd}（先 cargo build --release）" >&2
  exit 2
fi
if ! command -v node >/dev/null 2>&1; then
  echo "PATH 里没有 node" >&2
  exit 2
fi

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

echo "== ci_smoke: $("$jscd" version | head -1) / node $(node -v) / $(uname -sm)"

# 语法门禁：CJS 解析不过就按 ESM（.mjs）再解析一次 —— 覆盖含顶层 await/import 的产物。
syntax_gate() {
  if node --check "$1" >/dev/null 2>&1; then
    return 0
  fi
  cp "$1" "$1.mjs"
  node --check "$1.mjs"
}

# 封装：命令成功则把输出收进日志（失败才回显尾巴），让 CI 日志保持一行一个阶段。
run_quiet() { # 用法: run_quiet <日志文件> <命令...>
  local log="$1"
  shift
  if ! "$@" >>"$log" 2>&1; then
    echo "✗ 失败：$*" >&2
    tail -15 "$log" >&2
    return 1
  fi
}

# ① 行为 fixture：编译 → 反编译 → 语法门禁（用例级对拍由 scripts/verify_behavior.sh
#    跑版本矩阵；这里保证"真机 .jsc 一路反编译出来仍是合法 JS"）。
n=0
for f in tests/fixtures/behav/*.js; do
  b="$(basename "$f" .js)"
  run_quiet "$tmp/mkcorpus.log" node scripts/mkcorpus.js "$f" "$tmp/$b.jsc" || exit 1
  run_quiet "$tmp/jscd.log" "$jscd" decompile "$tmp/$b.jsc" --runtime -o "$tmp/$b.out.js" || {
    echo "反编译失败：$b" >&2
    exit 1
  }
  syntax_gate "$tmp/$b.out.js"
  run_quiet "$tmp/info.log" "$jscd" info "$tmp/$b.jsc" || exit 1
  n=$((n + 1))
done
echo "behav fixture：$n 个 —— 编译/反编译/语法门禁 全通过"

# ② 整脚本 fixture：再来一次 原脚本 vs 产物 的行为对拍。
#    原脚本在当前 node 上就跑不起来的（版本太老不支持某个语法）→ 跳过并说明。
n=0
skipped=0
for f in tests/fixtures/scripts/*.js; do
  b="$(basename "$f" .js)"
  run_quiet "$tmp/mkcorpus.log" node scripts/mkcorpus.js "$f" "$tmp/s_$b.jsc" || exit 1
  run_quiet "$tmp/jscd.log" "$jscd" decompile "$tmp/s_$b.jsc" --runtime -o "$tmp/s_$b.out.js" || {
    echo "反编译失败：$b" >&2
    exit 1
  }
  syntax_gate "$tmp/s_$b.out.js"
  set +e
  o1="$(node "$f" 2>&1)"
  c1=$?
  o2="$(node "$tmp/s_$b.out.js" 2>&1)"
  c2=$?
  set -e
  if [ "$c1" != 0 ]; then
    echo "  skip ${b}：原脚本在本 node（$(node -v)）上就跑不起来"
    skipped=$((skipped + 1))
    continue
  fi
  if [ "$c1" != "$c2" ] || [ "$o1" != "$o2" ]; then
    echo "✗ ${b}：行为不一致（退出码 $c1 vs ${c2}）" >&2
    diff <(printf '%s\n' "$o1") <(printf '%s\n' "$o2") | head -20 >&2 || true
    exit 1
  fi
  n=$((n + 1))
done
echo "script fixture：$n 个 —— 原脚本 vs 反编译产物 行为逐字一致（skip ${skipped}）"

echo "== ci_smoke 全通过"
