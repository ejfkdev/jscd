#!/usr/bin/env python3
"""jscd 版本表生成器：从 Node 源码树按 tag 提取 V8 解析所需数据 → tables/*.json。

数据源：一份 Node 源码树（浅克隆即可，需先
  git fetch --depth 1 origin tag v16.20.2 ...）
产出：
  tables/manifest.json     版本索引（90 个 Node↔V8 组合的 version_hash）+ 表别名（去重）
  tables/v<X>_<Y>.json     单个 V8 major.minor 的解析表（内容相同的版本别名复用同一文件）
  src/tables_embed.rs      Rust 内嵌清单（include_str!）

用法:
  python3 scripts/codegen.py --node /path/to/node \
      --tag v16.20.2 [--tag v18.20.8 ...]
"""
import argparse
import concurrent.futures
import glob
import hashlib
import json
import os
import re
import subprocess
import sys
import urllib.error
import urllib.request
from datetime import datetime, timezone

NODE = None          # --node：本地 node 克隆（可选，离线用）
CACHE_DIR = None     # --cache：按 tag 缓存单个源文件（默认 workspace/node-src）
OFFLINE = False      # --offline：只许用缓存/克隆，绝不联网
REPO = "nodejs/node"


def _http_text(url):
    """取一个文件；404 返回 None，其它错误抛出（绝不能把网络故障当成"这版没这文件"）。"""
    req = urllib.request.Request(url, headers={"User-Agent": "jscd-codegen"})
    with urllib.request.urlopen(req, timeout=45) as r:
        return r.read().decode("utf-8", "replace")


def _cache_path(tag, path):
    if not CACHE_DIR:
        return None
    return os.path.join(CACHE_DIR, tag, path)


def repo_file(tag, path, in_v8=True):
    """按需取**单个**文件（不再拉整份 node 源码）。

    顺序：本地缓存 → `--node` 克隆 → raw.githubusercontent（`deps/v8/` 下）。
    404 会在缓存里落一个 `.404` 哨兵，避免重复请求；传输错误直接抛。
    """
    rel = f"deps/v8/{path}" if in_v8 else path
    cache = _cache_path(tag, path)
    if cache and os.path.exists(cache):
        with open(cache, encoding="utf-8") as f:
            return f.read()
    if cache and os.path.exists(cache + ".404"):
        return None
    git_answered = False
    for git_dir in (NODE, REPO_DIR):
        if not git_dir or not os.path.exists(os.path.join(git_dir, ".git")):
            continue
        r = subprocess.run(["git", "-C", git_dir, "cat-file", "-e", f"{tag}^{{commit}}"],
                           capture_output=True, text=True)
        if r.returncode != 0:
            continue          # 本地没有这个 tag，交给下一个源
        git_answered = True
        r = subprocess.run(["git", "-C", git_dir, "show", f"{tag}:{rel}"],
                           capture_output=True, text=True)
        if r.returncode == 0:
            if cache:
                os.makedirs(os.path.dirname(cache), exist_ok=True)
                with open(cache, "w", encoding="utf-8") as f:
                    f.write(r.stdout)
            return r.stdout
    if git_answered:
        # 本地 git 已经能看到这个 tag 了：查不到就是这版没这个文件，
        # 别再打网络（raw.githubusercontent 在有些网络里是黑洞）。
        if cache:
            os.makedirs(os.path.dirname(cache), exist_ok=True)
            open(cache + ".404", "w").close()
        return None
    if OFFLINE:
        return None
    url = f"https://raw.githubusercontent.com/{REPO}/{tag}/{rel}"
    try:
        text = _http_text(url)
    except urllib.error.HTTPError as e:
        if e.code == 404:
            if cache:
                os.makedirs(os.path.dirname(cache), exist_ok=True)
                open(cache + ".404", "w").close()
            return None
        raise
    if cache:
        os.makedirs(os.path.dirname(cache), exist_ok=True)
        with open(cache, "w", encoding="utf-8") as f:
            f.write(text)
    return text


# 一个 tag 需要的全部 V8 源文件（含按版本改名/移动的候选）
NEEDED_V8_FILES = [
    "include/v8-version.h",
    "src/base/functional.h",
    "src/base/hashing.h",
    "src/snapshot/code-serializer.h",
    "src/interpreter/bytecodes.h",
    "src/interpreter/bytecode-operands.h",
    "src/interpreter/interpreter-intrinsics.h",
    "src/snapshot/serializer-deserializer.h",
    "src/snapshot/serializer-common.h",
    "src/snapshot/serializer.h",
    "src/roots/roots.h",
    "src/roots.h",
    "src/init/heap-symbols.h",
    "src/heap-symbols.h",
    "src/objects/objects-definitions.h",
    "src/objects-definitions.h",
    "src/runtime/runtime.h",
    "src/objects/bytecode-array.tq",
    "src/objects/code.tq",
    "src/objects/trusted-object.tq",
    "src/objects/fixed-array.tq",
    "src/execution/frame-constants.h",
    "src/frame-constants.h",
    "src/objects/shared-function-info.tq",
    "src/objects/scope-info.tq",
    "src/objects/bytecode-array-inl.h",
    "src/common/globals.h",
]


REPO_DIR = None      # --repo-dir：blob 过滤的部分克隆（只按需取文件，不拉全量）
REPO_URL = "https://github.com/nodejs/node"


def _git(args, **kw):
    return subprocess.run(["git", "-C", REPO_DIR, *args], capture_output=True, text=True, **kw)


def ensure_repo():
    """确保有一个 **blob 过滤**的部分克隆：--depth 1 --filter=blob:none --no-checkout。

    这样只有 commit 与目录树会随 fetch 下来（几 MB），具体文件是 `git cat-file`
    按需单独取的 —— 不需要整份 node 源码。
    """
    global REPO_DIR
    if REPO_DIR and os.path.exists(os.path.join(REPO_DIR, ".git")):
        return
    if OFFLINE:
        return
    d = REPO_DIR or os.path.join(os.path.dirname(__file__), "..", "workspace", "node-partial")
    if not os.path.exists(os.path.join(d, ".git")):
        os.makedirs(os.path.dirname(d), exist_ok=True)
        subprocess.run(["git", "clone", "--filter=blob:none", "--no-checkout", "--depth", "1",
                        "-q", REPO_URL, d], check=True)
        print(f"partial clone -> {d}", file=sys.stderr)
    REPO_DIR = d


def have_tag(tag):
    r = _git(["rev-parse", "--verify", "--quiet", f"refs/tags/{tag}"])
    return r.returncode == 0


def prefetch(tag, jobs=16):
    """把这个 tag 需要的**那些文件**取进缓存（其余源码一概不要）。

    实现：先 fetch 该 tag 的 commit+树（无 blob），再一次 `git cat-file --batch`
    让它把所有缺失的 blob 合并成一轮网络往返，逐个写进文件缓存。
    """
    wanted = [(p, True) for p in NEEDED_V8_FILES] + [("node.gypi", False), ("common.gypi", False)]
    missing = []
    for path, in_v8 in wanted:
        rel = f"deps/v8/{path}" if in_v8 else path
        cache = _cache_path(tag, path)
        if cache and (os.path.exists(cache) or os.path.exists(cache + ".404")):
            continue
        missing.append((path, rel))
    ensure_repo()
    if REPO_DIR and os.path.exists(os.path.join(REPO_DIR, ".git")):
        if not have_tag(tag):
            r = _git(["fetch", "--depth", "1", "--filter=blob:none", "-q", "origin",
                      f"refs/tags/{tag}:refs/tags/{tag}"])
            if r.returncode != 0:
                print(f"warn: fetch {tag} failed: {r.stderr.strip()[:120]}", file=sys.stderr)
        if have_tag(tag) and missing:
            _batch_fetch(tag, missing)
    # 还没拿到的（克隆不可用/文件不存在）逐个兜底：缓存 → git → raw HTTP
    for path, in_v8 in wanted:
        repo_file(tag, path, in_v8)


def _batch_fetch(tag, missing):
    """一次 cat-file --batch 取多文件：missing blob 会被合并成一轮 fetch。

    输出格式：每个请求回 `<sha> <type> <size>\n<payload>\n`，对象不存在则回
    `<spec> missing\n`。这里按顺序一一对应地切出来写缓存。
    """
    req = ("\n".join(f"{tag}:{rel}" for _, rel in missing) + "\n").encode()
    r = subprocess.run(["git", "-C", REPO_DIR, "cat-file", "--batch"],
                       input=req, capture_output=True)
    if r.returncode != 0:
        return  # 交给逐个兜底
    buf = r.stdout
    i = 0
    for path, _rel in missing:
        nl = buf.find(b"\n", i)
        if nl < 0:
            break
        head = buf[i:nl].decode("utf-8", "replace").split()
        i = nl + 1
        if len(head) == 2 and head[1] == "missing":
            cache = _cache_path(tag, path)
            if cache:
                os.makedirs(os.path.dirname(cache), exist_ok=True)
                open(cache + ".404", "w").close()
            continue
        if len(head) != 3:
            break
        size = int(head[2])
        payload = buf[i:i + size]
        i += size + 1  # 结尾还有一个换行
        cache = _cache_path(tag, path)
        if cache:
            os.makedirs(os.path.dirname(cache), exist_ok=True)
            with open(cache, "w", encoding="utf-8") as f:
                f.write(payload.decode("utf-8", "replace"))


def git_show(tag, path):
    """历史入口：等价于取 deps/v8/<path>。"""
    return repo_file(tag, path, in_v8=True)


def parse_v8_version(tag):
    text = git_show(tag, "include/v8-version.h")
    if text is None:
        return None
    vals = {}
    for key in ("V8_MAJOR_VERSION", "V8_MINOR_VERSION", "V8_BUILD_NUMBER", "V8_PATCH_LEVEL"):
        m = re.search(rf"#define {key} (\d+)", text)
        if not m:
            return None
        vals[key] = int(m.group(1))
    return (vals["V8_MAJOR_VERSION"], vals["V8_MINOR_VERSION"],
            vals["V8_BUILD_NUMBER"], vals["V8_PATCH_LEVEL"])


# ---------------------------------------------------------------- bytecodes.h

def extract_bytecodes(text):
    """从 bytecodes.h 提取 opcode 顺序表与每个 opcode 的操作数类型。

    BYTECODE_LIST 宏条目形如
      V(Name, ImplicitRegisterUse::kX, OperandType::kA, OperandType::kB, ...)
    枚举顺序 = 数值顺序。返回 [(name, [operand types], is_prefix)]。
    """
    # 找到 BYTECODE_LIST(V, ...) 宏定义体（可能带 V_TSA 变体，取第一个含 V( 的）
    m = re.search(r"#define BYTECODE_LIST(?:\w*)\(([^)]*)\)\s*\\(.*?)(?=\n#define |\n#endif|\Z)",
                  text, re.S)
    if not m:
        raise RuntimeError("BYTECODE_LIST not found")
    body = m.group(2)
    entries = []
    for line in body.splitlines():
        line = line.strip()
        lm = re.match(r"^V\(\s*(\w+)\s*,", line)
        if not lm:
            continue
        name = lm.group(1)
        operands = re.findall(r"OperandType::k(\w+)", line)
        is_prefix = name in ("Wide", "ExtraWide")
        entries.append({"name": name, "operands": operands, "prefix": is_prefix or None})
    return entries


def extract_operand_types(text):
    """提取 OperandType → {size, scalable}。

    两种版本风格：
    - V8 ≤ 11.x: V(Reg, OperandTypeInfo::kScalableSignedByte)
    - V8 ≥ 12:   V(Reg, OperandSize::kByte) [+ OperandScale::kQuadruple]
    """
    INFO = {
        "None": (0, False),
        "ScalableSignedByte": (1, True),
        "ScalableUnsignedByte": (1, True),
        "FixedSignedByte": (1, False),
        "FixedUnsignedByte": (1, False),
        "FixedUnsignedShort": (2, False),
        "FixedUnsignedQuad": (4, False),
        "FixedSignedShort": (2, False),
    }
    types = {}
    for m in re.finditer(r"V\(\s*(\w+)\s*,\s*OperandTypeInfo::k(\w+)\s*\)", text):
        name, info = m.group(1), m.group(2)
        if info in INFO:
            size, scalable = INFO[info]
            types[name] = {"size": size, "scalable": scalable}
    for m in re.finditer(r"V\(\s*(\w+)\s*,\s*OperandSize::k(\w+)\s*(?:,\s*OperandScale::k(\w+))?\s*\)", text):
        name, size, scale = m.group(1), m.group(2), m.group(3)
        size_bytes = {"None": 0, "Byte": 1, "Short": 2, "Quad": 4}.get(size)
        if size_bytes is None:
            continue
        scalable = scale in ("Double", "Quadruple", "Octuple")
        types[name] = {"size": size_bytes, "scalable": scalable}
    return types


# --------------------------------------------------- serializer-deserializer.h

def extract_legacy_tags(text):
    """旧族（V8 ≤ 8.x）的序列化 tag：不是枚举而是 static const int + 位掩码。

    形如 kNewObject/kBackref 定义在 snapshot/serializer.h，其余在 serializer-common.h。
    这一族的 tag 是"位打包"（Where / HowToCode / WhereToPoint），与现代扁平枚举不同族，
    解码逻辑需独立实现（见 docs/VERSIONS.md「旧族」一节）。
    """
    if text is None:
        return None
    sect = text[text.find("class SerializerDeserializer") :]
    consts = dict(
        (k, int(v, 0)) for k, v in re.findall(r"static const int (k\w+)\s*=\s*(0x[0-9a-fA-F]+|\d+)\s*;", sect)
    )
    # 6.x 把主标签放在 `enum Where { kNewObject = 0x00, … }`（还有 enum HowToCode/
    # WhereToPoint 的位掩码常量）。不抓这一块时表里只有那些 static const int 的
    # 次要常量 → 解析直接报 "table missing serialization tag kNewObject"。
    for m in re.finditer(r"enum (Where|HowToCode|WhereToPoint)\s*\{(.*?)\}", sect, re.S):
        for k, v in re.findall(r"(k\w+)\s*=\s*(0x[0-9a-fA-F]+|\d+)", m.group(2)):
            consts.setdefault(k, int(v, 0))
    return consts or None


def extract_serialization_tags(text):
    """提取 SerializationTag::Bytecode 枚举（显式值 + 隐式递增 + range 常量）。"""
    # 类型拼写随版本变：V8 ≤ 11.x 用 ": byte"，12.x+ 用 ": uint8_t"；
    # 更早的版本（Node 8/10/12）在 serializer-common.h 里且无底层类型
    m = re.search(r"enum Bytecode(?:\s*:\s*(?:byte|uint8_t|uint8))?\s*\{(.*?)\n  \};", text, re.S)
    if not m:
        raise RuntimeError("SerializationTag::Bytecode not found")
    body = m.group(1)
    tags = {}
    value = 0
    for raw_line in body.splitlines():
        line = raw_line.split("//")[0].strip()
        line = line.rstrip(",").rstrip("\\").strip()
        if not line or line.startswith("/*") or line.startswith("*"):
            continue
        dm = re.match(r"(\w+)\s*=\s*(0x[0-9a-fA-F]+|\d+)\s*$", line)
        if dm:
            tags[dm.group(1)] = int(dm.group(2), 0)
            value = tags[dm.group(1)] + 1
            continue
        if re.match(r"^\w+$", line):
            tags[line] = value
            value += 1
        # else: 非法行（预处理 continuation 等）跳过
    # range 常量（kRootArrayConstants = 0x40 等在枚举体内显式出现，上面已捕获）
    return tags


def _grab_macro_list(text, name):
    """抓 #define NAME(V) 的宏体（到下一个 #define 或文件尾）。"""
    m = re.search(rf"#define {name}\(([^)]*)\)(.*?)(?=\n#define |\Z)", text, re.S)
    return m.group(2) if m else None


def _acc_use(inner):
    """字节码对累加器的隐式读写（反编译时判断 acc 活跃性用）。

    现代写法 V(Name, ImplicitRegisterUse::kReadWriteAccumulator, ...)
    老写法  V(Name, AccumulatorUse::kReadWrite, ...)
    返回 "r" / "w" / "rw" / ""（完全不碰累加器）。
      kWriteShortStar（Star0-3）写的是寄存器、累加器只被读 → 记 "r"。
    """
    uses = set(re.findall(r"(?:ImplicitRegisterUse|AccumulatorUse)::(k\w+)", inner))
    if not uses:
        return ""
    # 短名（≤9.x 的 AccumulatorUse::kRead/kWrite/kReadWrite）与长名（10.x+ 的
    # ImplicitRegisterUse::kReadAccumulator/kWriteAccumulator）都要认 —— 只认长名时
    # 老族整张表的 acc 全空，反编译的 acc 活跃性判断失效（keyed 访问的键被当死值丢掉）。
    read = bool(uses & {"kRead", "kReadWrite",
                        "kReadAccumulator", "kReadWriteAccumulator",
                        "kReadAccumulatorWriteShortStar", "kWriteShortStar"})
    # 老写法 `AccumulatorUse::kWrite/kReadWrite` 也要算写 —— 只认长名时
    # 老族（≤8.6）整张表把 `Add`/`Call…` 这类标成 "r"，反编译的 acc 活跃性判断失效：
    # 调用结果被当死值丢掉（实测 node15 的 class_basic 少两行语句、算出来的数不对）。
    write = bool(uses & {"kWrite", "kReadWrite",
                         "kWriteAccumulator", "kReadWriteAccumulator"})
    return ("rw" if (read and write) else "r" if read else "w" if write else "")


def _expand_bytecode_entries(text, macro_name, entries, seen=None, depth=0):
    """递归展开 bytecode 宏列表。

    宏体里有两类调用：
      SUB_LIST(V)              → 子列表，递归
      V(Name, ..., OperandType::kX, ...)  → 条目（括号配平，可能跨行）
    """
    if seen is None:
        seen = set()
    if macro_name in seen or depth > 8:
        return
    seen.add(macro_name)
    body = _grab_macro_list(text, macro_name)
    if body is None:
        return
    i = 0
    while i < len(body):
        im = re.compile(r"\b(\w+)\s*\(").search(body, i)
        if not im:
            break
        ident = im.group(1)
        j = body.index("(", im.start())
        depth_p = 0
        k = j
        while k < len(body):
            if body[k] == "(":
                depth_p += 1
            elif body[k] == ")":
                depth_p -= 1
                if depth_p == 0:
                    break
            k += 1
        inner = body[j + 1:k]
        i = k + 1
        if ident in ("V", "V_TSA"):
            # V_TSA：CPU 无关的 trusted-space 变体，同样是枚举里的一个 opcode
            args = [a.strip() for a in inner.split(",")]
            name = args[0]
            m2 = re.match(r"^(\w+)$", name)
            if not m2 or name in ("ImplicitRegisterUse", "OperandScale"):
                continue
            operands = re.findall(r"OperandType::k(\w+)", inner)
            is_prefix = name in ("Wide", "ExtraWide")
            entries.append({
                "name": name,
                "operands": operands,
                "prefix": name if is_prefix else None,
                "acc": _acc_use(inner),
            })
        else:
            # 子列表（如 BYTECODE_LIST_WITH_UNIQUE_HANDLERS(V, V_TSA) / SHORT_STAR_BYTECODE_LIST(V)）
            _expand_bytecode_entries(text, ident, entries, seen, depth + 1)


def extract_bytecodes(text):
    """从 bytecodes.h 提取 opcode 顺序表（BYTECODE_LIST 递归展开）。

    枚举顺序 = 数值顺序。返回 [{"name", "operands", "prefix"}]。
    """
    if not re.search(r"#define BYTECODE_LIST\(", text):
        raise RuntimeError("BYTECODE_LIST not found")
    entries = []
    _expand_bytecode_entries(text, "BYTECODE_LIST", entries)
    return entries


# ------------------------------------------------------------------- roots.h

def _split_top_args(inner):
    """按顶层逗号切分宏实参（跳过字符串字面量与括号）。"""
    args, depth, cur, in_str = [], 0, "", None
    for ch in inner:
        if in_str:
            cur += ch
            if ch == in_str:
                in_str = None
            continue
        if ch in ("\"", "'"):
            in_str = ch
            cur += ch
            continue
        if ch == "(":
            depth += 1
        elif ch == ")":
            depth -= 1
        if ch == "," and depth == 0:
            args.append(cur.strip())
            cur = ""
        else:
            cur += ch
    if cur.strip():
        args.append(cur.strip())
    return args


def _roots_six_x(*sources):
    """6.x（Node 8/10 的 V8 6.2/6.8）根名表。

    条目形态：`V(Type, name, camel_name)`（用 camel_name 作索引名）、
    字符串 `V(name, "literal")`（用 `String:<literal>`，反编译时直接字面量化）、
    符号 `V(name[, "desc"])`、访问器 `V(name, AccessorName)`、
    结构映射 `V(NAME, Name, name)`、数据处理器 `V(NAME, Name, Size, name)`。
    """
    texts = [t for t in sources if t]

    def body(name):
        for t in texts:
            m = re.search(rf"#define {name}\(V\)(.*?)(?=\n#define |\Z)", t, re.S)
            if m:
                return m.group(1)
        return None

    def entries(name):
        b = body(name)
        if b is None:
            return None
        b = b.replace("\\\n", " ")
        out = []
        for m in re.finditer(r"\bV\(([^()]*)\)", b):
            out.append(_split_top_args(m.group(1)))
        return out

    out = []
    for a in entries("STRONG_ROOT_LIST") or []:
        out.append(a[2] if len(a) >= 3 else a[0])
    for a in entries("INTERNALIZED_STRING_LIST") or []:
        lit = a[1].strip()
        if lit.startswith('"') and lit.endswith('"'):
            lit = lit[1:-1]
        out.append("String:" + lit)
    for name in ("PRIVATE_SYMBOL_LIST", "PUBLIC_SYMBOL_LIST", "WELL_KNOWN_SYMBOL_LIST"):
        for a in entries(name) or []:
            out.append("Symbol:" + a[0])
    for a in entries("ACCESSOR_INFO_LIST") or []:
        out.append(a[1] if len(a) >= 2 else a[0])
    for a in entries("STRUCT_LIST") or []:
        out.append(a[1] if len(a) >= 2 else a[0])
    for a in entries("DATA_HANDLER_LIST") or []:
        out.append((a[1] + a[2] + "Map") if len(a) >= 3 else a[0])
    out.append("StringTable")
    for a in entries("SMI_ROOT_LIST") or []:
        out.append(a[2] if len(a) >= 3 else a[0])
    return out


def extract_roots(text, symbols_text=None, defs_text=None, torque_count=None):
    if torque_count is None:
        torque_count = TORQUE_MAP_COUNT
    """解析 roots.h 的 READ_ONLY + MUTABLE 根列表顺序，返回按索引排列的名字表。

    递归展开子列表宏；生成器宏按 adapter 命名。限制：TORQUE_DEFINED_MAP_ROOT_LIST
    是构建期由 torque 生成的（.tq 声明序），此处以 0 条占位——该段及其后的根索引
    不可靠，运行时对未知根走结构指纹分类（见 serializer.rs classify）。
    """
    # 6.x：根列表在 heap/heap.h 的 STRONG_ROOT_LIST/SMI_ROOT_LIST，其余子列表在
    # heap-symbols.h（字符串/符号）、accessors.h（访问器）、objects.h（结构映射）。
    # 枚举顺序见 RootListIndex（STRONG → 字符串 → 私有/公开/知名符号 → 访问器信息 →
    # STRUCT → DATA_HANDLER → string_table → SMI）。
    if text and "STRONG_ROOT_LIST" in text:
        return _roots_six_x(text, symbols_text, defs_text)

    sources = [text] + [s for s in (symbols_text, defs_text) if s]

    def grab(name):
        for s in sources:
            m = re.search(rf"#define {name}\b(.*?)(?=\n#define |\Z)", s, re.S)
            if m:
                return m.group(1)
        return None

    order = []

    def split_top(inner):
        """按顶层逗号切分宏实参。**必须跳过字符串字面量**：`V_(_, comma_string, ",")`
        的实参里就有逗号，早先会切成 `"` + `"` 两段，于是逗号字符串根的名字成了
        `String:`（空字面量）—— node24 的 `join(",")` 分隔符变成空串、结果全粘一起。"""
        args, depth, cur, in_str = [], 0, "", None
        for ch in inner:
            if in_str:
                cur += ch
                if ch == in_str:
                    in_str = None
                continue
            if ch in ("\"", "'"):
                in_str = ch
                cur += ch
                continue
            if ch == "(":
                depth += 1
            elif ch == ")":
                depth -= 1
            if ch == "," and depth == 0:
                args.append(cur.strip())
                cur = ""
            else:
                cur += ch
        if cur.strip():
            args.append(cur.strip())
        return args

    def invocations(body):
        """产出 (ident, inner) 序列，括号配平，跳过注释行，拼接续行。"""
        lines = []
        for raw in body.splitlines():
            code = raw.split("//")[0]
            lines.append(code)
        body = "\n".join(lines).replace("\\\n", " ")  # 预处理续行
        i = 0
        while i < len(body):
            m = re.compile(r"\b(\w+)\s*\(").search(body, i)
            if not m:
                break
            ident = m.group(1)
            j = body.index("(", m.start())
            d, k, in_str = 0, j, None
            while k < len(body):
                ch = body[k]
                # 字符串字面量里的括号不算配平 —— V8 里就有 `"function ("` 这种条目，
                # 早先不看引号会把它整条漏掉（后来发现正是 13.6 根索引差一的元凶）。
                if in_str:
                    if ch == "\\":
                        k += 2
                        continue
                    if ch == in_str:
                        in_str = None
                elif ch in ("\"", "'"):
                    in_str = ch
                elif ch == "(":
                    d += 1
                elif ch == ")":
                    d -= 1
                    if d == 0:
                        break
                k += 1
            yield ident, body[j + 1:k]
            i = k + 1

    def add_generator_entry(gen, inner, mode):
        args = split_top(inner)
        if len(args) < 2:
            return
        if "INTERNALIZED_STRING" in gen:
            # 条目 V(_, name_string, "literal")：优先用字面量，否则去 _string 后缀
            if len(args) >= 3 and args[2].startswith('"'):
                literal = args[2].strip('"')
                order.append("String:" + literal)
            else:
                order.append("String:" + args[1].removesuffix("_string"))
        elif "SYMBOL" in gen:
            order.append("Symbol:" + args[1])
        elif "ACCESSOR_INFO" in gen:
            order.append("AccessorInfo:" + args[1] + "_accessor")
        # 8.4 的条目在 `STRUCT_LIST_GENERATOR_BASE` 里（`STRUCT_LIST_GENERATOR` 只是转发，
        # 见 objects-definitions.h）；只认全名会让 41 个 map 根退化成 `Gen:<TYPE>`,
        # 于是 ArrayBoilerplateDescription 之类的类型识别全失效（node14 的数组字面量
        # 变成 `/* ?unknown(N) */ undefined`）。
        elif gen.startswith("STRUCT_LIST_GENERATOR"):
            # 条目 V(_, TYPE, Name, name) → Map 名 = Name + "Map"
            order.append((args[2] if len(args) >= 3 else args[1]) + "Map")
        elif mode == "maps" and gen in ("ALLOCATION_SITE_LIST", "DATA_HANDLER_LIST"):
            order.append((args[1] + args[2] + "Map") if len(args) >= 3 else args[1] + "Map")
        else:
            order.append("Gen:" + args[1])

    def expand_macro(name, depth=0, mode=None):
        """展开宏体。名字含 _GENERATOR（含 _INTL 变体）按生成器条目处理，
        否则按 roots.h 直写条目处理；未知宏体忽略。"""
        if depth > 10:
            return
        if name == "TORQUE_DEFINED_MAP_ROOT_LIST":
            order.append("__torque_map_list_unresolved__")
            return
        body = grab(name)
        if body is None:
            return
        is_generator = "_GENERATOR" in name
        my_mode = "maps" if name.endswith("_MAPS_LIST") else mode
        for ident, inner in invocations(body):
            # V_ 是 V8 的变体宏（"重要性较低"的字符串/符号根），同样占一个根索引 ——
            # 早先只认 `V(` 会把这类条目整批丢掉（":"/"|"/"-" 就是这样消失的，
            # 直接导致 13.6 的根索引错位、常量池字符串全错）。
            if ident in ("V", "V_"):
                if is_generator:
                    add_generator_entry(name, inner, my_mode)
                else:
                    args = split_top(inner)
                    if len(args) >= 3:
                        order.append(args[2])
                    elif len(args) == 2:
                        order.append(args[1])
            elif ident == "IF_WASM":
                args = split_top(inner)
                if my_mode == "maps" and len(args) >= 4:
                    order.append(args[3] + "Map")
                elif len(args) >= 4:
                    order.append(args[3])
            else:
                expand_macro(ident, depth + 1, my_mode)

    for top in ("READ_ONLY_ROOT_LIST", "MUTABLE_ROOT_LIST"):
        expand_macro(top)
    # torque 生成段（TORQUE_DEFINED_MAP_ROOT_LIST）：每张实例类型的 map 一个根，源码里看不到，
    # 但它的**条数**能用真值锚点解出来（见 TORQUE_MAP_COUNT 注释）。占位补齐后**继续保留
    # 后面的尾部**（ALLOCATION_SITE / NAME_FOR_PROTECTOR / DATA_HANDLER 等）—— 早先整段截断，
    # 导致 "length"/"join" 这类保护器字符串根缺失、node24 的属性名解析错。
    n = torque_count
    if "__torque_map_list_unresolved__" in order:
        cutoff = order.index("__torque_map_list_unresolved__")
        head, tail = order[:cutoff], order[cutoff + 1 :]
        order = head + [f"__torque_map_{i}__" for i in range(n)] + tail
    return order


# torque 生成段（TORQUE_DEFINED_MAP_ROOT_LIST）的条数：源码里没有这张列表
# （构建期由 torque 生成），只能按版本用**真值锚点**解出来。锚点取法：编译一份含
# 受保护字符串属性访问的探针（work/genprobe/roots.js），看真实 .jsc 里这些名字
# 被引用的根索引，再与表里 `String:<名>` 的位置对照：
#
#   V8 9.4  constructor=299 next=385 resolve=422 then=450     表天然对齐 → 36
#   V8 10.2 constructor=321 next=458 resolve=503 then=537     表天然对齐 → 36
#   V8 11.3 constructor=724 next=725 resolve=726 then=727     表整体 +2    → 34
#   V8 12.4 constructor=741 next=742 resolve=743 then=744     表天然对齐 → 36
#   V8 13.6 constructor=1006 next=1008 resolve=1009 then=1010 表天然对齐 → 36
#
# 11.3 少两格：旧值 36 会把 ALLOCATION_SITE_MAPS_LIST 与 NAME_FOR_PROTECTOR 段
# 整体后移两位，于是 for-of 的 `.next` 解析成 AllocationSite、保护器字符串全错位
# —— node20 的 for_of_in / spread_rest / generator 就是这么挂的。
# 实测锚点（同 11_3 的做法）：14.x 的 torque map 段比 13.6 少 4 条，按默认 36 会让
# 之后所有根索引 +4 —— twoclasses 的类 boilerplate 键是 `Root(1027) = String:constructor`，
# 用 36 时算成 1031；改成 32 后逐条对上（1027 constructor / 1028 next / 1029 resolve /
# 1030 then / 1031 valueOf），类模板随即能解出 `{ n, i: {"constructor":…, "bump":…} }`。
# 见 minor_table 处的说明：格式一致就直接复用同一张表。
# 表别名（格式一致时可直接复用另一张表）：目前**为空**。
# 试过两条都只是"看起来能解"、经不起满矩阵：
#   * 6.6 借 6.2：解析不报错，但结构门禁仍判无函数（`JSCD_TABLE` 调试路径会绕过门禁，
#     早先据此误判为可用）；
#   * 6.7 借 6.8：arith 侥幸通过，其余 12 个 fixture 照挂。
# 这两族各自需要真正的格式移植。机制留着，将来有证据时填。
TABLE_ALIAS: dict[str, str] = {}

TORQUE_MAP_COUNT_BY_VERSION = {
    "11_3": 34,
    "10_7": 25,   # node19：实测 delta +11（constructor 根 693，我们 704）
    "10_8": 25,   # node19
    "11_8": 34,   # node21：实测 delta +2（735 vs 737）
    "14_1": 32,   # node25：见下面 14.x 的说明
    "14_6": 32,   # node26
}
TORQUE_MAP_COUNT = 36


# ------------------------------------------------------------ code-serializer

def extract_header_layout(tag):
    """从 code-serializer.h 提取头字段偏移（解析常量表达式链）。"""
    text = git_show(tag, "src/snapshot/code-serializer.h")
    if text is None:
        return None
    consts = {"kMagicNumberOffset": 0, "kUInt32Size": 4}
    for m in re.finditer(r"static const uint32_t (k\w+)\s*=\s*([^;]+);", text):
        name, expr = m.group(1), m.group(2).strip()
        expr = re.sub(r"POINTER_SIZE_ALIGN\(([^)]*)\)", r"ALIGN8(\1)", expr)
        try:
            val = _eval_expr(expr, consts)
        except Exception:
            continue
        consts[name] = val
    layout = {
        "version_hash": consts.get("kVersionHashOffset", 4),
        "source_hash": consts.get("kSourceHashOffset", 8),
        "flag_hash": consts.get("kFlagHashOffset", 12),
        "read_only_checksum": consts.get("kReadOnlySnapshotChecksumOffset"),
        "payload_length": consts.get("kPayloadLengthOffset", 16),
        "checksum": consts.get("kChecksumOffset", 20),
        "header_size": consts.get("kHeaderSize", 24),
    }
    return layout


def _eval_expr(expr, consts):
    """求值 V8 头常量表达式（kX + kN 形式的链式常量，含 POINTER_SIZE_ALIGN）。"""
    expr = expr.strip()
    expr = expr.replace("POINTER_SIZE_ALIGN", "ALIGN8")
    expr = re.sub(r"k\w+", lambda m: str(consts.get(m.group(0), 0)), expr)
    m = re.match(r"^ALIGN8\((.*)\)$", expr, re.S)
    if m:
        inner = _eval_expr(m.group(1), consts)
        return (inner + 7) & ~7
    # 仅允许数字与四则运算
    if not re.fullmatch(r"[0-9+\-*/()\s~&|]+", expr):
        raise ValueError(f"unsupported expr: {expr!r}")
    return int(eval(expr))  # noqa: S307 受控输入（V8 源码常量表达式）


# 官方 Node 构建的 V8 条件开关（影响 runtime/intrinsic 枚举条目数）。
# 依据：common.gypi / node.gypi 的 v8_enable_* 设置 + V8 的 dev-only flag 惯例。
BUILD_DEFINES = {
    "V8_TRACE_UNOPTIMIZED": False,      # dev-only，release 构建不定义
    "V8_TRACE_FEEDBACK_UPDATES": False, # dev-only
    "V8_INTL_SUPPORT": True,            # Node 默认 full-icu
    "V8_ENABLE_WEBASSEMBLY": True,      # Node 默认启用
    "V8_TRACE_IGNITION": False,         # dev-only（8.4 用这个名字包 INTERPRETER_TRACE）
    # 14.x 起 runtime.h 用 `#ifdef V8_DUMPLING` 包两组条目（PrintDumpedFrame /
    # DumpExecutionFrame）。Node 不发这个开关 —— 留着会让 364 往后的
    # Runtime::FunctionId 整体 +1（实测：`ThrowConstAssignError` 真值 364、
    # 带这两条时算成 365，于是顶层 `DeclareGlobals` 认成 `DeclareEvalVar`，
    # 整个顶层代码无法摊平）。13.6 及更早没有这段，故只影响 14.x。
    "V8_DUMPLING": False,
}

# `IF_<FLAG>(F, ...)` 的取舍：官方 Node 构建的取值。
# 有证据才写在这里 —— SPARKPLUG_PLUS：node26 二进制里有 MaybePatchBinaryBaselineCode；
# WASM_RANDOM_FUZZERS / DRUMBRAKE：二进制里查无此名（fuzzer 与 wasm 解释器都不发）。
IF_FLAGS = {
    "IF_WASM": True,                    # = V8_ENABLE_WEBASSEMBLY
    "IF_WASM_DRUMBRAKE": False,
    "IF_SPARKPLUG_PLUS": True,
    "IF_V8_WASM_RANDOM_FUZZERS": False,
    "IF_TSA": True,
}


def strip_disabled_blocks(text):
    """按官方 Node 构建的开关**剔除 `#ifdef` 禁用的分支**，只留下会编译进去的那份。

    V8 头文件里同一宏常有"启用/空"两份定义，例如 8.4 runtime.h：

        #ifdef V8_TRACE_IGNITION
        #define FOR_EACH_INTRINSIC_INTERPRETER_TRACE(F, I) \
          F(InterpreterTraceBytecodeEntry, 3, 1)           \
          F(InterpreterTraceBytecodeExit, 3, 1)
        #else
        #define FOR_EACH_INTRINSIC_INTERPRETER_TRACE(F, I)
        #endif

    提取器原来按 `#define NAME(...)` 取**第一个**匹配 —— 于是把 dev-only 的两条
    Trace 条目也算进了 Runtime::FunctionId，整张表从那里起偏移 3（实测 node14：
    官方 `DeclareGlobals` 的 id 是 312，我们表里是 315；node16 恰好不受影响，
    所以这个错一直没暴露）。
    """
    out, stack = [], []   # stack 元素：[当前区域是否启用, 该 if 块的父区域是否启用]
    for line in text.splitlines():
        st = line.strip()
        enabled = stack[-1][0] if stack else True
        if st.startswith("#ifdef ") or st.startswith("#ifndef "):
            parts = st.split()
            flag = parts[1] if len(parts) > 1 else ""
            # 未列出的开关按"官方构建里未定义"处理（8.4/9.4 的 x64 Node 就是关掉
            # 指针压缩、sandbox、各种 trace 的）。注意别把 include guard
            # `#ifndef V8_RUNTIME_RUNTIME_H_` 整份裁掉 —— `#ifndef 未定义` = 保留。
            # 只在**显式**写进 BUILD_DEFINES 的开关上做取舍：
            #   显式 False（V8_TRACE_* 这类 dev-only）→ `#ifdef` 分支剔除、`#else` 保留；
            #   未知开关 → 一律保留（V8 头文件里 `#ifdef` 的第一分支就是官方启用的那份，
            #   include guard 的 `#ifndef` 也属这一类）。早先"未知=未定义"会把
            # Maglev/Turbofan 的 5 条 runtime 裁掉（node24 表偏 3、class_basic 挂），
            # "未知=已定义"又会把 include guard 整份裁掉（老族表直接空）。
            if flag not in BUILD_DEFINES:
                on = True
            else:
                on = BUILD_DEFINES[flag] if st.startswith("#ifdef ") else not BUILD_DEFINES[flag]
            stack.append([enabled and on, enabled])
            continue
        if st.startswith("#else") and stack:
            top = stack[-1]
            top[0] = top[1] and not top[0]
            continue
        if st.startswith("#endif") and stack:
            stack.pop()
            continue
        if enabled:
            out.append(line)
    return "\n".join(out)


def extract_runtime_names(text):
    """提取 Runtime::FunctionId 的顺序名表（id → name）。

    FunctionId = FOR_EACH_INTRINSIC(F) + FOR_EACH_INLINE_INTRINSIC(I)；
    前者 = RETURN_PAIR_IMPL + RETURN_OBJECT_IMPL，逐域拼接。条件域按 BUILD_DEFINES
    取舍（如 TRACE_UNOPTIMIZED/TRACE_FEEDBACK 在官方 release 构建中为空）。
    """
    if text is None:
        return []
    text = strip_disabled_blocks(text)

    def macro_body(name):
        # 10.x+ 是 `#define X(F, I)`；6.x–9.x 是 `#define X(F)`（枚举里
        # `FOR_EACH_INTRINSIC(F) FOR_EACH_INTRINSIC(I)` 各展开一遍）
        m = re.search(rf"#define {name}\(F, I\)(.*?)(?=\n#define |\Z)", text, re.S)
        if m:
            return m.group(1)
        m = re.search(rf"#define {name}\(F\)(.*?)(?=\n#define |\Z)", text, re.S)
        return m.group(1) if m else None

    def invocations(body):
        body = body.replace("\\\n", " ")
        i = 0
        while i < len(body):
            m = re.compile(r"\b([A-Za-z_][A-Za-z0-9_]*)\s*\(").search(body, i)
            if not m:
                break
            ident = m.group(1)
            j = body.index("(", m.start())
            d, k = 0, j
            while k < len(body):
                if body[k] == "(":
                    d += 1
                elif body[k] == ")":
                    d -= 1
                    if d == 0:
                        break
                k += 1
            yield ident, body[j + 1:k]
            i = k + 1

    out = []
    # 10.x+（`*_IMPL` 起点）宏体里 F/I 的书写顺序**就是**枚举顺序，照 body 顺序收集即可
    # （这条路径长期验证过：8.4 的 481 条与官方字节码逐一对上）。
    # 6.x–9.x 的聚合宏不同：枚举是 `FOR_EACH_INTRINSIC(F) FOR_EACH_INTRINSIC(I)`，
    # 即"所有 F 在前、所有 I 在后"，而宏体里 F/I 是穿插写的 —— 所以那条兜底路径
    # 要分开收集（见下面 fallback）。
    inline = []

    split_fi = not bool(re.search(r"#define FOR_EACH_INTRINSIC_RETURN_PAIR_IMPL\b", text))

    def expand(macro_name, depth=0):
        if depth > 6:
            return
        # 条件域：禁用则整段跳过
        for flag, enabled in BUILD_DEFINES.items():
            if macro_name.startswith(f"FOR_EACH_INTRINSIC_{flag.removeprefix('V8_').split('_')[0]}"):
                pass
        if macro_name == "FOR_EACH_INTRINSIC_TRACE_UNOPTIMIZED" and not BUILD_DEFINES["V8_TRACE_UNOPTIMIZED"]:
            return
        if macro_name == "FOR_EACH_INTRINSIC_TRACE_FEEDBACK" and not BUILD_DEFINES["V8_TRACE_FEEDBACK_UPDATES"]:
            return
        if macro_name == "FOR_EACH_INTRINSIC_INTL" and not BUILD_DEFINES["V8_INTL_SUPPORT"]:
            return
        if macro_name == "FOR_EACH_INTRINSIC_WASM" and not BUILD_DEFINES["V8_ENABLE_WEBASSEMBLY"]:
            return
        body = macro_body(macro_name)
        if body is None:
            return
        for ident, inner in invocations(body):
            if ident in ("F", "I"):
                name = inner.split(",")[0].strip()
                if name and name[0].isalpha():
                    if split_fi:
                        (out if ident == "F" else inline).append(name)
                    else:
                        out.append(name)
            elif ident.startswith("IF_"):
                # `IF_WASM(FOR_EACH_INTRINSIC_WASM, F, I)`：整个组按开关取舍；
                # `IF_SPARKPLUG_PLUS(F, MaybePatchBinaryBaselineCode, 4, 1)`：**直接一条**，
                # 老写法只认 FOR_EACH_ 开头的参数，会把这类整条漏掉（14.x 的
                # MaybePatchBinaryBaselineCode 就是这么丢的）。
                enabled = IF_FLAGS.get(ident, True)
                if enabled:
                    parts = [p.strip() for p in inner.split(",")]
                    if parts and parts[0].startswith("FOR_EACH_"):
                        expand(parts[0], depth + 1)
                    elif parts and parts[0] in ("F", "I"):
                        name = parts[1].strip() if len(parts) > 1 else ""
                        if name and name[0].isalpha():
                            out.append(name)
            elif ident.startswith("FOR_EACH_"):
                expand(ident, depth + 1)

    expand("FOR_EACH_INTRINSIC_RETURN_PAIR_IMPL")
    expand("FOR_EACH_INTRINSIC_RETURN_OBJECT_IMPL")
    if not out:
        # 6.x–9.x：枚举是 `FOR_EACH_INTRINSIC(F) FOR_EACH_INTRINSIC(I)`，起点宏没有
        # `_IMPL` 后缀。**必须展开聚合宏本身**（它按 TRIPLE→PAIR→OBJECT 的顺序拼子列表，
        # 少展开一个子列表会让后面所有 id 整体偏移 —— node8 的 DeclareGlobals 曾因此
        # 读成 DeclareEvalFunction）。
        expand("FOR_EACH_INTRINSIC")
        # inline 段是同一份列表用 I 再展开一次
        return out + (inline if inline else list(out))
    return out + inline


def extract_intrinsic_names(text):
    """INTRINSICS_LIST 顺序名表（IntrinsicId → 反汇编显示的 [_Name]）。"""
    if text is None:
        return []
    m = re.search(r"#define INTRINSICS_LIST\(V\)(.*?)(?=\n#define |\Z)", text, re.S)
    if not m:
        return []
    body = m.group(1).replace("\\\n", " ")
    return re.findall(r"V\(\s*([A-Za-z0-9_]+)\s*,", body)


def _tq_class(text, name):
    """在 .tq 文本里找 `class NAME ... { ... }`，返回 (body, base_name)。"""
    m = re.search(rf"(?:extern )?class {name}\b([^{{]*)\{{(.*?)\n\}}", text, re.S)
    if not m:
        return None, None
    header, body = m.group(1), m.group(2)
    bm = re.search(r"extends\s+(\w+)", header)
    return body, (bm.group(1) if bm else None)


def _parse_field_line(line):
    """字段行 → (name, type, is_variable_array)。忽略 weak/const 等修饰符。"""
    line = re.sub(r"^(weak|const|extern|static)\s+", "", line.strip())
    m = re.match(r"^const\s+([a-z_][a-z0-9_]*)\s*:\s*(.+?);?$", line)
    if not m:
        m = re.match(r"^([a-z_][a-z0-9_]*)\s*(\[[^\]]*\])?\s*:\s*(.+?);?$", line)
        if not m:
            return None
        name, bracket, ty = m.group(1), m.group(2), m.group(3)
        return name, ty, bool(bracket)
    return m.group(1), m.group(2), False


def _tq_type_size(ty, sources, tagged_size, sandbox, depth=0):
    """类型 → 字节数。sources: {file: text} 供跨文件查找 Struct 定义。"""
    ty = ty.strip()
    if ty in ("void", "constexpr '')"):
        return 0
    if "|" in ty:
        return tagged_size
    base = re.split(r"[<:]", ty)[0].strip()
    simple = {"int32": 4, "uint32": 4, "int16": 2, "uint16": 2,
              "int8": 1, "uint8": 1, "bool": 1, "Smi": tagged_size}
    if base in simple:
        return simple[base]
    if base in ("ProtectedPointer", "IndirectPointer", "ProtectedPointerTagged"):
        return 4 if sandbox else tagged_size
    if base == "Indirect":  # 沙箱间接指针（32 位表索引）
        return 4 if sandbox else tagged_size
    # bitfield struct X extends uint16/... → 底层类型尺寸
    for text in sources.values():
        bm = re.search(rf"bitfield struct {base}\s+extends\s+(\w+)", text)
        if bm:
            return _tq_type_size(bm.group(1), sources, tagged_size, sandbox, depth + 1)
    # 仅"值类型"（Struct 派生，如 BytecodeWrapper）内联展开；其余堆对象按 tagged 指针计
    if depth < 4:
        for text in sources.values():
            body, base_name = _tq_class(text, base)
            if body is None:
                continue
            if not _is_inline_struct(sources, base_name):
                break  # 堆对象 → 引用
            total = 0
            for line, cond, _ in _tq_fields(body, tagged_size, sandbox):
                if cond and not _cond_enabled(cond, tagged_size, sandbox):
                    continue
                parsed = _parse_field_line(line)
                if parsed and not re.fullmatch(r"padding\d*", parsed[0]):
                    total += _tq_type_size(parsed[1], sources, tagged_size, sandbox, depth + 1)
            return total
    return tagged_size


def _is_inline_struct(sources, class_name, depth=0):
    """类是否内联存储（继承 Struct 的值类型）。"""
    if not class_name or depth > 4:
        return False
    if class_name == "Struct":
        return True
    for text in sources.values():
        body, base = _tq_class(text, class_name)
        if body is not None:
            return _is_inline_struct(sources, base, depth + 1)
    return False


def _cond_enabled(cond, tagged_size, sandbox):
    cond = cond.strip()
    negate = False
    if cond.startswith("ifnot(") or cond.startswith("ifnot ("):
        negate = True
        cond = cond[cond.index("(") + 1 : cond.rindex(")")]
    elif cond.startswith("if(") or cond.startswith("if ("):
        cond = cond[cond.index("(") + 1 : cond.rindex(")")]
    flags = {
        "TAGGED_SIZE_8_BYTES": tagged_size == 8,
        "TAGGED_SIZE_4_BYTES": tagged_size == 4,
        "V8_ENABLE_SANDBOX": sandbox,
        "V8_EXTERNAL_CODE_SPACE": False,
        "V8_ENABLE_LEAPTIERING": True,
        "V8_ENABLE_WEBASSEMBLY": BUILD_DEFINES["V8_ENABLE_WEBASSEMBLY"],
        "V8_INTL_SUPPORT": BUILD_DEFINES["V8_INTL_SUPPORT"],
    }
    val = flags.get(cond, True)
    return (not val) if negate else val


def _tq_fields(body, tagged_size, sandbox):
    """解析类的字段行 → [(字段声明, 条件, None)]。

    处理 Torque 注解：@if/@ifnot(...) 作为条件；其余 @注解（@customWeakMarking 等）
    直接剥离；注解可与字段同行或独占一行。
    """
    out = []
    pending_cond = None
    for raw in body.splitlines():
        line = raw.split("//")[0].strip()
        if not line:
            continue
        cond = pending_cond
        pending_cond = None
        while line.startswith("@"):
            cm = re.match(r"@(if|ifnot)\s*\(([^)]*)\)\s*(.*)$", line)
            if cm:
                c = f"{cm.group(1)}({cm.group(2)})"
                rest = cm.group(3).strip()
                if rest:
                    out.append((rest, c, None))
                    line = ""
                else:
                    pending_cond = c
                    line = ""
                break
            am = re.match(r"@\w+\s*(.*)$", line)
            rest = am.group(1).strip() if am else ""
            line = rest
            if not line:
                break
        if line and not line.startswith("@"):
            out.append((line, cond, None))
    return out


def tq_layout(sources, class_name, tagged_size, sandbox):
    """按继承链 + @if 条件推导类的内存布局。

    sources: {file: text}；返回 {"header_size": 变长区起点, "fields": {name: offset}}。
    """
    chain = []
    cur = class_name
    guard = 0
    while cur and guard < 8:
        guard += 1
        found = False
        for text in sources.values():
            body, base = _tq_class(text, cur)
            if body is not None:
                chain.append((cur, body))
                cur = base
                found = True
                break
        if not found:
            break
    if not chain:
        return None
    offset = tagged_size  # HeapObject: map
    fields = {}
    for name, body in reversed(chain):
        for fname, cond, _ in _tq_fields(body, tagged_size, sandbox):
            # 条件字段
            if cond and not _cond_enabled(cond, tagged_size, sandbox):
                continue
            parsed = _parse_field_line(fname)
            if not parsed:
                continue
            field, ty, is_var = parsed
            # Struct 的显式 padding 字段（padding1/padding2）不计入（真机校准：12.4 头=64）
            if re.fullmatch(r"padding\d*", field):
                continue
            if is_var:
                # 变长数组：标记变长区起点（bytecode / 字符串内容从这里开始）
                fields.setdefault(f"__var_{field}", offset)
                continue
            size = _tq_type_size(ty, sources, tagged_size, sandbox)
            fields[field] = offset
            offset += size
    return {"header_size": offset, "fields": fields}


def bytecode_array_layout(sources, tagged_size, sandbox):
    """BytecodeArray 头布局（版本间差异大：9.4=54 / 10.2=56 / 11.3=54 / 12.4+=64）。"""
    return tq_layout(sources, "BytecodeArray", tagged_size, sandbox)


def extract_frame_layout(text):
    """解释器帧常量 → 寄存器命名基址（随版本变：9.x–11.x = -6，12.x+ = -7）。

    kRegisterFileStartOffset = -kFixedFrameSizeFromFp/S - 1，
    其中 UnoptimizedFrameConstants::kFixedFrameSizeFromFp
        = StandardFrameConstants::kFixedFrameSizeFromFp + extra*S
        = (3*S + kCPSlotSize) + extra*S
    派生：context = start+1，closure = start+2，first_param = start-2。
    """
    if text is None:
        return None
    m = re.search(r"class UnoptimizedFrameConstants.*?DEFINE_STANDARD_FRAME_SIZES\((\d+)\)", text, re.S)
    if not m:
        return extract_frame_layout_legacy(text)
    extra = int(m.group(1))
    # V8_EMBEDDED_CONSTANT_POOL 在官方 Node 构建中关闭（真机校准：9.4 start=-6、12.4 start=-7）
    cp_slots = 0
    standard = 3 + cp_slots          # 以 S 为单位
    fixed = standard + extra
    start = -(fixed + 1)             # kRegisterFileFromFp/S
    return {
        "reg_file_start": start,
        "context_index": start + 1,
        "closure_index": start + 2,
        "first_param": start - 2,
        "extra_slots": extra,
        "cp_slots": cp_slots,
    }


def extract_frame_layout_legacy(text):
    """V8 ≤ 8.4 的旧帧布局（`InterpretedFrameConstants`/`InterpreterFrameConstants`）。

    现代提取器找的是 `UnoptimizedFrameConstants` + `DEFINE_STANDARD_FRAME_SIZES(n)`，
    8.4 里没有这个类 → 以前直接返回 None，于是寄存器编码常量全落默认值，
    老族 disasm 打出 `Star a-8` 这种垃圾。8.4 的公式（frame-constants.h）：

        StandardFrameConstants::kFixedFrameSizeFromFp = 2*S + kCPSlotSize
        InterpreterFrameConstants::kRegisterFileFromFp
            = -StandardFrameConstants::kFixedFrameSizeFromFp - 3*S
        kFunctionOffset = -2*S - kCPSlotSize   → closure = (regfile - kFunctionOffset)/S
        kContextOffset  = -S                   → context = (regfile - kContextOffset)/S
        kLastParamFromFp = kCallerSPOffset = kFixedFrameSizeAboveFp = 2*S
                                               → last_param = (regfile - 2*S)/S

    参数索引**随 parameter_count 变**（`Register::FromParameterIndex`，
    interpreter/bytecode-register.cc）：`this = kLastParamRegisterIndex - pc + 1`，
    所以这里不给 first_param，改给 `param_base = last_param + 1`，运行时按 pc 现算。
    官方 Node 构建 kCPSlotSize = 0（实测 node12/14：reg_file_start=-5、closure=-3、
    context=-4、last_param=-7、this=-6-pc）。
    """
    if text is None or "kRegisterFileFromFp" not in text:
        return None
    cp_slots = 0  # V8_EMBEDDED_CONSTANT_POOL 在官方 Node 构建中关闭
    standard = 2 + cp_slots
    start = -(standard + 3)
    return {
        "reg_file_start": start,
        "context_index": start + 1,
        "closure_index": start + 2 + cp_slots,
        "last_param": start - 2,
        "param_base": start - 1,
        "extra_slots": 2,
        "cp_slots": cp_slots,
    }


def extract_parameter_count_semantics(text):
    """parameter_count() 的语义：V8 ≤ 12 存"字节数"（需 >> log2(kSystemPointerSize)）；
    13.x 起直接是计数（uint16）。"""
    if text is None:
        return {"direct": False}
    m = re.search(r"parameter_count\(\) const \{(.*?)\n\}", text, re.S)
    if m and re.search(r"ReadField<uint16_t>", m.group(1)):
        return {"direct": True}
    return {"direct": False}


def extract_scope_info(text, globals_text):
    """ScopeInfo 布局差异（名字取值路径随版本变）。

    - flags 编码：V8 ≤ 12 = SmiTagged（值在高 32 位），13.x = 裸 uint32
    - 变长区顺序：V8 ≤ 12 = names 在 infos 前、position_info 在后；
      13.x = position_info 在前、names/infos 可走 hashtable（> kMaxInlinedLocalNamesSize）
    """
    if text is None:
        return None
    max_inlined = 75
    if globals_text:
        m = re.search(r"kScopeInfoMaxInlinedLocalNamesSize\s*=\s*(\d+)", globals_text)
        if m:
            max_inlined = int(m.group(1))
    flags_smi = "SmiTagged<ScopeFlags>" in text
    # 只有存在 context_local_names_hashtable 的版本才用阈值分流；否则名字恒内联
    has_table = "context_local_names_hashtable" in text
    m_pos = text.find("position_info")
    m_names = text.find("context_local_names")
    early_position = m_pos != -1 and m_names != -1 and m_pos < m_names
    out = {
        "flags_smi": flags_smi,
        "position_info_early": early_position,
        "max_inlined_names": max_inlined if has_table else (1 << 30),
        "saved_class_bit": 10,   # flags 位域位置（9.x–13.x 一致）
        "function_variable_bits": [12, 13],
        "receiver_bits": [7, 8],
        "has_inferred_bit": 14,
    }
    # ≤8.x（torque 之前）用链式位域声明，位置与 9.x+ 不同（7.8 没有
    # has_saved_class_variable_index → function_variable 落在 11..12 而非 12..13，
    # 照 9.x 的定值读会把函数名/形参名读成相邻槽的垃圾）。
    bits = scope_flags_bits_from_chain(text)
    if bits:
        out.update(bits)
    return out


def scope_flags_bits_from_chain(text):
    """从 `using XField = PrevField::Next<Type, bits>;` 链推导 ScopeFlags 各字段起点。"""
    # 链头是 `using ScopeTypeField = BitField<ScopeType, 0, 4>;`（带显式起点），
    # 其余是 `using XField = PrevField::Next<Type, width>;`（多行声明要折叠空白）。
    flat = re.sub(r"\s+", " ", text)
    nxt = re.compile(r"using (\w+?)Field = (\w+?)Field::Next<([\w:]+), (\d+)>;")
    head = re.compile(r"using (\w+?)Field = BitField<([\w:]+), (\d+), (\d+)>;")
    succ = {prev: (name, int(width)) for name, prev, _ty, width in nxt.findall(flat)}
    heads = head.findall(flat)
    if not heads:
        return None
    order, cur, bit = [], heads[0][0], int(heads[0][2])
    seen = set()
    while cur and cur not in seen:
        seen.add(cur)
        if cur in succ:
            name, width = succ[cur]
            order.append((cur, bit, width))
            bit += width
            cur = name
        else:
            break
    starts = {name: (start, width) for name, start, width in order}
    if "ReceiverVariable" not in starts or "FunctionVariable" not in starts:
        return None
    rs, rw = starts["ReceiverVariable"]
    fs, fw = starts["FunctionVariable"]
    out = {
        "receiver_bits": [rs, rs + rw - 1],
        "function_variable_bits": [fs, fs + fw - 1],
    }
    if "HasInferredFunctionName" in starts:
        out["has_inferred_bit"] = starts["HasInferredFunctionName"][0]
    if "HasSavedClassVariableIndex" in starts:
        out["saved_class_bit"] = starts["HasSavedClassVariableIndex"][0]
    else:
        out["saved_class_bit"] = 30   # 该版本没有这个位（8.x 之前）→ 永不置位
    return out


def extract_hash_fold(tag):
    """V8 ≥ 12 引入 base::Hasher（左折叠）；此前为变参递归（右折叠）。

    Hasher 的位置换过：早期在 `src/base/functional.h`，后来拆到 `src/base/hashing.h`。
    两处都没有时按大版本号兜底（≥12 一律左折叠）——比"猜错折叠方式"安全。
    """
    for path in ("src/base/functional.h", "src/base/hashing.h"):
        fh = git_show(tag, path)
        if fh and "class Hasher" in fh and "hash_value_unsigned_impl" in fh:
            return "left_fold"
    vv = parse_v8_version(tag)
    if vv and vv[0] >= 12:
        return "left_fold"
    return "right_fold"


def extract_tagged_size(tag):
    """kTaggedSize：看 build 配置里指针压缩是否对该平台开启。

    code cache 与平台绑定；Node 官方构建：linux/win x64 开压缩(4)，macOS 不开(8)。
    这里从 node 的 common.gypi 读默认值并按平台覆写。
    """
    gypi = repo_file(tag, "node.gypi", in_v8=False) or ""
    gypi += repo_file(tag, "common.gypi", in_v8=False) or ""
    enabled = re.search(r"'v8_enable_pointer_compression':\s*(\d)", gypi)
    # Node 官方默认只在 linux x64 / win x64 开；macOS/arm64 均不开
    return 4 if (enabled and enabled.group(1) == "1") else 8


# ------------------------------------------------------------------- 主流程

MAGIC_BASE = 0xC0DE0000


def v8_hash(fold, a, b, c, d):
    M = 0xC6A4A7935BD1E995
    MASK = (1 << 64) - 1

    def mix32(v):
        v &= 0xFFFFFFFF
        v = (~v & 0xFFFFFFFF) + ((v << 15) & 0xFFFFFFFF)
        v &= 0xFFFFFFFF
        v ^= v >> 12
        v = (v + ((v << 2) & 0xFFFFFFFF)) & 0xFFFFFFFF
        v ^= v >> 4
        v = (v * 2057) & 0xFFFFFFFF
        v ^= v >> 16
        return v

    def combine(seed, val):
        val = (val * M) & MASK
        val ^= val >> 47
        val = (val * M) & MASK
        seed ^= val
        seed = (seed * M) & MASK
        return seed

    s = 0
    order = [d, c, b, a] if fold == "right_fold" else [a, b, c, d]
    for v in order:
        s = combine(s, mix32(v))
    return s & 0xFFFFFFFF


def _deep_fill(dst, src, path=""):
    """把 src 里有、dst 里没有（或值为 null）的键补进 dst（递归；list 不合并）。

    **null 视同缺失**：老族的 `scope_info` 常常抽不出来（7.9 的 ScopeInfo 在源码里不是
    .tq 定义）→ 生成的是 `"scope_info": null`，键在、值是空 —— 只补"缺键"的话
    donor 的配置永远补不上，反编译于是用默认布局读名字表，变量名全丢
    （node13 的 closure 里 `n` 成了 `__ctx.ctx4`）。
    """
    if not isinstance(dst, dict) or not isinstance(src, dict):
        return 0
    n = 0
    for k, v in src.items():
        if k not in dst or dst[k] is None:
            dst[k] = v
            n += 1
        elif isinstance(dst[k], dict) and isinstance(v, dict):
            n += _deep_fill(dst[k], v, f"{path}{k}.")
    return n


def donor_for(minor, donors):
    """最近的 donor：按 major*100+minor 的距离选。

    老族（≤8）只许借**不高于**自己的表 —— 那些手工字段（string 布局、legacy 标签）
    是向下兼容的；现代族（≥9）直接借最近的一张（9.0–9.3 借 9.4，而不是隔着代的 8.4：
    实测借 8.4 会把老的对象布局带进来，"object 3 size mismatch: consumed 40 of 32"）。
    """
    if not donors:
        return None
    def key(m):
        a, b = (int(x) for x in m.split("."))
        return a * 100 + b
    tgt = key(minor)
    major = int(minor.split(".")[0])
    pool = list(donors)
    # 一律借**最近**的一张（含向上借）：6.6/6.7 该借 6.8、8.1/8.3 该借 8.4；
    # 早先"只许借更低版本"是因为老族的手工字段被认为向下兼容，实测向上借同样成立，
    # 而"只往下"会把 6.6 借到 6.2、8.1 借到 7.8，反而更远。
    pool = list(donors)
    if False:
        pass
    return min(pool, key=lambda m: abs(key(m) - tgt))


# V8 14 把操作数种类从"编码形态"改成了"语义名"：`kIdx` 变成 `kConstantPoolIndex`，
# 另加 kFeedbackSlot/kContextSlot/kCoverageSlot/kAbortReason/kEmbeddedFeedback。
# 解码器与渲染器共用历史那套名字（大小/可缩放性逐项对齐：见括号里的 size/scalable），
# 所以这里把新名折叠回去 —— 否则 14.x 的池索引会被当成未知种类，闭包工厂识别不出来。
OPERAND_TYPE_ALIASES = {
    "ConstantPoolIndex": "Idx",     # 1/scalable
    "ContextSlot": "Idx",           # 1/scalable
    "FeedbackSlot": "Idx",          # 1/scalable
    "CoverageSlot": "Idx",          # 1/scalable
    "AbortReason": "Flag8",         # 1/fixed
    "EmbeddedFeedback": "Flag16",   # 2/fixed
}


def normalize_operand_types(table):
    """把 14.x 的语义操作数名折叠回历史种类（bytecodes 与 operand_types 一起改）。"""
    aliases = OPERAND_TYPE_ALIASES
    table["bytecodes"] = [
        {**b, "operands": [aliases.get(o, o) for o in b.get("operands", [])]}
        for b in table["bytecodes"]
    ]
    merged = {}
    for k, v in table["operand_types"].items():
        merged.setdefault(aliases.get(k, k), v)
    table["operand_types"] = merged
    return table


def ver_key(version):
    return tuple(int(x) for x in version.lstrip("v").split("."))


def load_index(path, floor=None):
    """nodejs.org/dist/index.json：每个发布带 version 与 v8。缺文件时自动抓一次。"""
    if not os.path.exists(path):
        os.makedirs(os.path.dirname(path) or ".", exist_ok=True)
        with open(path, "w", encoding="utf-8") as f:
            json.dump(json.loads(_http_text("https://nodejs.org/dist/index.json")), f)
    entries = json.load(open(path, encoding="utf-8"))
    if floor:
        entries = [e for e in entries if ver_key(e["version"]) >= floor]
    return entries


def tags_for_all(entries, major_floor=None):
    """每个**不同的 V8 版本**取它最新的那个 Node 发布当 tag。

    表内容取决于 V8 源码，同一个 V8 版本的不同 patch 发布源码可能不同，所以按
    4 段 V8 版本（13.6.233.17）分组，而不是 major.minor。
    """
    newest = {}
    for e in entries:
        v8 = e.get("v8")
        if not v8:
            continue
        if major_floor and ver_key(e["version"]) < major_floor:
            continue
        cur = newest.get(v8)
        if cur is None or ver_key(e["version"]) > ver_key(cur):
            newest[v8] = e["version"]
    return sorted(set(newest.values()), key=ver_key)


def main():
    global NODE, CACHE_DIR, OFFLINE
    ap = argparse.ArgumentParser()
    ap.add_argument("--node", default=None,
                    help="本地 node 克隆（可选；给了就优先用它，离线也能跑）")
    ap.add_argument("--tag", action="append", default=[],
                    help="node tag，可多次；不给则用 --all-from")
    ap.add_argument("--all-from", default=None,
                    help="从 dist/index.json 取所有 >= 该版本的发布（如 8.0.0），"
                         "每个不同的 V8 版本挑一个 tag")
    ap.add_argument("--index", default=None,
                    help="nodejs.org/dist/index.json 缓存（缺省 workspace/node-dist-index.json，"
                         "没有就自动下载一次）")
    ap.add_argument("--cache", default=None,
                    help="单文件源码缓存目录（缺省 workspace/node-src）")
    ap.add_argument("--offline", action="store_true", help="只用缓存/克隆，不联网")
    ap.add_argument("--repo-dir", default=None,
                    help="blob 过滤的部分克隆目录（缺省 workspace/node-partial，自动创建）")
    ap.add_argument("--no-clone", action="store_true",
                    help="不建部分克隆，直接走 raw.githubusercontent")
    ap.add_argument("--jobs", type=int, default=16, help="并发取文件数（默认 16）")
    ap.add_argument("--donors", default=None,
                    help="已知表的目录（tables/）：新表缺的字段从最近的 donor 借")
    ap.add_argument("--keep-existing", action="store_true",
                    help="输出目录里已存在的 v<major>_<minor>.json 直接沿用，不覆盖")
    ap.add_argument("--ro-maps", default=None,
                    help="已生成的 ro-map 目录（workspace/ro）：按 V8 minor 收进 tables/ 并内嵌")
    ap.add_argument("--per-minor", action="store_true",
                    help="每个 V8 major.minor 只保留最新 patch 的表（其余发布映射到它）")
    ap.add_argument("--out", default=None, help="输出目录（默认仓库根）")
    args = ap.parse_args()
    NODE = args.node
    OFFLINE = args.offline
    out_root = args.out or os.path.join(os.path.dirname(__file__), "..")
    CACHE_DIR = args.cache or os.path.join(out_root, "workspace", "node-src")
    global REPO_DIR
    if not args.no_clone:
        REPO_DIR = args.repo_dir or os.path.join(out_root, "workspace", "node-partial")
    tables_dir = os.path.join(out_root, "tables")
    os.makedirs(tables_dir, exist_ok=True)

    donors = {}
    if args.donors:
        for f in glob.glob(os.path.join(args.donors, "v*.json")):
            t = json.load(open(f))
            donors[".".join(t["v8"].split(".")[:2])] = t
        print(f"donors: {sorted(donors)}", file=sys.stderr)

    index_path = args.index or os.path.join(out_root, "workspace", "node-dist-index.json")
    floor = ver_key(args.all_from) if args.all_from else None
    index = load_index(index_path, floor=floor) if (args.all_from or args.index) else []
    index_by_v8 = {}
    for e in index:
        if e.get("v8"):
            index_by_v8.setdefault(e["v8"], []).append(e)

    tags = list(args.tag)
    if args.all_from:
        tags += tags_for_all(index, major_floor=floor)
        print(f"index: {len(index)} releases >= {args.all_from}, "
              f"{len(set(e['v8'] for e in index if e.get('v8')))} distinct V8 versions", file=sys.stderr)
    if not tags:
        ap.error("需要 --tag 或 --all-from")
    tags = sorted(set(tags), key=ver_key)

    versions = []       # manifest 条目
    table_files = {}    # file -> table dict
    file_of_content = {}  # content-hash -> filename（去重）

    tables_by_v8 = {}   # V8 版本字符串 -> table dict（manifest 用它精确映射每个发布）
    fold_of_v8 = {}     # V8 版本字符串 -> hash 折叠方式
    fname_of_v8 = {}    # V8 版本字符串 -> 表文件名
    for i, tag in enumerate(tags, 1):
        prefetch(tag, jobs=args.jobs)
        vv = parse_v8_version(tag)
        if vv is None:
            print(f"skip {tag}: no v8-version.h", file=sys.stderr)
            continue
        print(f"[{i}/{len(tags)}] {tag} -> V8 {'.'.join(map(str, vv))}", file=sys.stderr)
        maj, minor, build, patch = vv
        key = f"{maj}_{minor}"

        src_b = git_show(tag, "src/interpreter/bytecodes.h")
        src_o = git_show(tag, "src/interpreter/bytecode-operands.h")
        src_t = (
            git_show(tag, "src/snapshot/serializer-deserializer.h")
            or git_show(tag, "src/snapshot/serializer-common.h")
            or git_show(tag, "src/snapshot/serializer.h")
        )
        src_serializer_h = git_show(tag, "src/snapshot/serializer.h")
        # 旧族：tag 常量散落在 serializer-common.h 与 serializer.h（8.x 两族并存）
        src_t = (src_t or "") + "\n" + (src_serializer_h or "")
        src_r = git_show(tag, "src/roots/roots.h") or git_show(tag, "src/roots.h")
        src_s = git_show(tag, "src/init/heap-symbols.h") or git_show(tag, "src/heap-symbols.h")
        src_d = git_show(tag, "src/objects/objects-definitions.h") or git_show(
            tag, "src/objects-definitions.h"
        )
        src_rt = git_show(tag, "src/runtime/runtime.h")
        src_intr = git_show(tag, "src/interpreter/interpreter-intrinsics.h")
        # BytecodeArray 定义位置随版本移动：9.x–11.x 在 objects/code.tq，12.x+ 在 objects/bytecode-array.tq
        src_code = git_show(tag, "src/objects/bytecode-array.tq") or git_show(tag, "src/objects/code.tq")
        src_trusted = git_show(tag, "src/objects/trusted-object.tq")
        src_fixed = git_show(tag, "src/objects/fixed-array.tq")
        src_frame = git_show(tag, "src/execution/frame-constants.h") or git_show(
            tag, "src/frame-constants.h"
        )
        src_sfi = git_show(tag, "src/objects/shared-function-info.tq")
        src_scope = git_show(tag, "src/objects/scope-info.tq")
        src_ba_inl = git_show(tag, "src/objects/bytecode-array-inl.h")
        src_globals = git_show(tag, "src/common/globals.h")
        if not all([src_b, src_o, src_t]):
            print(f"skip {tag}: missing V8 sources", file=sys.stderr)
            continue

        table = {
            "v8": f"{maj}.{minor}.{build}.{patch}",
            "source_tag": tag,
            "hash": {"algorithm": extract_hash_fold(tag)},
            "header": extract_header_layout(tag),
            "tagged_size": extract_tagged_size(tag),
            "bytecodes": extract_bytecodes(src_b),
            "operand_types": extract_operand_types(src_o),
            "serialization": {
                "tags": extract_serialization_tags(src_t) if "enum Bytecode" in (src_t or "") else {},
                # legacy 段（kSpaceMask/kHotObjectMask 等）只对**真正用老 payload 的 V8 ≤ 8** 有意义。
                # 9.0–9.3 的源码里这些常量还在（提取器照样能收），但 .jsc 已经是新格式 ——
                # 留着会让序列化器按老方式解 space 编码的 tag：实测 16.3(9.0) 在 pos=5 把
                # `new space=7` 读成只读堆 backref，随后 unknown serialization tag 0x0d。
                "legacy": extract_legacy_tags(src_t) if maj <= 8 else {},
            },
            "roots": extract_roots(
                src_r,
                src_s,
                src_d,
                TORQUE_MAP_COUNT_BY_VERSION.get(key, TORQUE_MAP_COUNT),
            )
            if src_r
            else [],
            # 根索引校准：13.x 的 RootIndex 枚举在只读根之前还有一位（源码列表里看不到），
            # 实测校正在表首补一个占位即可对齐（依据：真实 .jsc 里 ":"→377、"-"→364、
            # "target"→849，与补位后的表逐一对上）。12.x 及更早无需补。
            "roots_shift": 0,
            "frame": extract_frame_layout(src_frame),
            "scope_info": extract_scope_info(src_scope, src_globals),
            "parameter_count": extract_parameter_count_semantics(src_ba_inl),
            "shared_function_info": tq_layout(
                {k: v for k, v in {
                    "shared-function-info.tq": src_sfi,
                    "trusted-object.tq": src_trusted,
                    "exposed-trusted-object.tq": src_trusted,
                }.items() if v},
                "SharedFunctionInfo",
                extract_tagged_size(tag),
                sandbox=extract_tagged_size(tag) == 4,
            ),
            "runtime_names": extract_runtime_names(src_rt),
            "intrinsic_names": extract_intrinsic_names(src_intr),
            "bytecode_array": bytecode_array_layout(
                {k: v for k, v in {
                    "bytecode-array.tq": src_code,
                    "code.tq": src_code,
                    "exposed-trusted-object.tq": src_trusted,
                    "trusted-object.tq": src_trusted,
                    "fixed-array.tq": src_fixed,
                }.items() if v},
                extract_tagged_size(tag),
                sandbox=extract_tagged_size(tag) == 4,
            ),
        }

        # 继承：老族有些字段源码里造不出来（string 布局、ScopeInfo 位域、legacy 标签…），
        # 从最近的已知表借一份；生成出来的值永远优先。
        if donors:
            minor = f"{maj}.{minor_}" if False else f"{maj}.{minor}"
            dm = donor_for(minor, donors)
            if dm:
                filled = _deep_fill(table, donors[dm])
                if filled:
                    print(f"  inherit {tag}: {filled} keys <- v{dm}", file=sys.stderr)

        # 归一化放在**继承之后**：donor 里可能还带着 14.x 的新名，一起折叠干净。
        normalize_operand_types(table)
        # 同理：donor（老族表）会把 legacy 段带回来 —— V8 > 8 一律清掉。
        if int(table["v8"].split(".")[0]) > 8:
            table.get("serialization", {}).pop("legacy", None)

        content_key = hashlib.sha1(json.dumps(table, sort_keys=True).encode()).hexdigest()
        if content_key not in file_of_content:
            fname = f"v{key}.json"
            if fname in table_files:
                fname = f"v{key}_{tag.lstrip('v')}.json"
            file_of_content[content_key] = fname
            table_files[fname] = table
        fname = file_of_content[content_key]
        tables_by_v8[table["v8"]] = table
        fname_of_v8[table["v8"]] = fname
        fold_of_v8[table["v8"]] = table["hash"]["algorithm"]

        # 没给 index 时，至少把 tag 自己登记进去
        entries = index_by_v8.get(table["v8"]) or [{"version": tag.lstrip("v"), "v8": table["v8"]}]
        for e in entries:
            e_v8 = e.get("v8") or table["v8"]
            p = [int(x) for x in e_v8.split(".")]
            versions.append({
                "v8": e_v8,
                "hash": v8_hash(table["hash"]["algorithm"], *p),
                "node": e["version"],
                "table": file_of_content[content_key],
            })

    # index 里每个发布 → 它那个 V8 版本的表（4 段精确匹配；表是按 V8 版本生成的，
    # 同名表已按内容去重，所以这里给的是"这个发布该用哪张表"）
    minor_table = {}
    for fname, t in table_files.items():
        minor_table.setdefault(".".join(t["v8"].split(".")[:2]), fname)
    # 格式别名：这些 minor 与另一张表**同格式** —— 实测用那张表能解析、反编译，
    # 且行为对拍通过（node10.3 的产物用 v6_2、node10.8 的用 v6_8）。与其维护一份会
    # 逐渐漂移的副本，不如让它们的发布直接指向同一张表。
    for alias_minor, target_minor in TABLE_ALIAS.items():
        if target_minor in minor_table:
            minor_table[alias_minor] = minor_table[target_minor]
    for e in index:
        e_v8 = e.get("v8")
        if not e_v8:
            continue
        fname = None
        mk = ".".join(e_v8.split(".")[:2])
        if mk in TABLE_ALIAS:
            fname = minor_table.get(TABLE_ALIAS[mk])
        if fname is None and e_v8 in tables_by_v8:
            fname = file_of_content.get(hashlib.sha1(
                json.dumps(tables_by_v8[e_v8], sort_keys=True).encode()).hexdigest())
        if fname is None:
            fname = minor_table.get(".".join(e_v8.split(".")[:2]))
        if fname is None:
            print(f"warn: no table for node {e['version']} (V8 {e_v8})", file=sys.stderr)
            continue
        fold = fold_of_v8.get(e_v8) or (
            "left_fold" if int(e_v8.split(".")[0]) >= 12 else "right_fold")
        p = [int(x) for x in e_v8.split(".")]
        versions.append({
            "v8": e_v8,
            "hash": v8_hash(fold, *p),
            "node": e["version"],
            "table": fname,
        })

    # 去重 hash 索引
    seen = set()
    uniq_versions = []
    for v in versions:
        k = (v["v8"], v["hash"], v["node"])
        if k in seen:
            continue
        seen.add(k)
        uniq_versions.append(v)

    if args.per_minor:
        # 每个 major.minor 只留最新 patch 的表，其余发布都指过去 —— 表是按 V8 家族定的，
        # 同一 minor 内 patch 之间有差异的情况极少；这样内嵌体积从 90 张降到 36 张。
        newest = {}
        for v8 in fname_of_v8:
            m = ".".join(v8.split(".")[:2])
            cur = newest.get(m)
            if cur is None or [int(x) for x in v8.split(".")] > [int(x) for x in cur.split(".")]:
                newest[m] = v8
        # 已有的表（人工调过的老族表）原样保留，只在缺的 minor 上新增
        if args.keep_existing:
            for m, v8 in newest.items():
                cur = os.path.join(tables_dir, f"v{m.replace('.', '_')}.json")
                if os.path.exists(cur):
                    fname_of_v8[v8] = os.path.basename(cur)
        keep = {fname_of_v8[v8] for v8 in newest.values()}
        for v in uniq_versions:
            m = ".".join(v["v8"].split(".")[:2])
            m = TABLE_ALIAS.get(m, m)      # 格式别名优先（见 minor_table 处说明）
            v["table"] = fname_of_v8[newest[m]]
        dropped = [f for f in table_files if f not in keep]
        for f in dropped:
            del table_files[f]
        print(f"per-minor: {len(keep)} tables kept, {len(dropped)} dropped", file=sys.stderr)

    manifest = {
        "schema_version": 1,
        "generated_at": datetime.now(timezone.utc).isoformat(timespec="seconds"),
        "versions": uniq_versions,
    }
    with open(os.path.join(tables_dir, "manifest.json"), "w") as f:
        json.dump(manifest, f, indent=1, sort_keys=True)
    for fname, table in table_files.items():
        dest = os.path.join(tables_dir, fname)
        if args.keep_existing and os.path.exists(dest):
            continue      # 沿用已有的表（不重写，保持逐字节一致）
        with open(dest, "w") as f:
            json.dump(table, f, indent=1, sort_keys=True)

    # ro-map 内嵌：每个 V8 minor 收一份（取该 minor 里有 ro-map 的那个 node 发布）。
    # 只读堆地址（chunk/offset）是**按 V8 版本**编号的，同一 minor 内基本一致；
    # 内嵌后 Node 22+ 不必再手动 `--ro-map`。
    ro_files = {}
    if args.ro_maps:
        # 键用**精确 V8 版本**：只读堆地址 (chunk/offset) 是按 V8 版本编号的 —— 同一
        # minor 的不同 patch 都可能不同，错配会静默给出**错误的属性名**（比留占位更糟）。
        per_minor_pick = {}
        for e in index:
            v8 = e.get("v8") or ""
            if not v8 or v8 in per_minor_pick:
                continue
            # 该 V8 版本下的**任意**一个发布有本地 ro-map 就行（不能只看 index 里的第一条：
            # 第一条常常没生成过 map，那样整条线都退化成占位）
            cand = os.path.join(args.ro_maps, f"ro-map-{e['version'].lstrip('v')}.json")
            if not os.path.exists(cand):
                continue
            per_minor_pick[v8] = cand
        seen_ro = {}
        for v8str, path in sorted(per_minor_pick.items()):
            text = open(path, encoding="utf-8").read()
            h = hashlib.sha1(text.encode()).hexdigest()
            if h in seen_ro:
                ro_files[v8str] = seen_ro[h]
                continue
            fname = f"ro_map_{v8str.replace('.', '_')}.json"
            seen_ro[h] = fname
            ro_files[v8str] = fname
            with open(os.path.join(tables_dir, fname), "w", encoding="utf-8") as f:
                f.write(text)
        print(f"ro-maps: {len(ro_files)} V8 versions", file=sys.stderr)

    # 生成 Rust 内嵌清单
    lines = ["// 本文件由 scripts/codegen.py 生成，请勿手改。"]
    lines.append(f"pub static MANIFEST_JSON: &str = include_str!(\"../tables/manifest.json\");")
    lines.append("pub static EMBEDDED_TABLE_FILES: &[(&str, &str)] = &[")
    for fname in sorted(table_files):
        lines.append(f"    (\"{fname}\", include_str!(\"../tables/{fname}\")),")
    lines.append("];")
    with open(os.path.join(out_root, "src", "tables_embed.rs"), "w") as f:
        f.write("\n".join(lines) + "\n")
    if args.ro_maps:
        rlines = ["// 本文件由 scripts/codegen.py 生成，请勿手改。",
                  "pub static RO_MAPS: &[(&str, &str)] = &["]
        for mk in sorted(ro_files, key=lambda s: tuple(int(x) for x in s.split("."))):
            rlines.append(
                f'    ("{mk}", include_str!("../tables/{ro_files[mk]}")),')
        rlines.append("];")
        with open(os.path.join(out_root, "src", "ro_embed.rs"), "w") as f:
            f.write("\n".join(rlines) + "\n")

    print(f"tables: {len(table_files)} files, {len(uniq_versions)} hash entries")


if __name__ == "__main__":
    main()
