#!/usr/bin/env python3
"""把 `jscd --help` 与每个子命令的 help 原样同步进双语 README（折叠在 <details> 里）。

- 用 `<!-- BEGIN help:<key> -->` / `<!-- END help:<key> -->` 标记定位，重复运行幂等；
- 标记不在时，按锚点把整段插进去（首次用）；
- help 文本由**当前二进制**生成（`JSCD_LANG` 控制语言），所以发版后跑一次即可同步版本行。

用法：
    cargo build --release && python3 scripts/update_readme_help.py [--jscd PATH] [--check]

`--check` 只报告差异、不改文件（CI 或发版前用）。
"""
import argparse
import os
import re
import subprocess
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
DEFAULT_BIN = os.path.join(ROOT, "target", "release", "jscd")

# key -> (命令参数, 英文 summary, 中文 summary)
BLOCKS = [
    ("main", ["--help"], "jscd --help", "jscd --help"),
    ("decompile", ["help", "decompile"], "jscd help decompile", "jscd help decompile"),
    ("info", ["help", "info"], "jscd help info", "jscd help info"),
    ("strings", ["help", "strings"], "jscd help strings", "jscd help strings"),
    ("functions", ["help", "functions"], "jscd help functions", "jscd help functions"),
    ("disasm", ["help", "disasm"], "jscd help disasm", "jscd help disasm"),
    ("ro-map", ["help", "ro-map"], "jscd help ro-map", "jscd help ro-map"),
    ("help", ["help", "help"], "jscd help help", "jscd help help"),
    ("version", ["help", "version"], "jscd help version", "jscd help version"),
]

FILES = [("README.md", "en"), ("README.zh.md", "zh")]

SECTION_INTRO = {
    "en": "Everything the CLI prints for `--help` and `help <SUBCOMMAND>`, verbatim "
          "(regenerate with `python3 scripts/update_readme_help.py`).",
    "zh": "`--help` 与 `help <子命令>` 的输出原样收在这里"
          "（刷新用 `python3 scripts/update_readme_help.py`）。",
}

# 首次插入的位置：紧跟通配锚点（Usage 一节末尾的 flags 表之后）
ANCHOR = {
    "en": "| `-v, -V, --version` | all forms | print name, version and repository |\n",
    "zh": "| `-v, -V, --version` | 全部形态 | 打印名字、版本与仓库地址 |\n",
}
SECTION_TITLE = {"en": "## CLI reference\n", "zh": "## CLI 参考\n"}


def run_help(binary, lang, args):
    env = dict(os.environ, JSCD_LANG=lang)
    out = subprocess.run([binary, *args], env=env, capture_output=True, text=True, check=True)
    return out.stdout.rstrip("\n")


def render_all(binary, lang):
    """整段 CLI 帮助折进**一个** <details>：主帮助 + 每个子命令，中间用粗体命令行分隔。"""
    parts = []
    for key, args, en_sum, zh_sum in BLOCKS:
        summary = en_sum if lang == "en" else zh_sum
        parts.append(f"**`{summary}`**\n\n```console\n$ {summary}\n{run_help(binary, lang, args)}\n```")
    body = "\n\n".join(parts)
    if lang == "en":
        summary = "Full CLI help — <code>jscd --help</code> and every subcommand, verbatim"
    else:
        summary = "完整 CLI 帮助 —— <code>jscd --help</code> 与每个子命令，原样输出"
    return (
        "<!-- BEGIN help:cli -->\n"
        f"<details>\n<summary>{summary}</summary>\n\n"
        f"{body}\n\n"
        "</details>\n"
        "<!-- END help:cli -->\n"
    )


def sync(path, lang, binary, check):
    text = open(path, encoding="utf-8").read()
    block = render_all(binary, lang)
    begin, end = "<!-- BEGIN help:cli -->", "<!-- END help:cli -->"
    pat = re.compile(re.escape(begin) + r".*?" + re.escape(end) + r"\n?", re.S)
    if pat.search(text):
        new = pat.sub(lambda _m: block, text, count=1)
    else:
        section = SECTION_TITLE[lang] + "\n" + SECTION_INTRO[lang] + "\n\n" + block + "\n"
        anchor = ANCHOR[lang]
        assert anchor in text, f"{path}: 找不到锚点，请手动放置 {begin} 标记"
        new = text.replace(anchor, anchor + "\n" + section, 1)
    if new != text:
        if check:
            print(f"{path}: 需要刷新")
        else:
            open(path, "w", encoding="utf-8").write(new)
            print(f"{path}: 已刷新 CLI 帮助块")
        return True
    print(f"{path}: 已是最新")
    return False


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--jscd", default=DEFAULT_BIN)
    ap.add_argument("--check", action="store_true")
    a = ap.parse_args()
    if not os.path.exists(a.jscd):
        sys.exit(f"找不到二进制：{a.jscd}（先 cargo build --release）")
    dirty = False
    for name, lang in FILES:
        dirty |= sync(os.path.join(ROOT, name), lang, a.jscd, a.check)
    if a.check and dirty:
        sys.exit(1)


if __name__ == "__main__":
    main()