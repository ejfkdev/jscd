#!/usr/bin/env python3
"""jscd 版本表生成器：从 Node 源码树按 tag 提取 V8 解析所需数据 → tables/*.json。

数据源：/Users/e/Documents/github/node（浅克隆，需先
  git fetch --depth 1 origin tag v16.20.2 ...）
产出：
  tables/manifest.json     版本索引（90 个 Node↔V8 组合的 version_hash）+ 表别名（去重）
  tables/v<X>_<Y>.json     单个 V8 major.minor 的解析表（内容相同的版本别名复用同一文件）
  src/tables_embed.rs      Rust 内嵌清单（include_str!）

用法:
  python3 scripts/codegen.py --node /Users/e/Documents/github/node \
      --tag v16.20.2 [--tag v18.20.8 ...]
"""
import argparse
import hashlib
import json
import os
import re
import subprocess
import sys
from datetime import datetime, timezone

NODE = None


def git_show(tag, path):
    """path 相对 deps/v8/（如 src/interpreter/bytecodes.h、include/v8-version.h）。"""
    r = subprocess.run(["git", "-C", NODE, "show", f"{tag}:deps/v8/{path}"],
                       capture_output=True, text=True)
    if r.returncode != 0:
        return None
    return r.stdout


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

def extract_serialization_tags(text):
    """提取 SerializationTag::Bytecode 枚举（显式值 + 隐式递增 + range 常量）。"""
    m = re.search(r"enum Bytecode : byte \{(.*?)\n  \};", text, re.S)
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
    m = re.search(rf"#define {name}\((\w+|V(?:, \w+)*)\)(.*?)(?=\n#define |\Z)", text, re.S)
    return m.group(2) if m else None


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
        if ident == "V":
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
            })
        else:
            # 子列表（如 BYTECODE_LIST_WITH_UNIQUE_HANDLERS(V) / SHORT_STAR_BYTECODE_LIST(V)）
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

def extract_roots(text, symbols_text=None, defs_text=None):
    """解析 roots.h 的 READ_ONLY + MUTABLE 根列表顺序，返回按索引排列的名字表。

    递归展开子列表宏；生成器宏按 adapter 命名。限制：TORQUE_DEFINED_MAP_ROOT_LIST
    是构建期由 torque 生成的（.tq 声明序），此处以 0 条占位——该段及其后的根索引
    不可靠，运行时对未知根走结构指纹分类（见 serializer.rs classify）。
    """
    sources = [text] + [s for s in (symbols_text, defs_text) if s]

    def grab(name):
        for s in sources:
            m = re.search(rf"#define {name}\b(.*?)(?=\n#define |\Z)", s, re.S)
            if m:
                return m.group(1)
        return None

    order = []

    def split_top(inner):
        args, depth, cur = [], 0, ""
        for ch in inner:
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
        elif gen == "STRUCT_LIST_GENERATOR":
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
            if ident == "V":
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
    # torque 生成段（TORQUE_DEFINED_MAP_ROOT_LIST）起点之后的索引不可静态复现，
    # 直接截断：调用方对超出范围的 Root 引用走结构指纹分类，避免错分类。
    if "__torque_map_list_unresolved__" in order:
        cutoff = order.index("__torque_map_list_unresolved__")
        order = order[:cutoff]
    return order


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
    expr = expr.replace("ALIGN8", f"({{}})")
    # 简单四则 + 已知常量替换
    def repl(m):
        return str(consts.get(m.group(1), 0))
    expr = re.sub(r"k\w+", repl, expr)
    expr = re.sub(r"ALIGN8\(([^)]*)\)", r"(((\1)+7)&~7)", expr)
    return int(eval(expr))  # noqa: S307 受控输入（V8 源码常量表达式）


# 官方 Node 构建的 V8 条件开关（影响 runtime/intrinsic 枚举条目数）。
# 依据：common.gypi / node.gypi 的 v8_enable_* 设置 + V8 的 dev-only flag 惯例。
BUILD_DEFINES = {
    "V8_TRACE_UNOPTIMIZED": False,      # dev-only，release 构建不定义
    "V8_TRACE_FEEDBACK_UPDATES": False, # dev-only
    "V8_INTL_SUPPORT": True,            # Node 默认 full-icu
    "V8_ENABLE_WEBASSEMBLY": True,      # Node 默认启用
}


def extract_runtime_names(text):
    """提取 Runtime::FunctionId 的顺序名表（id → name）。

    FunctionId = FOR_EACH_INTRINSIC(F) + FOR_EACH_INLINE_INTRINSIC(I)；
    前者 = RETURN_PAIR_IMPL + RETURN_OBJECT_IMPL，逐域拼接。条件域按 BUILD_DEFINES
    取舍（如 TRACE_UNOPTIMIZED/TRACE_FEEDBACK 在官方 release 构建中为空）。
    """
    if text is None:
        return []

    def macro_body(name):
        m = re.search(rf"#define {name}\(F, I\)(.*?)(?=\n#define |\Z)", text, re.S)
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
                    out.append(name)
            elif ident.startswith("IF_"):
                # IF_WASM(FOR_EACH_INTRINSIC_WASM, F, I) 之类：按开关决定是否展开
                flag = ident
                enabled = {
                    "IF_WASM": BUILD_DEFINES["V8_ENABLE_WEBASSEMBLY"],
                    "IF_TSA": True,
                }.get(flag, True)
                if enabled:
                    parts = [p.strip() for p in inner.split(",")]
                    if parts and parts[0].startswith("FOR_EACH_"):
                        expand(parts[0], depth + 1)
            elif ident.startswith("FOR_EACH_"):
                expand(ident, depth + 1)

    expand("FOR_EACH_INTRINSIC_RETURN_PAIR_IMPL")
    expand("FOR_EACH_INTRINSIC_RETURN_OBJECT_IMPL")
    return out


def extract_intrinsic_names(text):
    """INTRINSICS_LIST 顺序名表（IntrinsicId → 反汇编显示的 [_Name]）。"""
    if text is None:
        return []
    m = re.search(r"#define INTRINSICS_LIST\(V\)(.*?)(?=\n#define |\Z)", text, re.S)
    if not m:
        return []
    body = m.group(1).replace("\\\n", " ")
    return re.findall(r"V\(\s*([A-Za-z0-9_]+)\s*,", body)


def extract_hash_fold(tag):
    """V8 ≥ 12 引入 base::Hasher（左折叠）；此前为变参递归（右折叠）。"""
    fh = git_show(tag, "src/base/functional.h")
    if fh and "class Hasher" in fh and "hash_value_unsigned_impl" in fh:
        return "left_fold"
    return "right_fold"


def extract_tagged_size(tag):
    """kTaggedSize：看 build 配置里指针压缩是否对该平台开启。

    code cache 与平台绑定；Node 官方构建：linux/win x64 开压缩(4)，macOS 不开(8)。
    这里从 node 的 common.gypi 读默认值并按平台覆写。
    """
    r = subprocess.run(["git", "-C", NODE, "show", f"{tag}:node.gypi"],
                       capture_output=True, text=True)
    gypi = r.stdout if r.returncode == 0 else ""
    r2 = subprocess.run(["git", "-C", NODE, "show", f"{tag}:common.gypi"],
                        capture_output=True, text=True)
    gypi += r2.stdout if r2.returncode == 0 else ""
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


def main():
    global NODE
    ap = argparse.ArgumentParser()
    ap.add_argument("--node", required=True)
    ap.add_argument("--tag", action="append", required=True,
                    help="node tag，可多次；建议每 major 取最早/最新")
    ap.add_argument("--index", default=None,
                    help="nodejs.org/dist/index.json 缓存文件（用于补全 hash 索引）")
    ap.add_argument("--out", default=None, help="输出目录（默认仓库根）")
    args = ap.parse_args()
    NODE = args.node
    out_root = args.out or os.path.join(os.path.dirname(__file__), "..")
    tables_dir = os.path.join(out_root, "tables")
    os.makedirs(tables_dir, exist_ok=True)

    index_by_tag = {}
    if args.index and os.path.exists(args.index):
        for e in json.load(open(args.index)):
            index_by_tag.setdefault(e["version"], []).append(e)

    versions = []       # manifest 条目
    table_files = {}    # file -> table dict
    file_of_content = {}  # content-hash -> filename（去重）

    for tag in sorted(args.tag):
        vv = parse_v8_version(tag)
        if vv is None:
            print(f"skip {tag}: no v8-version.h", file=sys.stderr)
            continue
        maj, minor, build, patch = vv
        key = f"{maj}_{minor}"

        src_b = git_show(tag, "src/interpreter/bytecodes.h")
        src_o = git_show(tag, "src/interpreter/bytecode-operands.h")
        src_t = git_show(tag, "src/snapshot/serializer-deserializer.h")
        src_r = git_show(tag, "src/roots/roots.h")
        src_s = git_show(tag, "src/init/heap-symbols.h")
        src_d = git_show(tag, "src/objects/objects-definitions.h")
        src_rt = git_show(tag, "src/runtime/runtime.h")
        src_intr = git_show(tag, "src/interpreter/interpreter-intrinsics.h")
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
                "tags": extract_serialization_tags(src_t),
            },
            "roots": extract_roots(src_r, src_s, src_d) if src_r else [],
            "runtime_names": extract_runtime_names(src_rt),
            "intrinsic_names": extract_intrinsic_names(src_intr),
        }

        content_key = hashlib.sha1(json.dumps(table, sort_keys=True).encode()).hexdigest()
        if content_key not in file_of_content:
            fname = f"v{key}.json"
            if fname in table_files:
                fname = f"v{key}_{tag.lstrip('v')}.json"
            file_of_content[content_key] = fname
            table_files[fname] = table
        fname = file_of_content[content_key]

        # 该 tag 对应的 Node 版本及其 patch 序列都登记进 hash 索引
        entries = index_by_tag.get(tag) or [{"version": tag.lstrip("v"), "v8": table["v8"]}]
        for e in entries:
            e_v8 = e.get("v8") or table["v8"]
            p = [int(x) for x in e_v8.split(".")]
            versions.append({
                "v8": e_v8,
                "hash": v8_hash(table["hash"]["algorithm"], *p),
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

    manifest = {
        "schema_version": 1,
        "generated_at": datetime.now(timezone.utc).isoformat(timespec="seconds"),
        "versions": uniq_versions,
    }
    with open(os.path.join(tables_dir, "manifest.json"), "w") as f:
        json.dump(manifest, f, indent=1, sort_keys=True)
    for fname, table in table_files.items():
        with open(os.path.join(tables_dir, fname), "w") as f:
            json.dump(table, f, indent=1, sort_keys=True)

    # 生成 Rust 内嵌清单
    lines = ["// 本文件由 scripts/codegen.py 生成，请勿手改。"]
    lines.append(f"pub static MANIFEST_JSON: &str = include_str!(\"../tables/manifest.json\");")
    lines.append("pub static EMBEDDED_TABLE_FILES: &[(&str, &str)] = &[")
    for fname in sorted(table_files):
        lines.append(f"    (\"{fname}\", include_str!(\"../tables/{fname}\")),")
    lines.append("];")
    with open(os.path.join(out_root, "src", "tables_embed.rs"), "w") as f:
        f.write("\n".join(lines) + "\n")

    print(f"tables: {len(table_files)} files, {len(uniq_versions)} hash entries")


if __name__ == "__main__":
    main()
