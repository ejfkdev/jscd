#!/usr/bin/env python3
"""按需生成 ro-map：**每个名字单独编译一份小 jsc**，从占位符里读出 (chunk, offset)。

为什么不用 `build_ro_map.sh`（一个探针装下所有名字）：老族对**大 payload** 的解析还会
走偏（实测 1.2MB 探针报 `unknown serialization tag 0x98`），而 fixture 规模的小 payload
完全正常。逐个编译绕开了这个问题，也顺便给出"某个名字到底落在哪个只读堆地址"的直接证据。

做法：对每个候选名字生成 `function p(o){ return o.NAME; }`（非标识符用 `o["NAME"]`），
用指定 node 编成 .jsc，再用 jscd 反编译（**不带 --ro-map**）→ 未解析的只读堆引用会渲染成
`<ro{chunk}_{offset}>` → 由此得到映射。

用法: python3 scripts/build_ro_map_small.py <node版本> <名字清单> <输出json>
"""
import json
import os
import re
import subprocess
import sys
import tempfile

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
PLACEHOLDER = re.compile(r"<(?:ro|RO)(\d+)_(\d+)>")


def fn_src(name: str) -> str:
    if name.isidentifier():
        return f"function p(o) {{ return o.{name}; }}\n"
    return f"function p(o) {{ return o[{json.dumps(name)}]; }}\n"


def main() -> None:
    ver, list_path, out_path = sys.argv[1], sys.argv[2], sys.argv[3]
    names, seen = [], set()
    for line in open(list_path, encoding="utf-8", errors="ignore"):
        n = line.rstrip("\n")
        if n and n not in seen:
            seen.add(n)
            names.append(n)

    node = os.path.expanduser(f"~/.local/share/mise/installs/node/{ver}/bin/node")
    entries = {}
    fails = []
    with tempfile.TemporaryDirectory() as td:
        for i, name in enumerate(names):
            if name in ("", "\n"):
                continue
            src = os.path.join(td, "p.js")
            jsc = os.path.join(td, "p.jsc")
            open(src, "w", encoding="utf-8").write(fn_src(name))
            r = subprocess.run(
                [node, os.path.join(ROOT, "scripts", "mkcorpus.js"), src, jsc],
                capture_output=True, text=True, cwd=ROOT, timeout=120,
            )
            if r.returncode != 0:
                continue
            d = subprocess.run(
                [os.path.join(ROOT, "target", "release", "jscd"), "decompile", jsc],
                capture_output=True, text=True, cwd=ROOT, timeout=120,
            )
            if d.returncode != 0:
                fails.append((name, (d.stderr or "").strip()[:60]))
                continue
            m = PLACEHOLDER.search(d.stdout)
            if m:
                entries[f"{int(m.group(1))}/{int(m.group(2))}"] = name
            if (i + 1) % 200 == 0:
                print(f"  …{i + 1}/{len(names)}（已解出 {len(entries)}）", file=sys.stderr)

    out = {
        "schema": "jscd.ro-map/v1",
        "v8": ver,
        "note": "由 scripts/build_ro_map_small.py 逐名编译生成",
        "entries": entries,
    }
    with open(out_path, "w", encoding="utf-8") as f:
        json.dump(out, f, ensure_ascii=False, indent=1, sort_keys=True)
    print(f"ro-map: {len(entries)} 条（{len(names)} 个候选，{len(fails)} 份解析失败）→ {out_path}")


if __name__ == "__main__":
    main()