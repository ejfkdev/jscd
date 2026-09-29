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
    # 类型拼写随版本变：V8 ≤ 11.x 用 ": byte"，12.x+ 用 ": uint8_t"
    m = re.search(r"enum Bytecode : (?:byte|uint8_t|uint8) \{(.*?)\n  \};", text, re.S)
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
        return None
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
    return {
        "flags_smi": flags_smi,
        "position_info_early": early_position,
        "max_inlined_names": max_inlined if has_table else (1 << 30),
        "saved_class_bit": 10,   # flags 位域位置（9.x–13.x 一致）
        "function_variable_bits": [12, 13],
        "receiver_bits": [7, 8],
        "has_inferred_bit": 14,
    }


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
        # BytecodeArray 定义位置随版本移动：9.x–11.x 在 objects/code.tq，12.x+ 在 objects/bytecode-array.tq
        src_code = git_show(tag, "src/objects/bytecode-array.tq") or git_show(tag, "src/objects/code.tq")
        src_trusted = git_show(tag, "src/objects/trusted-object.tq")
        src_fixed = git_show(tag, "src/objects/fixed-array.tq")
        src_frame = git_show(tag, "src/execution/frame-constants.h")
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
                "tags": extract_serialization_tags(src_t),
            },
            "roots": extract_roots(src_r, src_s, src_d) if src_r else [],
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
