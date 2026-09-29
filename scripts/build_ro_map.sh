#!/usr/bin/env bash
# 生成"只读堆引用名表"：把候选属性名编译成探针 jsc，再从中提取 (chunk/offset → 名称)。
# 用法: scripts/build_ro_map.sh <node-version> <ident-list.txt> <out.json>
#   ident-list.txt：每行一个属性名（合法标识符）。可从 JS 语料里 grep 出标识符。
set -euo pipefail
cd "$(dirname "$0")/.."
VER=${1:?node version}
LIST=${2:?ident list}
OUT=${3:?output json}

PROBE=workspace/ro/probe-$VER.js
JSC=workspace/ro/probe-$VER.jsc
mkdir -p workspace/ro

# 探针：每个属性名一个函数，函数名带 p_ 前缀便于提取
python3 - "$LIST" "$PROBE" <<'PY'
import sys, json
lst, out = sys.argv[1], sys.argv[2]
names = []
seen = set()
ESCAPES = {'\\n': '\n', '\\t': '\t', '\\r': '\r', '\\\\': '\\', '\\0': '\0', "\\'": "'", '\\"': '"'}
for line in open(lst, encoding='utf-8', errors='ignore'):
    n = line.rstrip('\n')
    n = ESCAPES.get(n, n)  # 支持 \n \t \\ 等转义写法
    if not n or n in seen:
        continue
    seen.add(n)
    names.append(n)
# 名字编码约定：
#   合法标识符 → 函数名 p_<name>，点访问 o.<name>
#   其它（单字符/标点/转义）→ 函数名 p_hex_<hex(utf8)>，中括号访问 o["<name>"]
def fn_name(n):
    if n.isidentifier():
        return f"p_{n}", f"o.{n}"
    return f"p_hex_{n.encode('utf-8').hex()}", "o[{}]".format(json.dumps(n))

with open(out, 'w', encoding='utf-8') as f:
    f.write("// 探针：为每个候选属性名生成一个函数并调用（--no-lazy 下确保被编译）\n")
    pairs = [fn_name(n) for n in names]
    for fname, access in pairs:
        f.write(f"function {fname}(o) {{ return {access}; }}\n")
    f.write("function __runAll(o) {\n")
    for fname, _ in pairs:
        f.write(f"  {fname}(o);\n")
    f.write("}\n__runAll({});\n")
print(f"{len(names)} probes")
PY

mise exec "node@$VER" -- node scripts/mkcorpus.js "$PROBE" "$JSC" --module >/dev/null
./target/release/jscd ro-map "$JSC" -o "$OUT"
python3 -c "
import json,sys
m=json.load(open('$OUT'))
print(f'ro-map: {len(m[\"entries\"])} 条 → $OUT')
"
