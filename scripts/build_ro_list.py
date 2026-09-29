#!/usr/bin/env python3
"""生成 ro-map 的候选名清单：从语料里抓"属性名 + 字符串字面量"，再补标点/短串。

为什么需要它：`.jsc` 里对只读堆字符串有时给的是 `kReadOnlyHeapRef`（chunk/offset 地址），
地址→名字没有别的来源 —— 只能拿"探针 jsc"去解。探针里 `o["<字面量>"]` 这种**计算属性**
形态才会把字面量编码成只读堆引用（实测 `o["/"]` → `0/18016: "/"`，而把它当 join 参数
就不产生），所以候选清单必须包含"非标识符"的串（标点、数字串等），否则像 `join("/")`
这种就会在反编译结果里留下 `<ro0_18016>` 占位。

用法：
  python3 scripts/build_ro_list.py --out workspace/ro/idents-final.txt \
      work/samples work/samples-all workspace/ro/chars.txt
"""
import argparse
import re
import sys

# 属性名：.name / ["name"] / .name 里的名字
PROP = re.compile(r"\.\s*([A-Za-z_$][A-Za-z0-9_$]*)")
# 裸标识符：全局名/函数名等也会以只读堆引用出现（`var o = JSON;` 里的 JSON
# 在 .jsc 里就是 `ro0/61040`，只收属性名 + 字面量会漏掉它们）
WORD = re.compile(r"[A-Za-z_$][A-Za-z0-9_$]*")
# 字符串字面量（单/双引号，粗略支持转义）
LIT = re.compile(r"""(?<!\\)(['"])((?:\\.|(?!\1).){0,24})\1""")
ESCAPE = {"\\n": "\n", "\\t": "\t", "\\r": "\r", "\\\\": "\\", "\\0": "\0",
          "\\'": "'", '\\"': '"'}


def unescape(s: str) -> str:
    for k, v in ESCAPE.items():
        s = s.replace(k, v)
    return s


def add(seen: set, out: list, s: str) -> None:
    if not s or len(s) > 32 or s in seen:
        return
    if any(ord(c) < 0x20 for c in s):
        return
    seen.add(s)
    out.append(s)


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--out", required=True)
    ap.add_argument("--no-punct", action="store_true",
                    help="不补标点/数字等短串（只要语料里出现的）")
    ap.add_argument("dirs", nargs="+")
    args = ap.parse_args()

    seen: set = set()
    out: list = []

    # 1) 标点/单字符：单字符全 ASCII 可打印 + 常见两字符组合
    if not args.no_punct:
        for c in range(0x20, 0x7F):
            add(seen, out, chr(c))
        punct = "!\"#$%&'()*+,-./:;<=>?@[\\]^_`{|}~"
        for a in punct:
            for b in punct:
                add(seen, out, a + b)
        for a in punct:
            for b in punct:
                for c in punct:
                    if a == b == c:  # 重复字符的三连（如 "..."）单独补
                        continue
                    add(seen, out, a + b + c)
        for s in ("...", "===", "!==", "=>", "<!--", "-->", "//", "/*", "*/",
                  "&&", "||", "??", "?.", "**", "++", "--", "0", "00"):
            add(seen, out, s)
        for n in range(0, 1000):
            add(seen, out, str(n))
        for n in ("-1", "0x", "1e", "Infinity", "NaN", "true", "false", "null",
                  "undefined", "none", "TRUE", "FALSE", "NULL"):
            add(seen, out, n)

    # 2) 语料：属性名 + 字符串字面量
    import os
    files = 0
    for d in args.dirs:
        if os.path.isfile(d):
            paths = [d]
        else:
            paths = []
            for root, _dirs, names in os.walk(d):
                for n in names:
                    if n.endswith(".js"):
                        paths.append(os.path.join(root, n))
        for p in paths:
            try:
                text = open(p, encoding="utf-8", errors="ignore").read()
            except OSError:
                continue
            files += 1
            for m in PROP.finditer(text):
                add(seen, out, m.group(1))
            for m in LIT.finditer(text):
                add(seen, out, unescape(m.group(2)))
            for m in WORD.finditer(text):
                add(seen, out, m.group(0))

    with open(args.out, "w", encoding="utf-8") as f:
        for s in out:
            f.write(s + "\n")
    print(f"{len(out)} 条候选（扫了 {files} 个文件）→ {args.out}", file=sys.stderr)


if __name__ == "__main__":
    main()