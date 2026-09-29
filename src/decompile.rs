//! 反编译器：字节码 → 伪 JS（语法合法，尽量可运行）。
//!
//! 流水线（单遍、按需、低内存）：
//! ```text
//! bytecode ──Decoder──▶ Instr[] ──┬─ 表达式重建（acc/寄存器状态机）
//!                                 ├─ 控制流结构化（if/while/do/for/switch/break/continue/try）
//!                                 └─ 语句打印（AST 形态，括号/花括号由构造保证配平）
//! ```
//!
//! 设计约束：
//! - **输出必为合法 JS**：所有语句都经 `emit_*` 生成，块结构由递归范围发射保证；未知 opcode
//!   退化为注释而不是垃圾代码。
//! - **低内存**：不克隆字节码/字符串，逐函数生成并写出（`render_all` 流式写）。
//! - 名称来源：参数 → `aN`；context 槽 → ScopeInfo 里的真实变量名；全局 → 常量池字符串；
//!   栈寄存器 → `rN`（V8 未保留名字，属物理上限）。

use crate::bytecode::{Decoder, FamilyLayout, Instr, Operand};
use crate::disasm::Disassembler;
use crate::serializer::{CodeCache, Elem, ObjId, Ref, SlotValue};
use crate::tables::VersionTable;
use std::collections::HashMap;
use std::fmt::Write as FmtWrite;
use std::io::Write;

// ─────────────────────────────── 表达式 ───────────────────────────────

#[derive(Debug, Clone)]
pub enum Expr {
    Num(f64),
    BigInt(String),
    Str(String),
    Bool(bool),
    Null,
    Undefined,
    /// 标识符（变量/参数/全局名）
    Ident(String),
    /// 未识别值（注释形式，保证仍可解析）
    Hole,
    Reg(u32),
    Member {
        obj: Box<Expr>,
        key: Key,
    },
    Call {
        callee: Box<Expr>,
        args: Vec<Expr>,
        is_new: bool,
        spread_arg: Option<Box<Expr>>,
    },
    Bin {
        op: &'static str,
        l: Box<Expr>,
        r: Box<Expr>,
    },
    Un {
        op: &'static str,
        e: Box<Expr>,
        postfix: bool,
    },
    Assign {
        target: Box<Expr>,
        value: Box<Expr>,
        op: &'static str,
    },
    Seq(Vec<Expr>),
    Await(Box<Expr>),
    Yield(Box<Expr>),
    Spread(Box<Expr>),
    /// 内部标记：需要物化成临时变量
    Temp(String),
    /// 对象字面量（键已渲染为合法 JS 键，值已求好）
    ObjectLit(Vec<(String, Expr)>),
    /// 数组字面量
    ArrayLit(Vec<Expr>),
}

#[derive(Debug, Clone)]
pub enum Key {
    Ident(String),
    Str(String),
    Num(f64),
    Index(u32),
    /// 计算属性（任意表达式）
    Computed(Box<Expr>),
}

impl Key {
    /// 渲染（标识符用 `.k`，其余用 `[...]`）。
    pub fn render(&self) -> String {
        match self {
            Key::Ident(n) => n.clone(),
            Key::Str(s) => js_string(s),
            Key::Num(n) => format!("{n}"),
            Key::Index(i) => format!("r{i}"),
            Key::Computed(e) => e.render(),
        }
    }
}

impl Expr {
    /// 是否有副作用（用于决定要不要单独成句）。
    pub fn has_effect(&self) -> bool {
        match self {
            Expr::Call { .. } => true,
            Expr::Assign { .. } => true,
            Expr::Await(_) | Expr::Yield(_) => true,
            Expr::Bin { l, r, .. } => l.has_effect() || r.has_effect(),
            Expr::Un { e, .. } => e.has_effect(),
            Expr::Member { obj, .. } => obj.has_effect(),
            Expr::Seq(xs) => xs.iter().any(|x| x.has_effect()),
            Expr::Spread(e) => e.has_effect(),
            Expr::ObjectLit(kv) => kv.iter().any(|(_, v)| v.has_effect()),
            Expr::ArrayLit(xs) => xs.iter().any(|x| x.has_effect()),
            _ => false,
        }
    }

    fn render(&self) -> String {
        let mut s = String::new();
        self.write_to(&mut s, 0);
        s
    }

    /// 按优先级加括号渲染（保证语义不因省略括号而改变）。
    fn write_to(&self, out: &mut String, parent_prec: u8) {
        let prec = self.prec();
        let need = prec < parent_prec;
        if need {
            out.push('(');
        }
        match self {
            Expr::Num(v) => {
                if v.fract() == 0.0 && v.abs() < 1e15 {
                    let _ = write!(out, "{}", *v as i64);
                } else {
                    let _ = write!(out, "{v}");
                }
            }
            Expr::BigInt(s) => {
                let _ = write!(out, "{s}n");
            }
            Expr::Str(s) => {
                let _ = write!(out, "{}", js_string(s));
            }
            Expr::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            Expr::Null => out.push_str("null"),
            Expr::Undefined => out.push_str("undefined"),
            // 防御：空标识符会产出非法代码 → 退化为 undefined
            Expr::Ident(n) | Expr::Temp(n) => {
                if n.is_empty() {
                    out.push_str("undefined");
                } else {
                    out.push_str(n);
                }
            }
            Expr::Hole => out.push_str("undefined /* hole */"),
            Expr::Reg(i) => {
                let _ = write!(out, "r{i}");
            }
            Expr::Member { obj, key } => {
                // 数字字面量的成员访问必须加括号：`0.x` 非法、`(0).x` 合法
                if matches!(**obj, Expr::Num(_)) {
                    out.push('(');
                    obj.write_to(out, 0);
                    out.push(')');
                } else {
                    obj.write_to(out, prec + 1);
                }
                match key {
                    Key::Ident(n) => {
                        let _ = write!(out, ".{n}");
                    }
                    Key::Str(s) => {
                        let _ = write!(out, "[{}]", js_string(s));
                    }
                    Key::Num(n) => {
                        let _ = write!(out, "[{n}]");
                    }
                    Key::Index(i) => {
                        let _ = write!(out, "[r{i}]");
                    }
                    Key::Computed(e) => {
                        out.push('[');
                        e.write_to(out, 0);
                        out.push(']');
                    }
                }
            }
            Expr::Call {
                callee,
                args,
                is_new,
                spread_arg,
            } => {
                if *is_new {
                    out.push_str("new ");
                }
                callee.write_to(out, prec + 1);
                out.push('(');
                let mut first = true;
                for a in args {
                    if !first {
                        out.push_str(", ");
                    }
                    first = false;
                    a.write_to(out, 0);
                }
                // 展开实参语义上是最后一个实参（V8 的寄存器组把 spread 放在末尾）
                if let Some(sp) = spread_arg {
                    if !first {
                        out.push_str(", ");
                    }
                    let _ = write!(out, "...{}", sp.render());
                }
                out.push(')');
            }
            Expr::Bin { op, l, r } => {
                l.write_to(out, prec);
                let _ = write!(out, " {op} ");
                r.write_to(out, prec + 1);
            }
            Expr::Un { op, e, postfix } => {
                if *postfix {
                    e.write_to(out, prec + 1);
                    out.push_str(op);
                } else if *op == "!" {
                    // 取反化简：!(!(x)) → x；!(a === b) → a !== b
                    match &**e {
                        Expr::Un { op: "!", e: inner, postfix: false } => inner.write_to(out, parent_prec),
                        Expr::Bin { op: cmp, l, r }
                            if matches!(*cmp, "===" | "!==" | "==" | "!=") =>
                        {
                            let inv = match *cmp {
                                "===" => "!==",
                                "!==" => "===",
                                "==" => "!=",
                                _ => "==",
                            };
                            l.write_to(out, prec);
                            let _ = write!(out, " {inv} ");
                            r.write_to(out, prec + 1);
                        }
                        other => {
                            out.push('!');
                            other.write_to(out, prec);
                        }
                    }
                } else {
                    out.push_str(op);
                    e.write_to(out, prec);
                }
            }
            Expr::Assign { target, value, op } => {
                target.write_to(out, 3);
                let _ = write!(out, " {op} ");
                value.write_to(out, 1);
            }
            Expr::Seq(xs) => {
                for (i, x) in xs.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    x.write_to(out, 0);
                }
            }
            Expr::Await(e) => {
                out.push_str("await ");
                e.write_to(out, prec);
            }
            Expr::Yield(e) => {
                out.push_str("yield ");
                e.write_to(out, prec);
            }
            Expr::Spread(e) => {
                out.push_str("...");
                e.write_to(out, 0);
            }
            Expr::ObjectLit(kv) => {
                out.push_str("{ ");
                for (i, (k, v)) in kv.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    if k.is_empty() {
                        // 展开属性 `...v`
                        out.push_str("...");
                        v.write_to(out, 0);
                    } else {
                        out.push_str(k);
                        out.push_str(": ");
                        v.write_to(out, 0);
                    }
                }
                out.push_str(" }");
            }
            Expr::ArrayLit(xs) => {
                out.push('[');
                for (i, x) in xs.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    x.write_to(out, 0);
                }
                out.push(']');
            }
        }
        if need {
            out.push(')');
        }
    }

    /// 优先级（数字越大结合越紧）——用于最小括号插入。
    fn prec(&self) -> u8 {
        match self {
            Expr::Seq(_) => 1,
            Expr::Yield(_) => 2,
            Expr::Assign { .. } => 3,
            Expr::Await(_) | Expr::Bin { op: "||", .. } => 4,
            Expr::Bin { op, .. } => match *op {
                "??" => 4,
                "&&" => 5,
                "|" => 6,
                "^" => 7,
                "&" => 8,
                "==" | "!=" | "===" | "!==" | "in" | "instanceof" => 9,
                "<" | ">" | "<=" | ">=" => 10,
                "<<" | ">>" | ">>>" => 11,
                "+" | "-" => 12,
                "*" | "/" | "%" => 13,
                "**" => 14,
                _ => 12,
            },
            Expr::Un { postfix, .. } => {
                if *postfix {
                    15
                } else {
                    14
                }
            }
            Expr::Call { .. } => 17,
            Expr::Member { .. } => 18,
            Expr::ObjectLit(_) | Expr::ArrayLit(_) => 19,
            _ => 20,
        }
    }
}

/// JS 字符串字面量（转义保证可解析）。
pub fn js_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{2028}' => out.push_str("\\u2028"),
            '\u{2029}' => out.push_str("\\u2029"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// JS 保留字（不能作变量名；但可作属性名）。
const RESERVED: &[&str] = &[
    "break", "case", "catch", "class", "const", "continue", "debugger", "default", "delete",
    "do", "else", "enum", "export", "extends", "false", "finally", "for", "function", "if",
    "import", "in", "instanceof", "new", "null", "return", "super", "switch", "this", "throw",
    "true", "try", "typeof", "var", "void", "while", "with", "yield", "let", "static",
    "implements", "interface", "package", "private", "protected", "public", "await", "arguments",
    "eval",
];

/// 变量名安全化：非法标识符或保留字加后缀（保证是合法绑定名）。
pub fn sanitize_var(s: &str) -> String {
    if RESERVED.contains(&s) {
        return format!("{s}_");
    }
    if is_ident(s) {
        s.to_string()
    } else {
        sanitize_ident(s)
    }
}

/// 合法标识符 → 可安全用作 `.name`（保留字也允许，ES5+ 属性名可用）。
fn is_ident(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' || c == '$' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
}

// ─────────────────────────────── 作用域信息 ───────────────────────────────

/// 从 ScopeInfo 读出的函数级信息（变量名/参数/函数种类）。
#[derive(Debug, Default, Clone)]
pub struct Scope {
    pub context_locals: Vec<String>,
    /// 局部名过多（≥ kScopeInfoMaxInlinedLocalNamesSize）时的 NameToIndexHashTable
    pub locals_table: Option<ObjId>,
    /// 外层作用域的 ScopeInfo 对象（用于跨作用域解析 context 槽名）
    pub outer: Option<ObjId>,
    pub param_count: u32,
    pub name: String,
    pub inferred_name: String,
    pub flags: u64,
    /// FunctionKind 原始值（flags bits 17..21）
    pub function_kind: u32,
}

impl Scope {
    pub fn is_arrow(&self) -> bool {
        // FunctionKind: kArrowFunction=3, kAsyncArrowFunction=4（见 V8 function-kind.h）
        matches!(self.function_kind, 3 | 4)
    }
    pub fn is_async(&self) -> bool {
        matches!(self.function_kind, 2 | 4 | 6 | 8 | 10)
    }
    pub fn is_generator(&self) -> bool {
        matches!(self.function_kind, 5 | 6 | 7 | 8 | 9 | 10)
    }
    pub fn is_class_constructor(&self) -> bool {
        matches!(self.function_kind, 11..=13)
    }
}

/// 读取 ScopeInfo（布局随版本变，见 docs/VERSIONS.md §7）。
pub fn read_scope<'a>(
    cache: &CodeCache<'a>,
    table: &VersionTable,
    ts: usize,
    scope_id: ObjId,
) -> Option<Scope> {
    let cfg = table.scope_info.clone()?;
    // flags：Smi 编码（值在高 32 位）或裸 uint32（13.x）
    let flags = if cfg.flags_smi {
        cache.raw_at_ts(scope_id, ts, ts, ts).and_then(crate::serializer::decode_smi_bytes)? as u64
    } else {
        u32::from_le_bytes(cache.raw_at_ts(scope_id, ts, 4, ts)?.try_into().ok()?) as u64
    };
    // 9.x–12.x：flags(Smi)@ts | param_count@2ts | context_local_count@3ts
    // 13.x：    flags(u32)+padding@ts..ts+8 | param_count@ts+8 | context_local_count@ts+16
    let (param_off, clc_off) = if cfg.position_info_early {
        (ts + 8, ts + 16)
    } else {
        (2 * ts, 3 * ts)
    };
    let param_count = cache
        .raw_at_ts(scope_id, param_off, ts, ts)
        .and_then(crate::serializer::decode_smi_bytes)
        .unwrap_or(0) as u32;
    let n = cache
        .raw_at_ts(scope_id, clc_off, ts, ts)
        .and_then(crate::serializer::decode_smi_bytes)
        .unwrap_or(0) as usize;

    // 变量区起点（字节）
    let mut off = clc_off + ts;
    let _ = &mut off;
    if cfg.position_info_early {
        off += 2 * ts; // position_info
        let scope_type = flags & 0xF;
        if scope_type == 3 {
            off += ts; // module_variable_count
        }
    }
    let inlined = n < cfg.max_inlined_names;
    let names_off = off;
    let mut locals_table = None;
    if !inlined {
        // 名字在 NameToIndexHashTable 里（键=名字，值=context 槽号）
        if let Some(Ref::Object(t)) = cache.slot_at(scope_id, off / ts).and_then(|v| v.as_ref()) {
            locals_table = Some(t);
        }
        off += ts;
    } else {
        off += n * ts;
    }

    let mut scope = Scope {
        locals_table,
        param_count,
        flags,
        function_kind: ((flags >> 17) & 0x1F) as u32,
        ..Default::default()
    };
    if inlined {
        for i in 0..n {
            let slot = (names_off / ts) + i;
            match cache.slot_at(scope_id, slot).and_then(|v| v.as_ref()) {
                Some(r) => scope
                    .context_locals
                    .push(crate::disasm::name_of_ref(cache, table, r).unwrap_or_default()),
                None => scope.context_locals.push(String::new()),
            }
        }
    }

    // 函数名 / 推断名
    let has_function_var =
        ((flags >> cfg.function_variable_bits[0]) & 0b11) != 0;
    let has_inferred = (flags >> cfg.has_inferred_bit) & 1 == 1;
    let has_saved = (flags >> cfg.saved_class_bit) & 1 == 1;
    let receiver_var = (flags >> cfg.receiver_bits[0]) & 0b11;
    let has_receiver = matches!(receiver_var, 1 | 2);
    let mut cursor = off + n * ts; // infos[n]
    if has_saved {
        cursor += ts;
    }
    if has_receiver {
        cursor += ts;
    }
    if has_function_var {
        if let Some(r) = cache.slot_at(scope_id, cursor / ts).and_then(|v| v.as_ref()) {
            scope.name = crate::disasm::name_of_ref(cache, table, r).unwrap_or_default();
        }
        cursor += 2 * ts;
    }
    if has_inferred {
        if let Some(r) = cache.slot_at(scope_id, cursor / ts).and_then(|v| v.as_ref()) {
            scope.inferred_name = crate::disasm::name_of_ref(cache, table, r).unwrap_or_default();
        }
        cursor += ts;
    }
    // outer_scope_info（若存在）：跨作用域解析变量名
    // 9.x–12.x：inferred_function_name 之后还有 position_info(2 槽)；
    // 13.x：position_info 已前置（见上）
    if !cfg.position_info_early {
        cursor += 2 * ts;
    }
    let has_outer = (flags >> 22) & 1 == 1;
    if has_outer {
        if let Some(Ref::Object(o)) = cache.slot_at(scope_id, cursor / ts).and_then(|v| v.as_ref()) {
            if cache.obj(o).ty.is(table, "ScopeInfo") {
                scope.outer = Some(o);
            }
        }
    }
    Some(scope)
}

// ─────────────────────────────── 反编译器 ───────────────────────────────

/// 只读堆引用名表（chunk/offset → 名称）。
/// 字符串内容在 V8 的快照里而非 .jsc 内，靠"探针编译"预先提取（见 scripts/build_ro_map.sh）。
#[derive(Debug, Default, Clone)]
pub struct RoMap {
    pub entries: HashMap<String, String>,
    pub v8: String,
}

impl RoMap {
    pub fn load(path: &std::path::Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let v: serde_json::Value =
            serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        let mut entries = HashMap::new();
        if let Some(obj) = v.get("entries").and_then(|e| e.as_object()) {
            for (k, val) in obj {
                if let Some(s) = val.as_str() {
                    entries.insert(k.clone(), s.to_string());
                }
            }
        }
        Ok(RoMap {
            entries,
            v8: v
                .get("v8")
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .to_string(),
        })
    }

    pub fn get(&self, chunk: u32, offset: u32) -> Option<&str> {
        self.entries
            .get(&format!("{chunk}/{offset}"))
            .map(|s| s.as_str())
    }
}

pub struct Decompiler<'a> {
    cache: &'a CodeCache<'a>,
    table: &'a VersionTable,
    layout: FamilyLayout,
    dis: Disassembler<'a>,
    ts: usize,
    scope_cache: std::cell::RefCell<HashMap<ObjId, Scope>>,
    /// 只读堆名表（可选）
    ro_map: Option<RoMap>,
    /// 字节码 → (读 acc, 写 acc)：来自版本表（codegen 从 bytecodes.h 提取）
    acc_use: HashMap<String, (bool, bool)>,
}

/// 循环上下文（break/continue 目标）。
#[derive(Debug, Clone, Copy)]
struct LoopCtx {
    /// continue 跳转目标（= 循环判断处）
    continue_target: usize,
    /// break 跳转目标（= 循环结束）
    break_target: usize,
}

struct FnCtx<'a, 'b> {
    d: &'b Decompiler<'a>,
    sfi: ObjId,
    bca: ObjId,
    instrs: Vec<Instr>,
    idx_of: HashMap<usize, usize>,
    pool: Option<ObjId>,
    scope: Option<Scope>,
    scope_id: Option<ObjId>,
    /// 寄存器（下标 = V8 寄存器索引 >= 0）
    regs: Vec<Option<Expr>>,
    acc: Option<Expr>,
    /// acc 的当前值是否已落进寄存器（Star 后为真）→ 死 acc 无需重复求值
    acc_stored: bool,
    /// 落进的是哪个寄存器（物化 phi 时直接引用它，避免表达式在寄存器被改写后重算失真）
    acc_stored_reg: Option<u32>,
    /// 上一条已输出的语句文本（折叠完全重复的无副作用语句）
    last_line: String,
    /// 函数体文本（`finish_body` 用：脚本顶层代码需要原样铺开）
    body: String,
    /// true = 只出函数体（脚本/模块顶层）
    inline_body: bool,
    /// 最近一次 context 槽读取的槽号（供紧随其后的 TDZ 检查反推变量名）
    last_ctx_slot: Option<usize>,
    /// 槽号 → 变量名（由 TDZ 检查常量池反推，弥补外层 ScopeInfo 缺失）
    slot_aliases: HashMap<usize, String>,
    /// 已生成的语句
    out: String,
    indent: usize,
    loops: Vec<LoopCtx>,
    /// 已物化的临时变量（名字, 表达式）
    temps: Vec<(String, Expr)>,
    /// try/catch 的 handler 表
    handlers: Vec<Handler>,
    /// 需要声明的 context 名（按当前作用域顺序）
    ctx_names: Vec<String>,
    /// 块/catch 作用域栈（CreateCatchContext 等压入的 ScopeInfo）
    ctx_scopes: Vec<Option<ObjId>>,
    label_counter: usize,
    tmp_counter: usize,
    /// 分支间物化的累加器变量（phi）
    phi_vars: Vec<String>,
    /// 指令内吞掉后续区间时（如 switch 的 case 体）主循环下次的起点
    skip_to: Option<usize>,
    /// 已被 guard 子句就地发射的"冷块"区间（起点下标, 终点下标 exclusive）→ 线性扫描时跳过
    skip_spans: Vec<(usize, usize)>,
    name: String,
    is_async: bool,
    is_generator: bool,
}

#[derive(Debug, Clone, Copy)]
struct Handler {
    start: u32,
    end: u32,
    target: u32,
    /// 用于区分 catch 变量（handler 深度）
    depth: u32,
}

impl<'a> Decompiler<'a> {
    pub fn new(cache: &'a CodeCache<'a>, table: &'a VersionTable) -> Self {
        let layout = FamilyLayout::from_table(table);
        Decompiler {
            cache,
            table,
            layout,
            dis: Disassembler::new(cache, table, layout),
            ts: table.tagged_size as usize,
            scope_cache: std::cell::RefCell::new(HashMap::new()),
            ro_map: None,
            acc_use: table
                .bytecodes
                .iter()
                .filter(|b| !b.acc.is_empty())
                .map(|b| {
                    (
                        b.name.clone(),
                        (b.acc.contains('r'), b.acc.contains('w')),
                    )
                })
                .collect(),
        }
    }

    /// 注入只读堆名表（提升属性名可读性）。
    pub fn with_ro_map(mut self, m: Option<RoMap>) -> Self {
        self.ro_map = m;
        self
    }

    pub fn layout(&self) -> FamilyLayout {
        self.layout
    }

    /// 函数名 / function_data 槽位（供 CLI 的 ro-map 提取复用）。
    pub fn sfi_name(&self, sfi: ObjId) -> String {
        self.dis.sfi_name(sfi)
    }
    pub fn sfi_function_data_slots(&self) -> Vec<usize> {
        self.dis.sfi_function_data_slots()
    }
    pub fn bca_constant_pool_slot(&self) -> usize {
        self.dis.bca_constant_pool_slot()
    }

    /// 遍历全部已编译函数并流式写出（低内存：逐函数生成后立即写出）。
    /// 文件级占位声明。
    ///
    /// 语义说明：`__runtime_*` / `__intrinsic_*` 是 V8 的 C++ 内建（声明全局、类定义、
    /// 迭代器校验、对象展开…），码缓存里只有调用点，没有实现；`__anonymous` 是匿名
    /// 函数占位（模板标签、类定义等以常量池 SFI 形式出现）。这里给可运行的空实现，
    /// 让产物能直接跑起来（值可能是 undefined，逆向时按名字判断原意）。
    const STUBS: &'static str = r#"// @generated by jscd —— V8 code cache 里没有源码文本，以下是按字节码重建的伪 JS。
// __runtime/__intrinsic 是 V8 的 C++ 内建：码缓存里只有调用点、没有实现。
// 下面给最小可运行实现（类能 new、原型链正确、构造函数体真的跑；私有名变 Symbol；
// 迭代器校验会抛错），其余未知名退化成空实现。
var __runtime = new Proxy({
  DeclareGlobals: function () {},
  // DefineClass(boilerplate, ctor, parent, ...methods)：方法键在 boilerplate 里（形参看不到），
  // 但方法函数本身都在实参里、且带着自己的名字 → 按名字挂到原型上。
  // 这样 `this._read` 这类内部方法调用能真的走通（getter/setter 只能当普通方法近似）。
  DefineClass: function (bp, ctor, parent) {
    // V8 语义：本体就是传进来的那个构造函数（DefineClass 原地装配并返回它），
    // 调用点随后绑定的也是这个闭包 —— 所以这里必须原地改造，不能另造一个新函数。
    var Cls = ctor;
    if (parent) {
      Cls.prototype = Object.create(parent.prototype || Object.prototype);
      Object.setPrototypeOf(Cls, parent);
    }
    // 实参顺序：0=boilerplate 1=ctor 2=parent 3..=方法闭包
    for (var i = 3; i < arguments.length; i++) {
      var f = arguments[i];
      if (typeof f !== 'function' || !f.name) continue;
      // getter/setter：V8 给这类 SFI 起名 `get value` / `set value`，摊平后成了
      // `get_value` / `set_value` → 按后缀定义成访问器，`obj.value` 才取得到
      var m = /^(get|set)_([A-Za-z_$][A-Za-z0-9_$]*)$/.exec(f.name);
      if (m) {
        var d = Object.getOwnPropertyDescriptor(Cls.prototype, m[2]) || {};
        d[m[1]] = f;
        try { Object.defineProperty(Cls.prototype, m[2], d); } catch (e) {}
      } else {
        Cls.prototype[f.name] = f;
      }
      // 静态/实例分不清（种类在 boilerplate 里）→ 两边都挂
      Cls[f.name] = f;
    }
    return Cls;
  },
  CreatePrivateNameSymbol: function (d) { return typeof Symbol === 'function' ? Symbol(d) : d; },
  ThrowSymbolIteratorInvalid: function () { throw new TypeError('Invalid iterator'); },
  ThrowIteratorResultNotAnObject: function (v) { throw new TypeError('bad iterator result'); },
}, { get: function (t, k) { return k in t ? t[k] : function () {}; } });
var __intrinsic = new Proxy({}, { get: () => () => undefined });
var __context, __ctx = {};
function __anonymous() {}
var __uncompiled = new Proxy({}, { get: () => function () {} });

"#;

    pub fn render_all<W: FmtWrite>(&self, w: &mut W, filter: Option<&str>) -> Result<(), String> {
        // 前言：① 占位内建（V8 运行期内建/固有调用在 JS 层不可见，给空实现保证文件可运行）
        //       ② 把各作用域的 context 变量提升为文件级 var（摊平后靠共享绑定才可运行）
        let _ = w.write_str(Self::STUBS);
        let preamble = self.shared_bindings();
        if !preamble.is_empty() {
            let _ = w.write_str(&preamble);
        }
        for id in 0..self.cache.objects.len() {
            if !self.cache.obj(id).ty.is(self.table, "SharedFunctionInfo") {
                continue;
            }
            let name = self.dis.sfi_name(id);
            if let Some(f) = filter {
                if !name.contains(f) {
                    continue;
                }
            }
            let text = self.decompile_function(id)?;
            w.write_str(&text).map_err(|_| "write failed".to_string())?;
        }
        Ok(())
    }

    /// 所有作用域的 context 变量名（去重）→ 文件级声明。
    fn shared_bindings(&self) -> String {
        let mut names: Vec<String> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for id in 0..self.cache.objects.len() {
            if !self.cache.obj(id).ty.is(self.table, "SharedFunctionInfo") {
                continue;
            }
            let Some(sid) = self.scope_id_of(id) else {
                continue;
            };
            let Some(scope) = self.scope_by_id(sid) else {
                continue;
            };
            for n in &scope.context_locals {
                if n.is_empty() {
                    continue;
                }
                let v = sanitize_var(n);
                if seen.insert(v.clone()) {
                    names.push(v);
                }
            }
            // 顺着 outer 链一并收集
            let mut cur = scope.outer;
            for _ in 0..8 {
                let Some(next) = cur else { break };
                let Some(sc) = self.scope_by_id(next) else { break };
                for n in &sc.context_locals {
                    if n.is_empty() {
                        continue;
                    }
                    let v = sanitize_var(n);
                    if seen.insert(v.clone()) {
                        names.push(v);
                    }
                }
                cur = sc.outer;
            }
        }
        if names.is_empty() {
            return String::new();
        }
        format!(
            "// 共享绑定（闭包捕获的变量被摊平为文件级 var，便于直接运行）\nvar {};\n\n",
            names.join(", ")
        )
    }

    /// 反编译单个函数。
    pub fn decompile_function(&self, sfi: ObjId) -> Result<String, String> {
        let bca = self
            .dis
            .sfi_function_data_slots()
            .iter()
            .find_map(|slot| match self.cache.slot_at(sfi, *slot) {
                Some(SlotValue::Ref(Ref::Object(b)))
                    if self.cache.obj(*b).ty.is(self.table, "BytecodeArray") =>
                {
                    Some(*b)
                }
                _ => None,
            });
        let Some(bca) = bca else {
            // 未编译（UncompiledData）：只输出签名占位
            let name = self.dis.sfi_name(sfi);
            let fname = if name.is_empty() {
                "_anonymous".to_string()
            } else {
                sanitize_ident(&name)
            };
            return Ok(format!(
                "function {fname}() {{ /* 未编译（UncompiledData）：源码不在 code cache 中 */ }}\n\n"
            ));
        };
        let scope_id = self.scope_id_of(sfi);
        let scope = scope_id.and_then(|id| self.scope_by_id(id));
        if std::env::var("JSCD_DBG_SCOPE").is_ok() {
            eprintln!(
                "[scope] sfi={sfi} name={:?} locals={} table={:?} outer={:?} params={}",
                self.dis.sfi_name(sfi),
                scope.as_ref().map(|s| s.context_locals.len()).unwrap_or(0),
                scope.as_ref().and_then(|s| s.locals_table),
                scope.as_ref().and_then(|s| s.outer),
                scope.as_ref().map(|s| s.param_count).unwrap_or(0),
            );
        }
        let mut ctx = FnCtx::new(self, sfi, bca, scope, scope_id, self.dis.sfi_name(sfi))?;
        let inline = ctx.inline_body;
        ctx.run()?;
        Ok(if inline {
            format!("// ── 模块/脚本顶层代码 ─────────────────────────────\n{}", ctx.finish_body())
        } else {
            ctx.finish()
        })
    }

    /// 从 NameToIndexHashTable 里按槽号找变量名（大作用域用）。
    pub fn name_from_locals_table(&self, table_id: ObjId, want_index: usize) -> Option<String> {
        // HashTable: map, numberOfElements(Smi)@ts, numberOfDeleted(Smi)@2ts, capacity(Smi)@3ts,
        //            之后每项两槽 (key, value)；kElementsStartIndex = 3
        let n = self
            .cache
            .raw_at_ts(table_id, 2 * self.ts, self.ts, self.ts)
            .and_then(crate::serializer::decode_smi_bytes)
            .unwrap_or(0) as usize;
        let cap = self
            .cache
            .raw_at_ts(table_id, 3 * self.ts, self.ts, self.ts)
            .and_then(crate::serializer::decode_smi_bytes)
            .unwrap_or(0) as usize;
        // 元素区起点 = kElementsStartIndex(=3) * ts + 数据区；实际首项在 4*ts
        for e in 0..(n.min(cap).max(cap.min(256))) {
            let key_slot = 3 + 2 * e;
            let val_slot = key_slot + 1;
            let key = match self.cache.slot_at(table_id, key_slot).and_then(|v| v.as_ref()) {
                Some(r) => crate::disasm::name_of_ref(self.cache, self.table, r),
                None => None,
            };
            let val = self
                .cache
                .raw_at_ts(table_id, val_slot * self.ts, self.ts, self.ts)
                .and_then(crate::serializer::decode_smi_bytes);
            if let (Some(k), Some(v)) = (key, val) {
                if v as usize == want_index && !k.is_empty() {
                    return Some(k);
                }
            }
        }
        None
    }

    /// 沿 ScopeInfo 外层链查找 context 槽名（函数无自有上下文时，槽号属于外层作用域）。
    pub fn context_name_in_chain(&self, scope_id: Option<ObjId>, slot: usize) -> Option<String> {
        let mut cur = scope_id;
        for _ in 0..8 {
            let id = cur?;
            let scope = self.scope_by_id(id)?;
            let idx = slot.saturating_sub(2);
            if let Some(n) = scope.context_locals.get(idx) {
                if !n.is_empty() {
                    return Some(n.clone());
                }
            }
            if let Some(t) = scope.locals_table {
                if let Some(n) = self.name_from_locals_table(t, idx) {
                    return Some(n);
                }
            }
            cur = scope.outer;
        }
        None
    }

    fn scope_by_id(&self, scope_id: ObjId) -> Option<Scope> {
        if let Some(s) = self.scope_cache.borrow().get(&scope_id) {
            return Some(s.clone());
        }
        let s = read_scope(self.cache, self.table, self.ts, scope_id)?;
        self.scope_cache.borrow_mut().insert(scope_id, s.clone());
        Some(s)
    }

    /// SFI → ScopeInfo 对象 id。
    fn scope_id_of(&self, sfi: ObjId) -> Option<ObjId> {
        let name_slot = self
            .table
            .shared_function_info
            .as_ref()
            .and_then(|s| s.slot(self.ts, "name_or_scope_info"))
            .unwrap_or(2);
        match self.cache.slot_at(sfi, name_slot).and_then(|v| v.as_ref()) {
            Some(Ref::Object(o)) if self.cache.obj(o).ty.is(self.table, "ScopeInfo") => Some(o),
            _ => None,
        }
    }
}

fn sanitize_ident(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for (i, c) in s.chars().enumerate() {
        let ok = if i == 0 {
            c.is_ascii_alphabetic() || c == '_' || c == '$'
        } else {
            c.is_ascii_alphanumeric() || c == '_' || c == '$'
        };
        out.push(if ok { c } else { '_' });
    }
    if out.is_empty() {
        out.push_str("_anon");
    }
    out
}

impl<'a, 'b> FnCtx<'a, 'b> {
    fn new(
        d: &'b Decompiler<'a>,
        sfi: ObjId,
        bca: ObjId,
        scope: Option<Scope>,
    scope_id: Option<ObjId>,
        name: String,
    ) -> Result<Self, String> {
        let ba = d
            .table
            .bytecode_array
            .clone()
            .unwrap_or_else(|| crate::tables::BytecodeArrayLayout::fallback(d.ts));
        let ts = d.ts;
        let bytecode_len = d
            .cache
            .raw_at_ts(bca, ts, ts, ts)
            .and_then(crate::serializer::decode_smi_bytes)
            .unwrap_or(0) as usize;
        let code = d
            .cache
            .raw_at_ts(bca, ba.header_size, bytecode_len, ts)
            .unwrap_or(&[]);
        let parameter_size = ba
            .off("parameter_size")
            .and_then(|o| d.cache.raw_at_ts(bca, o, 4, ts))
            .map(|x| u32::from_le_bytes(x.try_into().unwrap()))
            .unwrap_or(0);
        let param_count = if d.dis.parameter_count_direct() {
            parameter_size & 0xFFFF
        } else {
            parameter_size / 8
        };
        let decoder = Decoder::new(d.table, d.layout, param_count as i32);
        let instrs = decoder.decode(code).map_err(|e| format!("decode bytecode: {e}"))?;
        let idx_of: HashMap<usize, usize> = instrs
            .iter()
            .enumerate()
            .map(|(i, ins)| (ins.offset, i))
            .collect();
        let pool = d
            .cache
            .slot_at(bca, d.dis.bca_constant_pool_slot())
            .and_then(|v| v.as_ref())
            .and_then(|r| d.cache.ref_object(r));
        let handlers = read_handler_table(d, bca);

        // 预扫：V8 的 TDZ 检查（`X; ThrowReferenceErrorIfHole [池索引]`）紧跟在 context 槽
        // 读取之后 → 用它把槽号绑到变量名。模块/外层 ScopeInfo 不在链上时，这是拿到真名的
        // 唯一线索（否则满屏 `__ctx.ctx25`）。
        let mut slot_aliases: HashMap<usize, String> = HashMap::new();
        for (k, ins) in instrs.iter().enumerate() {
            let b = ins.name.split('.').next().unwrap_or(&ins.name);
            let is_ctx_load = matches!(
                b,
                "LdaContextSlot"
                    | "LdaImmutableContextSlot"
                    | "LdaScriptContextSlot"
                    | "LdaCurrentContextSlot"
                    | "LdaImmutableCurrentContextSlot"
            );
            if !is_ctx_load {
                continue;
            }
            let ops_n = ins.operands.len();
            let slot_idx = if ops_n >= 3 { ops_n - 2 } else { 0 };
            let slot = match ins.operands.get(slot_idx) {
                Some(Operand::Idx(v)) => *v as usize,
                Some(Operand::Imm(v)) => *v as usize,
                _ => continue,
            };
            for nxt in instrs.iter().skip(k + 1).take(2) {
                let nb = nxt.name.split('.').next().unwrap_or(&nxt.name);
                if nb != "ThrowReferenceErrorIfHole" {
                    continue;
                }
                let Some(Operand::Idx(ci)) = nxt.operands.first() else {
                    continue;
                };
                let name = pool
                    .and_then(|p| d.cache.array_elem(p, *ci as usize))
                    .and_then(|e| e.as_ref())
                    .and_then(|r| d.cache.ref_object(r))
                    .filter(|o| d.cache.obj(*o).ty.is_string(d.table))
                    .and_then(|o| d.dis.string_value(o));
                if let Some(n) = name {
                    let v = sanitize_var(&n.replace(['<', '>'], "").replace('/', "_"));
                    if !v.is_empty() {
                        slot_aliases.entry(slot).or_insert(v);
                    }
                }
                break;
            }
        }

        // 脚本/模块顶层：V8 只在脚本顶层发 `DeclareGlobals`。
        // 这类函数的语句必须铺在文件里执行，否则模块级代码（类定义、require、初始化）永不运行。
        let inline_body = instrs.iter().any(|i| {
            let b = i.name.split('.').next().unwrap_or(&i.name);
            if b != "CallRuntime" && b != "CallJSRuntime" {
                return false;
            }
            // `CallRuntime [DeclareGlobals], …`：id 在操作数里，按名字表判定
            matches!(i.operands.first(), Some(Operand::RuntimeId(v))
                if d.table.runtime_names.get(*v as usize).map(|n| n == "DeclareGlobals").unwrap_or(false))
        });

        Ok(FnCtx {
            d,
            sfi,
            bca,
            instrs,
            idx_of,
            pool,
            scope,
            scope_id,
            regs: Vec::new(),
            acc: None,
            acc_stored: false,
            acc_stored_reg: None,
            last_line: String::new(),
            body: String::new(),
            inline_body,
            last_ctx_slot: None,
            slot_aliases,
            out: String::new(),
            indent: 0,
            loops: Vec::new(),
            temps: Vec::new(),
            handlers,
            ctx_names: Vec::new(),
            ctx_scopes: Vec::new(),
            label_counter: 0,
            tmp_counter: 0,
            phi_vars: Vec::new(),
            skip_to: None,
            skip_spans: Vec::new(),
            name,
            is_async: false,
            is_generator: false,
        })
    }

    fn render_operand(&self, op: &Operand) -> String {
        let dec = Decoder::new(self.d.table, self.d.layout, 0);
        dec.render_operand(op)
    }

    /// 上一条已输出语句（用于折叠完全重复的无副作用赋值）。
    fn line(&mut self, s: &str) {
        // 折叠"完全重复且无副作用"的连续语句：真实代码里 V8 会为同一个字面量
        // 连发两次 Star（`r3 = "#otherSide"; r3 = "#otherSide";`），读起来是噪声。
        // 只折叠"纯赋值语句"这一种形状（`r3 = "#otherSide";`）；
        // 结构行（`}` / `case` / `default:`）绝不能折叠，否则括号配平被破坏。
        if s == self.last_line
            && s.ends_with(';')
            && s.contains(" = ")
            && !s.contains('(')
            && !s.contains('[')
            && !s.contains('{')
        {
            return;
        }
        for _ in 0..self.indent {
            self.out.push_str("  ");
        }
        self.out.push_str(s);
        self.out.push('\n');
        self.last_line = s.to_string();
    }

    /// 语句化渲染：以 `{`/`function`/`class` 开头的表达式需要括号包裹，否则会被解析成块/声明。
    fn render_stmt(e: &Expr) -> String {
        guard_stmt_start(&e.render())
    }

    fn temp(&mut self, e: Expr) -> Expr {
        let name = format!("t{}", self.tmp_counter);
        self.tmp_counter += 1;
        self.temps.push((name.clone(), e));
        Expr::Temp(name)
    }

    /// 主流程：函数签名 + 语句体。
    fn run(&mut self) -> Result<(), String> {
        // 脚本/模块顶层：不包 function 外壳，直接出语句（原样铺在文件里才会执行）
        if self.inline_body {
            self.declare_locals();
            self.emit_range(0, self.instrs.len())?;
            self.body = std::mem::take(&mut self.out);
            return Ok(());
        }
        // 函数种类：按出现的 opcode 判定（比枚举稳）
        self.is_async = self.instrs.iter().any(|i| i.name.starts_with("Await"));
        self.is_generator = self.instrs.iter().any(|i| i.name.starts_with("SuspendGenerator"));

        let params = self.param_count();
        let names: Vec<String> = if params > 64 {
            vec![format!("/* {params} 个参数（异常值，忽略） */")]
        } else {
            (0..params).map(|i| format!("a{i}")).collect()
        };

        self.out.push_str("// @generated by jscd — 源码文本不在 code cache 中，以下是按字节码重建的伪 JS\n");
        // 需要 class 体的只有"真的用到 super"的构造器（派生类），判据用字节码证据：
        // ScopeInfo 的 function_kind 位在版本间不一致（曾把箭头函数 kind=11 误判成类构造器，
        // 于是 `const __class_add_7 = class{...}` 既不是箭头也无法当普通函数调用）。
        // 普通（基类）构造器用 `function X(){...}` 完全合法：`new X()` 一样工作。
        let is_class_ctor = self.instrs.iter().any(|i| {
            let b = i.name.split('.').next().unwrap_or(&i.name);
            b == "ThrowSuperNotCalledIfHole" || b == "GetSuperConstructor"
        });
        if is_class_ctor {
            let fname = sanitize_var(&self.fn_name());
            // 名字要与调用点一致（V8 里 `Counter.zero()` / `new Counter()` 都按这个名字引用）
            self.out.push_str(&format!("var {fname} = class {{\n"));
        }
        let header = if is_class_ctor {
            format!("constructor({})", names.join(", "))
        } else {
            let fname = sanitize_var(&self.fn_name());
            let star = if self.is_generator { "*" } else { "" };
            let prefix = if self.is_async { "async " } else { "" };
            format!("{prefix}function{star} {fname}({})", names.join(", "))
        };
        self.out.push_str(&header);
        self.out.push_str(" {\n");
        self.indent = 1;

        // 声明寄存器与 context 局部名（保持语法合法、便于阅读）
        self.declare_locals();
        self.emit_range(0, self.instrs.len())?;

        self.indent = 0;
        if is_class_ctor {
            // class 体用 `var X = class { constructor(){} }` 包：闭合的是表达式
            self.out.push_str("}\n};\n");
        } else {
            self.out.push_str("}\n");
        }
        self.out.push('\n');
        self.body = self.out.clone();
        Ok(())
    }

    fn finish(self) -> String {
        self.out
    }

    /// 只取函数体（脚本/模块顶层代码要原样铺在文件里，而不是包成函数）。
    fn finish_body(&self) -> String {
        self.body.clone()
    }

    fn fn_name(&self) -> String {
        match &self.scope {
            Some(s) if !s.name.is_empty() => s.name.clone(),
            Some(s) if !s.inferred_name.is_empty() => s.inferred_name.clone(),
            _ => self.name.clone(),
        }
    }

    fn param_count(&self) -> u32 {
        if let Some(s) = &self.scope {
            if s.param_count > 0 {
                return s.param_count;
            }
        }
        self.bca_param_count()
    }

    /// BCA 里记录的形参个数（与 disasm 的 `Parameter count` 一致，含 rest 形参本身）。
    fn bca_param_count(&self) -> u32 {
        let ba = self
            .d
            .table
            .bytecode_array
            .clone()
            .unwrap_or_else(|| crate::tables::BytecodeArrayLayout::fallback(self.d.ts));
        let v = ba
            .off("parameter_size")
            .and_then(|o| self.d.cache.raw_at_ts(self.bca, o, 4, self.d.ts))
            .map(|x| u32::from_le_bytes(x.try_into().unwrap()))
            .unwrap_or(0);
        if self.d.dis.parameter_count_direct() {
            v & 0xFFFF
        } else {
            v / 8
        }
    }

    /// 收集用到的寄存器与 context 名，生成声明。
    fn declare_locals(&mut self) {
        let mut max_reg: Option<u32> = None;
        for ins in &self.instrs {
            // 短 Star（Star0..Star15）把寄存器编码在 opcode 名里，没有操作数
            if let Some(rest) = ins.name.strip_prefix("Star") {
                if let Ok(n) = rest.parse::<u32>() {
                    max_reg = Some(max_reg.map_or(n, |m: u32| m.max(n)));
                }
            }
            for op in &ins.operands {
                let (idx, is_reg) = match op {
                    Operand::Reg(i) => (*i, true),
                    Operand::RegList { first, .. } => (*first, true),
                    _ => (0, false),
                };
                if is_reg && idx >= 0 {
                    max_reg = Some(max_reg.map_or(idx as u32, |m| m.max(idx as u32)));
                }
            }
        }
        if let Some(m) = max_reg {
            let names: Vec<String> = (0..=m).map(|i| format!("r{i}")).collect();
            self.line(&format!("let {};", names.join(", ")));
        }
        // context 变量已在文件级 `var` 声明（供摊平后的内层函数共享）→ 此处不再重复 let
        if let Some(s) = &self.scope {
            self.ctx_names = s
                .context_locals
                .iter()
                .filter(|n| !n.is_empty())
                .map(|n| sanitize_var(n))
                .collect();
        }
    }

    /// 常量池元素 → 表达式。
    fn constant(&mut self, idx: usize) -> Expr {
        let Some(pool) = self.pool else {
            return Expr::Ident(format!("__const{idx}"));
        };
        match self.d.cache.array_elem(pool, idx) {
            Some(Elem::Smi(v)) => Expr::Num(v as f64),
            Some(Elem::Ref(Ref::Object(o))) => self.object_constant(o),
            Some(Elem::Ref(Ref::RoRef(c, o))) => match self.d.ro_map.as_ref().and_then(|m| m.get(c, o)) {
                // 一律按字符串字面量给出：RO 串的内容未必是合法标识符
                // （"|" / ":" / "-" 之类曾输出成裸标识符 → 语法错误），
                // 用 `Expr::Str` 在任何表达式位置都合法且保真。
                Some(name) => Expr::Str(name.to_string()),
                // 未建表时给出稳定占位名（同一 ref 在整份文件里一致）
                None => Expr::Str(format!("<ro{c}_{o}>")),
            },
            Some(Elem::Ref(Ref::Root(i))) => self.root_value(i),
            _ => Expr::Hole,
        }
    }

    /// roots 表项 → 表达式（统一去前缀与字面量化）。
    fn root_value(&self, i: usize) -> Expr {
        let raw = self.d.table.roots.get(i).map(|s| s.as_str()).unwrap_or("");
        let n = raw
            .strip_prefix("String:")
            .or_else(|| raw.strip_prefix("Symbol:"))
            .unwrap_or(raw);
        match n {
            "UndefinedValue" | "undefined_value" | "uninitialized_value" | "UninitializedValue" => {
                Expr::Undefined
            }
            "TheHoleValue" | "the_hole_value" => Expr::Hole,
            "NullValue" | "null_value" => Expr::Null,
            "TrueValue" | "true_value" => Expr::Bool(true),
            "FalseValue" | "false_value" => Expr::Bool(false),
            "EmptyString" | "empty_string" => Expr::Str(String::new()),
            _ if raw.starts_with("String:") => Expr::Str(n.to_string()),
            // 只有真正的 JS 全局才能当裸标识符：V8 内部根（EmptySlowElementDictionary、
            // EmptyFixedArray…）直接输出会 ReferenceError（模块体现在会真的执行，暴露了出来）
            _ if is_ident(n) && is_js_global(n) => Expr::Ident(n.to_string()),
            _ if is_ident(n) => Expr::Ident(format!("/* root: {n} */ undefined")),
            _ => Expr::Str(n.to_string()),
        }
    }

    /// 对象常量（字符串/数字/正则/字面量模板/嵌套 SFI）。
    fn object_constant(&mut self, o: ObjId) -> Expr {
        let ty = self.d.cache.obj(o).ty;
        if ty.is_string(self.d.table) {
            return Expr::Str(self.d.dis.string_value(o).unwrap_or_default());
        }
        if ty.is(self.d.table, "HeapNumber") {
            if let Some(d) = self.d.cache.raw_at_ts(o, self.d.ts, 8, self.d.ts) {
                return Expr::Num(f64::from_le_bytes(d.try_into().unwrap()));
            }
        }
        if ty.is(self.d.table, "BigInt") {
            if let Some(v) = self.d.dis.describe_ref(&Ref::Object(o)).strip_prefix("<BigInt ") {
                return Expr::BigInt(v.trim_end_matches('>').to_string());
            }
        }
        if ty.is(self.d.table, "SharedFunctionInfo") {
            let n = self.d.dis.sfi_name(o);
            return Expr::Ident(format!(
                "/* function {} */ __uncompiled.{}",
                n,
                sanitize_ident(&n)
            ));
        }
        if ty.is(self.d.table, "ObjectBoilerplateDescription") {
            return self.object_boilerplate(o);
        }
        if ty.is(self.d.table, "ArrayBoilerplateDescription") {
            return self.array_boilerplate(o);
        }
        if ty.is(self.d.table, "FixedArray") {
            // 类方法表 / 跳转表 / 元素数组：把真实元素解出来（原先只给 /*0*/ 占位，
            // 于是类的构造函数、方法、私有名全看不出内容）。
            let n = self.d.cache.array_len(o);
            let mut items = Vec::new();
            for i in 0..n.min(16) {
                items.push(match self.d.cache.array_elem(o, i) {
                    Some(Elem::Smi(v)) => Expr::Num(v as f64),
                    Some(Elem::Ref(Ref::Object(x))) => self.object_constant(x),
                    Some(Elem::Ref(Ref::Root(r))) => self.root_value(r),
                    Some(Elem::Ref(Ref::RoRef(c, off))) => {
                        match self.d.ro_map.as_ref().and_then(|m| m.get(c, off)) {
                            Some(name) => Expr::Str(name.to_string()),
                            None => Expr::Str(format!("<ro{c}_{off}>")),
                        }
                    }
                    _ => Expr::Hole,
                });
            }
            return Expr::ArrayLit(items);
        }
        // 未知常量：给出类型线索（注释）但落到语法合法、运行不抛错的值上
        Expr::Ident(format!("/* {}({o}) */ undefined", ty.name(self.d.table)))
    }

    /// 字面量键：Smi / 堆字符串 / RO 字符串（经 ro-map 还原）。
    fn elem_key(&self, o: ObjId, idx: usize) -> Option<String> {
        match self.d.cache.array_elem(o, idx) {
            Some(Elem::Smi(v)) => Some(v.to_string()),
            Some(Elem::Ref(Ref::RoRef(c, off))) => Some(
                self.d
                    .ro_map
                    .as_ref()
                    .and_then(|m| m.get(c, off))
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| format!("<ro{c}_{off}>")),
            ),
            Some(Elem::Ref(r)) => crate::disasm::name_of_ref(self.d.cache, self.d.table, r),
            _ => None,
        }
    }

    /// 对象字面量的"键为字符串"打分（用于自动判定 kDescriptionStartIndex）。
    fn obp_string_keys(&self, o: ObjId, start: usize, len: usize) -> usize {
        let count = len.saturating_sub(start) / 2;
        (0..count.min(64))
            .filter(|i| self.elem_key(o, start + 2 * i).is_some_and(|k| !k.starts_with('<') || k.starts_with("<ro")))
            .count()
    }

    /// 对象字面量（ObjectBoilerplateDescription）。
    ///
    /// 元素布局随版本变化：
    ///   V8 9.4–12.4：[flags, key0, val0, key1, val1, ...]（kDescriptionStartIndex = 1）
    ///   V8 13.x+    ：[backing_store_size, flags, key0, val0, ...]（V8_ARRAY_EXTRA_FIELDS）
    /// 末位若多出一个元素，它是"计算属性名的个数"（Smi），靠整数除法自然排除。
    fn object_boilerplate(&mut self, o: ObjId) -> Expr {
        let len = self.d.cache.array_len(o);
        let v8_major: u32 = self
            .d
            .table
            .v8
            .split('.')
            .next()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        // 版本给先验，语料给证据：字符串键更多的那种布局胜出
        let (pref, alt) = if v8_major >= 13 { (2usize, 1usize) } else { (1usize, 2usize) };
        let start = if len >= 4 && self.obp_string_keys(o, alt, len) > self.obp_string_keys(o, pref, len) {
            alt
        } else {
            pref
        };
        let count = len.saturating_sub(start) / 2;
        let mut parts = Vec::new();
        for i in 0..count.min(64) {
            let key = self.elem_key(o, start + 2 * i);
            let val = match self.d.cache.array_elem(o, start + 2 * i + 1) {
                Some(Elem::Smi(v)) => Expr::Num(v as f64),
                Some(Elem::Ref(Ref::Object(v))) => self.object_constant(v),
                Some(Elem::Ref(Ref::Root(i))) => self.root_value(i),
                Some(Elem::Ref(Ref::RoRef(c, off))) => {
                    match self.d.ro_map.as_ref().and_then(|m| m.get(c, off)) {
                        Some(n) => Expr::Str(n.to_string()),
                        None => Expr::Str(format!("<ro{c}_{off}>")),
                    }
                }
                _ => Expr::Undefined,
            };
            let k = match key {
                Some(k) if is_ident(&k) => k,
                Some(k) => format!("[{}]", js_string(&k)),
                None => "[/*computed*/]".to_string(),
            };
            parts.push((k, val));
        }
        Expr::ObjectLit(parts)
    }

    /// 数组字面量（ArrayBoilerplateDescription：常量池元素序列）。
    fn array_boilerplate(&mut self, o: ObjId) -> Expr {
        // 布局随版本变化：
        //   V8 ≥ 9.x：Struct{map, flags(Smi), constant_elements(FixedArrayBase)}，
        //             真值在 constant_elements（FixedCOWArray）里，得再解一层
        //   老版本：元素序列直接平铺在 description 上
        let mut holder = o;
        let mut len = self.d.cache.array_len(o);
        if self.d.cache.array_elem(o, 1).is_none() {
            if let Some(Elem::Ref(Ref::Object(inner))) = self.d.cache.array_elem(o, 0) {
                let n = self.d.cache.obj(inner).ty.name(self.d.table).to_string();
                // 只认 Fixed*ArrayMap（排除 JSArrayMap / ArrayBoilerplateDescriptionMap 等误判）
                if n.contains("Fixed") && n.contains("ArrayMap") {
                    holder = inner;
                    len = self.d.cache.array_len(inner);
                }
            }
        }
        let mut items = Vec::new();
        for i in 0..len.min(64) {
            match self.d.cache.array_elem(holder, i) {
                Some(Elem::Smi(v)) => items.push(Expr::Num(v as f64)),
                Some(Elem::Ref(Ref::Object(oid))) => items.push(self.object_constant(oid)),
                Some(Elem::Ref(Ref::Root(r))) => items.push(self.root_value(r)),
                // 只读堆里的字符串（如 "a"）只能靠 ro-map 还原
                Some(Elem::Ref(Ref::RoRef(c, off))) => {
                    items.push(match self.d.ro_map.as_ref().and_then(|m| m.get(c, off)) {
                        Some(n) => Expr::Str(n.to_string()),
                        None => Expr::Str(format!("<ro{c}_{off}>")),
                    });
                }
                _ => items.push(Expr::Undefined),
            }
        }
        Expr::ArrayLit(items)
    }

    /// context 槽名（ScopeInfo 的 context_local_names；不可得时回落 ctxN）。
    fn context_name(&self, slot: usize) -> String {
        // 由 TDZ 检查反推出的别名最可靠（外层/模块 ScopeInfo 常常不在链上）
        if let Some(n) = self.slot_aliases.get(&slot) {
            return n.clone();
        }
        // 块/catch 作用域优先（其 context 的槽 0 为该作用域的 ScopeInfo）
        if let Some(Some(sid)) = self.ctx_scopes.last().copied() {
            if let Some(s) = self.d.scope_by_id(sid) {
                if let Some(n) = s.context_locals.get(slot.saturating_sub(2)) {
                    if !n.is_empty() {
                        return sanitize_var(n);
                    }
                }
            }
        }
        if let Some(n) = self.d.context_name_in_chain(self.scope_id, slot) {
            if !n.is_empty() {
                return sanitize_var(&n);
            }
        }
        // 名字解不出来时不要留裸标识符（会 ReferenceError）：
        // 落到一个已声明的命名空间对象上，读出来是 undefined、写进去也只是记账。
        format!("__ctx.ctx{slot}")
    }

    fn prop_key(&mut self, idx: usize) -> Key {
        match self.constant(idx) {
            Expr::Str(s) => {
                if is_ident(&s) {
                    Key::Ident(s)
                } else {
                    Key::Str(s)
                }
            }
            Expr::Num(n) => Key::Num(n),
            other => Key::Str(other.render()),
        }
    }

    /// 判断指令是否读取 acc（决定上一条 acc 是否已死、需不需要成句）。
    fn reads_acc(&self, name: &str) -> bool {
        // 版本表（bytecodes.h 的 ImplicitRegisterUse）优先：手工名单只作兜底
        if let Some(&(r, _)) = self.d.acc_use.get(name) {
            return r;
        }
        let base = name.split('.').next().unwrap_or(name);
        matches!(
            base,
            "Star" | "Star0" | "Star1" | "Star2" | "Star3" | "Star4" | "Star5" | "Star6" | "Star7"
                | "Star8" | "Star9" | "Star10" | "Star11" | "Star12" | "Star13" | "Star14" | "Star15"
                | "Return" | "Throw" | "ReThrow" | "StaGlobal" | "StaLookupSlot"
                | "StaNamedProperty" | "SetNamedProperty" | "StaKeyedProperty" | "SetKeyedProperty" | "StaNamedOwnProperty" | "DefineNamedOwnProperty"
                | "StaKeyedPropertyAsDefine" | "StaDataPropertyInLiteral" | "DefineKeyedOwnPropertyInLiteral" | "StaInArrayLiteral"
                | "DefineNamedOwnProperty" | "DefineKeyedOwnProperty" | "DefineKeyedOwnPropertyInLiteral"
                | "Add" | "Sub" | "Mul" | "Div" | "Mod" | "Exp" | "BitwiseOr" | "BitwiseXor"
                | "BitwiseAnd" | "ShiftLeft" | "ShiftRight" | "ShiftRightLogical" | "TestEqual"
                | "TestEqualStrict" | "TestLessThan" | "TestLessThanOrEqual" | "TestGreaterThan"
                | "TestGreaterThanOrEqual" | "TestInstanceOf" | "TestIn" | "TestNull"
                | "TestUndefined" | "TestTypeOf" | "ToBooleanLogicalNot" | "LogicalNot" | "TestReferenceEqual"
                | "Negate" | "BitwiseNot" | "Inc" | "Dec" | "ToName" | "ToNumber" | "ToNumeric"
                | "ToString" | "ToObject" | "TypeOf" | "GetIterator" | "GetAsyncIterator"
                | "ForInEnumerate" | "SetKeyedProperty" | "SetNamedProperty" | "CreateArrayFromIterable"
                | "CreateObjectFromIterable" | "Await" | "Yield" | "YieldStar" | "SuspendGenerator"
                | "ThrowReferenceErrorIfHole" | "JumpIfTrue" | "JumpIfFalse" | "JumpIfToBooleanTrue"
                | "JumpIfToBooleanFalse" | "JumpIfNull" | "JumpIfNotNull" | "JumpIfUndefined"
                | "JumpIfUndefinedOrNull" | "JumpIfNotUndefined" | "JumpIfJSReceiver"
                | "JumpIfNotHole" | "ThrowIfNotSuperConstructor" | "ThrowSuperNotCalledIfHole"
                | "TestUndetectable" | "CloneObject" | "DeletePropertyStrict" | "DeletePropertySloppy"
                | "ThrowSuperAlreadyCalledIfNotHole" | "CopyDataProperties" | "CloneObject"
                | "StaCurrentContextSlot" | "StaCurrentScriptContextSlot" | "StaContextSlot" | "StaScriptContextSlot" | "StaModuleVariable"
                | "AddSmi" | "SubSmi" | "MulSmi" | "DivSmi" | "ModSmi" | "ExpSmi"
                | "BitwiseOrSmi" | "BitwiseXorSmi" | "BitwiseAndSmi" | "ShiftLeftSmi" | "ShiftRightSmi"
                | "ShiftRightLogicalSmi" | "SwitchOnSmiNoFeedback" | "CallWithSpread"
                | "ConstructWithSpread" | "StaNamedProperty" | "SetNamedProperty" | "GetTemplateObject"
        )
    }

    /// 占位内建的名字访问：合法标识符用 `ns.name`，否则用 `ns["name"]`
    /// （名字表没覆盖时 id 会是裸数字，点访问就成了非法 JS）。
    fn stub_access(ns: &str, name: &str) -> Expr {
        if is_ident(name) {
            Expr::Member {
                obj: Box::new(Expr::Ident(ns.into())),
                key: Key::Ident(name.into()),
            }
        } else {
            Expr::Member {
                obj: Box::new(Expr::Ident(ns.into())),
                key: Key::Computed(Box::new(Expr::Str(name.into()))),
            }
        }
    }

    /// 累加器的值已经落到某个位置（变量、属性、context 槽）→ 死 acc 不必重复求值。
    /// 真实代码里这一步很关键：`kCallback = require("x")` 之后曾被再发射一次
    /// `require("x");`（重复调用），`r3 = "#otherSide";` 这种也重复。
    fn acc_consumed(&mut self) {
        self.acc_stored = true;
        self.acc_stored_reg = None;
    }

    /// 新建一个 phi 变量（就地声明，避免"声明列表漏项"导致赋值到未声明变量）。
    fn new_phi(&mut self) -> String {
        let phi = format!("phi{}", self.tmp_counter);
        self.tmp_counter += 1;
        self.line(&format!("let {phi};"));
        self.phi_vars.push(phi.clone());
        phi
    }

    /// 单条指令产出的字面量（case 标签用）：LdaSmi / LdaZero / LdaConstant(串或数)。
    fn literal_of(&mut self, idx: usize) -> Option<String> {
        let ins = self.instrs.get(idx)?.clone();
        let base = ins.name.split('.').next().unwrap_or(&ins.name);
        match base {
            "LdaZero" => Some("0".to_string()),
            "LdaSmi" => match ins.operands.first()? {
                Operand::Imm(v) => Some(v.to_string()),
                Operand::Idx(v) => Some(v.to_string()),
                _ => None,
            },
            "LdaConstant" | "LdaConstantWide" => match ins.operands.first()? {
                Operand::Idx(v) => Some(match self.constant(*v as usize) {
                    Expr::Str(s) => js_string(&s),
                    other => other.render(),
                }),
                _ => None,
            },
            _ => None,
        }
    }

    /// case 体收尾：体末是前向无条件 Jump ⇒ 源码里是 break，补一句；
    /// 体是"掉进下一个体"（fallthrough）时什么都不加，交给 JS 语义顺延。
    fn emit_case_break(&mut self, body_end: usize) {
        if body_end == 0 {
            return;
        }
        let last = self.instrs[body_end - 1].clone();
        if let Some(tgt) = self.uncond_jump_target(&last) {
            let prefix = if last.scale > 1 { 1 } else { 0 };
            if tgt > last.offset + prefix {
                self.line("break;");
            }
        }
    }

    /// 比较链 switch 识别与重建。
    ///
    /// V8 把 `switch (v) { case 1: … case 2: … default: … }` 编译成一串
    /// `LdaSmi c; TestEqualStrict v; JumpIfTrue body`，末尾一个 `Jump default`，
    /// case 体顺序铺在后面（相邻 case 靠 fallthrough 共用体）。
    /// 命中时直接发射 `switch`，返回 join（switch 之后的第一条指令下标）。
    fn try_case_chain(&mut self, i: usize, end: usize) -> Option<usize> {
        let cmp_names = [
            "TestEqualStrict",
            "TestEqual",
            "TestReferenceEqual",
        ];
        let base_of = |k: usize, this: &Self| -> String {
            this.instrs
                .get(k)
                .map(|x| x.name.split('.').next().unwrap_or(&x.name).to_string())
                .unwrap_or_default()
        };
        if !cmp_names.contains(&base_of(i, self).as_str()) {
            return None;
        }
        // 被测变量：寄存器（含参数 a0/this 这类负号寄存器）或上下文槽
        let var_op = self.instrs[i].operands.first()?.clone();
        if !matches!(var_op, Operand::Reg(_)) {
            return None;
        }
        let var_name = self.render_operand(&var_op);
        // 同一个值可能被 Mov 到别的寄存器上（V8 常这么干），把别名一起认下来
        let mut aliases: std::collections::HashSet<String> = std::collections::HashSet::new();
        aliases.insert(var_name.clone());
        let mut cases: Vec<(String, usize)> = Vec::new();
        // 首个 case 的字面量已被上层指令发射进 acc（LdaSmi 在 Test 之前）
        let mut lit: Option<String> = self
            .acc
            .as_ref()
            .map(|e| e.render())
            .filter(|s| s.chars().next().is_some_and(|c| c.is_ascii_digit() || c == '"'));
        let mut pending: Option<String> = None;
        let mut j = i;
        let default_target;
        loop {
            if j >= end {
                return None;
            }
            if let Some(v) = self.literal_of(j) {
                lit = Some(v);
                j += 1;
                continue;
            }
            let b = base_of(j, self);
            if cmp_names.contains(&b.as_str()) {
                // 比较的必须是同一个值（本体或它的 Mov 别名）
                match self.instrs[j].operands.first() {
                    Some(op @ Operand::Reg(_)) => {
                        if !aliases.contains(&self.render_operand(op)) {
                            return None;
                        }
                    }
                    _ => return None,
                }
                pending = Some(lit.take()?);
                j += 1;
                continue;
            }
            if b == "JumpIfTrue" {
                // 比较与跳转之间可能夹 Mov/Star（同一值换寄存器），靠 aliases 认下来
                let val = pending.take()?;
                let tgt = self.cond_jump_target(&self.instrs[j].clone())?;
                let t_idx = *self.idx_of.get(&tgt)?;
                cases.push((val, t_idx));
                j += 1;
                continue;
            }
            if b == "Mov" && self.instrs[j].operands.len() >= 2 {
                let src = self.render_operand(&self.instrs[j].operands[0].clone());
                let dst = self.render_operand(&self.instrs[j].operands[1].clone());
                if aliases.contains(&src) {
                    aliases.insert(dst);
                }
                j += 1;
                continue;
            }
            if b == "Jump" {
                let tgt = self.uncond_jump_target(&self.instrs[j].clone())?;
                default_target = *self.idx_of.get(&tgt)?;
                j += 1;
                break;
            }
            if b == "Nop" || b.starts_with("Star") {
                j += 1;
                continue;
            }
            return None;
        }
        if cases.len() < 2 {
            return None;
        }
        // case 体必须在派发链之后
        if cases.iter().any(|(_, t)| *t < j) || default_target < j {
            return None;
        }
        // 各 case 体的范围：按物理顺序，到下一个体起点为止
        let mut targets: Vec<usize> = cases.iter().map(|(_, t)| *t).collect();
        targets.push(default_target);
        targets.sort_unstable();
        targets.dedup();
        // join = 任一体末尾前向 Jump 的目标（V8 的 break 落点）
        let mut join: Option<usize> = None;
        for (k, &t) in targets.iter().enumerate() {
            let body_end = targets.get(k + 1).copied().unwrap_or(end);
            if body_end > t {
                if let Some(jt) = self.uncond_jump_target(&self.instrs[body_end - 1].clone()) {
                    if jt > self.instrs[body_end - 1].offset {
                        join = Some(*self.idx_of.get(&jt)?);
                    }
                }
            }
        }
        let join = join?;
        for &t in &targets {
            if t >= join {
                return None;
            }
        }
        // ── 发射 ──
        self.flush_acc_before("SwitchOnSmiNoFeedback");
        self.line(&format!("switch ({var_name}) {{"));
        self.indent += 1;
        // 按目标体归组：同一个体的所有 case 标签要挤在体之前
        // （V8 的派发链顺序是 1→体A、2→体A、3→体B，直接顺序输出会把 case 2 挂到体 B 上）
        let mut groups: Vec<(usize, Vec<String>)> = Vec::new();
        for (val, t) in cases.iter() {
            match groups.iter_mut().find(|(gt, _)| gt == t) {
                Some((_, v)) => v.push(val.clone()),
                None => groups.push((*t, vec![val.clone()])),
            }
        }
        for (t, labels) in groups {
            let line = labels
                .iter()
                .map(|l| format!("case {l}:"))
                .collect::<Vec<_>>()
                .join(" ");
            self.line(&line);
            self.indent += 1;
            let k = targets.iter().position(|x| *x == t).unwrap();
            let body_end = targets.get(k + 1).copied().unwrap_or(join).min(join);
            if let Err(e) = self.emit_range(t, body_end) {
                self.line(&format!("/* 结构化失败: {e} */"));
            }
            // 体末是前向 Jump（= 源码里的 break）→ 补 break；否则是 fallthrough，让 JS 顺延
            self.emit_case_break(body_end);
            if body_end == join {
                self.acc = None;
            }
            self.indent -= 1;
        }
        self.line("default:");
        self.indent += 1;
        {
            let k = targets.iter().position(|x| *x == default_target).unwrap();
            let body_end = targets.get(k + 1).copied().unwrap_or(join).min(join);
            if let Err(e) = self.emit_range(default_target, body_end) {
                self.line(&format!("/* 结构化失败: {e} */"));
            }
            self.emit_case_break(body_end);
        }
        self.indent -= 1;
        self.indent -= 1;
        self.line("}");
        Some(join)
    }

    /// 发射一段指令范围（结构化控制流重建的核心）。
    ///
    /// 规则（覆盖 V8 常见模式）：
    /// - 前向条件跳转到 break 目标 → `if (!cond) break;`
    /// - 后向条件跳转到 continue 目标 → `if (cond) continue;`
    /// - 前向条件跳转（其他）→ `if (cond) { … }`（紧跟的无条件 Jump 构成 `else`）
    /// - JumpLoop → `continue;`；前向无条件跳转到 break 目标 → `break;`
    /// - 循环头（本指令是后向跳转的目标）→ 包成 `while (true) { … }`
    fn emit_range(&mut self, mut i: usize, end: usize) -> Result<(), String> {
        while i < end {
            let ins = self.instrs[i].clone();
            let base = ins.name.split('.').next().unwrap_or(&ins.name).to_string();

            // ⓪ 已被 guard 子句就地发射的冷块 → 跳过（它的内容已在对应分支里生成过）。
            // 消费后要移除：同一区间可能被别的分支范围再次经过（可选链的 `LdaUndefined`
            // 块就踩过这个坑），留着会静默吞掉指令、让合并值取到分支里的旧值。
            if let Some(pos) = self.skip_spans.iter().position(|(s, _)| *s == i) {
                let (_, e) = self.skip_spans.remove(pos);
                i = e.max(i + 1);
                continue;
            }

            // ① try/catch：handler 覆盖的区间包一层
            if let Some(h) = self
                .handlers
                .iter()
                .find(|h| self.idx_of.get(&(h.start as usize)) == Some(&i) && (h.end as usize) < usize::MAX)
                .copied()
            {
                let body_end = self
                    .idx_of
                    .get(&(h.end as usize))
                    .copied()
                    .unwrap_or(end)
                    .min(end);
                let catch_start = self
                    .idx_of
                    .get(&(h.target as usize))
                    .copied()
                    .unwrap_or(body_end)
                    .min(end);
                let catch_end = self
                    .handlers
                    .iter()
                    .filter(|x| x.target > h.target)
                    .filter_map(|x| self.idx_of.get(&(x.target as usize)).copied())
                    .filter(|s| *s > catch_start && *s <= end)
                    .min()
                    .unwrap_or(end);
                self.line("try {");
                self.indent += 1;
                self.emit_range(i + 1, body_end)?;
                self.indent -= 1;
                self.line("} catch (e) {");
                self.indent += 1;
                self.emit_range(catch_start, catch_end)?;
                self.indent -= 1;
                self.line("}");
                i = catch_end.max(i + 1);
                continue;
            }

            // ② 循环头 → while(true) 包装
            if let Some((back_idx, exit_target)) = self.find_loop(i, end) {
                self.line("while (true) {");
                self.indent += 1;
                self.loops.push(LoopCtx {
                    continue_target: ins.offset,
                    break_target: exit_target,
                });
                // 循环头指令要放在循环体内：`continue` 回到顶部时需要重新求值（条件计算就在头部）
                self.emit_expr_statement(&ins);
                self.emit_range(i + 1, back_idx)?;
                // 回边本身用 continue 表示（不要把它当普通指令发射）
                let back_ins = &self.instrs[back_idx];
                let is_plain_loop = !back_ins.name.starts_with("JumpLoop")
                    && self.uncond_jump_target(back_ins).is_some();
                if !is_plain_loop {
                    self.line("continue;");
                }
                self.loops.pop();
                self.indent -= 1;
                self.line("}");
                i = (back_idx + 1).max(i + 1);
                continue;
            }

            // ③-0 比较链 switch（V8 对 switch 的常见降级形态）
            if let Some(join) = self.try_case_chain(i, end) {
                i = join.max(i + 1);
                continue;
            }

            // ③ 条件跳转
            if let Some(target) = self.cond_jump_target(&ins) {
                self.flush_acc_before(&base);
                // acc 里含调用/赋值等副作用时，条件表达式必须只求值一次：
                // 先物化成变量，条件和分支体都读它（否则 `f()` 会在 if 条件里被再调一次）。
                if let Some(e) = self.acc.clone() {
                    if e.has_effect() {
                        let phi = self.new_phi();
                        let indent = "  ".repeat(self.indent);
                        let init = Self::render_stmt(&e);
                        let at = self.out.len();
                        self.out.insert_str(at, &format!("{indent}{phi} = {init};\n"));
                        self.phi_vars.push(phi.clone());
                        self.acc = Some(Expr::Ident(phi));
                    }
                }
                let cond_e = self.cond_of(&base);
                let cond = cond_e.render();
                let in_break = self.loops.iter().any(|l| l.break_target == target);
                if in_break {
                    // 跳转发生 ⇔ 退出循环 → 条件原样
                    self.line(&format!("if ({cond}) break;"));
                    i += 1;
                    continue;
                }
                let in_continue = self.loops.iter().any(|l| l.continue_target == target);
                if in_continue {
                    self.line(&format!("if ({cond}) continue;"));
                    i += 1;
                    continue;
                }
                if let Some(&t_idx) = self.idx_of.get(&target) {
                    // ③b 冷块在 fallthrough 侧：`JumpIfTrue L`（L 是主流程）+ 紧跟其后的
                    //     短块以 throw/return 收尾。V8 对 `if (!cond) throw X` 就是这么排的
                    //     （跳过去 = 继续正常流程，掉下来 = 抛错）。真实代码里到处是这种守卫。
                    if t_idx > i + 1 && self.block_terminates(t_idx - 1) {
                        let not_taken = Expr::Un {
                            op: "!",
                            e: Box::new(cond_e.clone()),
                            postfix: false,
                        }
                        .render();
                        self.line(&format!("if ({not_taken}) {{"));
                        self.indent += 1;
                        let r = self.emit_range(i + 1, t_idx);
                        self.indent -= 1;
                        self.line("}");
                        if let Err(e) = r {
                            return Err(e);
                        }
                        // acc 在冷块之后保持跳转前的值（冷块必然离开控制流）
                        i = t_idx;
                        continue;
                    }
                    // ③a guard 子句：跳转目标块是"只进不落"且以 return/throw 收尾的冷块
                    //     （V8 对 switch 分支和提前 return 的典型布局）→ 直接展开成
                    //     `if (cond) { <冷块> }`，主流程线性继续，避免整段逻辑被嵌进 if/else。
                    if t_idx > i && self.is_detached(t_idx) {
                        let (ext_raw, tail_shared) = self.detached_extent(t_idx, end);
                        let ext = ext_raw.min(end);
                        if ext > t_idx && self.block_terminates(ext - 1) && self.self_contained(t_idx, ext)
                        {
                            let acc_save = self.acc.clone();
                            let acc_stored_save = self.acc_stored;
                            let acc_stored_reg_save = self.acc_stored_reg;
                            let regs_save = self.regs.clone();
                            // 冷块可能已被别的分支发射过（共享 return / case 穿透）→ 临时放开区间，允许重复展开
                            let mut stash: Vec<(usize, usize)> = Vec::new();
                            self.skip_spans.retain(|s| {
                                if s.0 >= t_idx && s.0 < ext {
                                    stash.push(*s);
                                    false
                                } else {
                                    true
                                }
                            });
                            self.line(&format!("if ({cond}) {{"));
                            self.indent += 1;
                            let r = self.emit_range(t_idx, ext);
                            self.indent -= 1;
                            self.line("}");
                            self.skip_spans.extend(stash);
                            // 冷块只在"跳转发生"时执行 → 未执行路径上 acc/regs 仍是跳转前的状态
                            self.acc = acc_save;
                            self.acc_stored = acc_stored_save;
                            self.acc_stored_reg = acc_stored_reg_save;
                            self.regs = regs_save;
                            // 共享的尾部终结指令（如 pos/neg 共用的 Return）留给线性路径再发射一次
                            let skip_end = if tail_shared { ext - 1 } else { ext };
                            if skip_end > t_idx {
                                self.skip_spans.push((t_idx, skip_end));
                            }
                            if let Err(e) = r {
                                return Err(e);
                            }
                            i += 1;
                            continue;
                        }
                    }
                    if t_idx > i {
                        let then_empty = t_idx <= i + 1;
                        if then_empty {
                            // 空 then：读作反向条件，避免输出 `if (x) { }`
                            if let Some(else_target) =
                                self.uncond_jump_target(&self.instrs[t_idx - 1])
                            {
                                if else_target > target {
                                    if let Some(&e_idx) = self.idx_of.get(&else_target) {
                                        self.line(&format!("if (!({cond})) {{"));
                                        self.indent += 1;
                                        self.emit_range(t_idx, e_idx.min(end))?;
                                        self.indent -= 1;
                                        self.line("}");
                                        i = e_idx.max(i + 1);
                                        continue;
                                    }
                                }
                            }
                            self.line(&format!("/* 条件跳转 @{target}（空分支） */"));
                            i += 1;
                            continue;
                        }
                        let acc_in = self.acc.clone();
                        // 必须在发射分支体**之前**记下来：分支体里的 Star 会改掉这个标记
                        let acc_in_reg = if self.acc_stored { self.acc_stored_reg } else { None };
                        let phi = self.new_phi();
                        let if_start = self.out.len();
                        let then_cond = Expr::Un {
                            op: "!",
                            e: Box::new(cond_e.clone()),
                            postfix: false,
                        }
                        .render();
                        self.line(&format!("if ({then_cond}) {{"));
                        self.indent += 1;
                        let body_start = self.out.len();
                        self.emit_range(i + 1, t_idx.min(end))?;
                        self.materialize_acc(&phi, &acc_in);
                        // then 分支自己给 phi 赋过值吗？（都赋值就不必再插一行死初始化）
                        let then_assigned = self.out[body_start..].contains(&format!("{phi} = "));
                        self.indent -= 1;
                        let mut next = t_idx;
                        let mut has_else = false;
                        let mut else_assigned = false;
                        let mut acc_then = acc_in.clone();
                        // 紧邻 target 之前的无条件 Jump → else 分支
                        if t_idx > 0 {
                            if let Some(else_target) = self.uncond_jump_target(&self.instrs[t_idx - 1])
                            {
                                if else_target > target {
                                    if let Some(&e_idx) = self.idx_of.get(&else_target) {
                                        acc_then = self.acc.clone();
                                        self.line("} else {");
                                        self.indent += 1;
                                        let else_start = self.out.len();
                                        self.emit_range(t_idx, e_idx.min(end))?;
                                        self.materialize_acc(&phi, &acc_then);
                                        else_assigned = self.out[else_start..].contains(&format!("{phi} = "));
                                        self.indent -= 1;
                                        has_else = true;
                                        next = e_idx;
                                    }
                                }
                            }
                        }
                        self.line("}");
                        // 合并点取值：两条分支给出不同的 acc（或 else 缺失时与入口不同）→ 用 phi。
                        // 只跟"入口 acc"比是不够的：`b === undefined ? 2 : b` 这种默认参数
                        // 形态里 else 分支恰好把 acc 还原成入口值，于是两边不同却漏掉了 phi。
                        let r_of = |e: &Option<Expr>| e.as_ref().map(|x| x.render()).unwrap_or_default();
                        let cur = r_of(&self.acc);
                        let entry = r_of(&acc_in);
                        let then_v = r_of(&acc_then);
                        // 有 else 时看两个分支是否给出不同值（默认参数 `b = 2` 的
                        // 形态里 else 恰好把 acc 还原成入口值，只比入口会漏 phi）；
                        // 没有 else 时只能跟入口比。
                        let changed = if has_else { then_v != cur } else { cur != entry };
                        // new_phi 已把名字登记进 phi_vars（并就地声明），这里只负责回填起始值
                        // 注意：即使两条分支都出现过 `phi = …`，也可能是条件赋值（嵌套 if 里），
                        // 直接省掉初始化会丢值 → 一律保留（试过省掉，行为矩阵掉了 3 个 fixture）。
                        let _ = (then_assigned, else_assigned);
                        if changed {
                            let init = match acc_in_reg {
                                Some(r) => format!("r{r}"),
                                None => acc_in
                                    .as_ref()
                                    .map(|e| e.render())
                                    .unwrap_or_else(|| "undefined".into()),
                            };
                            let indent = "  ".repeat(self.indent);
                            self.out
                                .insert_str(if_start, &format!("{indent}{phi} = {init};\n"));
                            self.acc = Some(Expr::Ident(phi));
                        }

                        i = next.max(i + 1);
                        continue;
                    }
                }
                self.line(&format!("if ({cond}) {{ /* jump to @{target} */ }}"));
                i += 1;
                continue;
            }

            // ④ 无条件跳转
            if let Some(target) = self.uncond_jump_target(&ins) {
                let prefix = if ins.scale > 1 { 1 } else { 0 };
                let cur = ins.offset + prefix;
                let is_loop = base.starts_with("JumpLoop");
                if is_loop || target < cur {
                    // 只在循环体内才输出 continue（避免游离的 continue 造成语法错误）
                    if self.loops.iter().any(|l| l.continue_target == target) {
                        self.line("continue;");
                    } else if self.loops.iter().any(|l| l.break_target == target) {
                        self.line("break;");
                    } else {
                        self.line(&format!("/* 回边 @{target}（未识别的循环结构） */"));
                    }
                    i += 1;
                    continue;
                }
                if self.loops.iter().any(|l| l.break_target == target) {
                    self.flush_acc_before(&base);
                    self.line("break;");
                }
                // 其他前向 Jump：通常是 if 的尾部跳转，已由 ③ 消费
                i += 1;
                continue;
            }

            // ⑤ 普通指令
            self.emit_expr_statement(&ins);
            i += 1;
            if let Some(j) = self.skip_to.take() {
                if j > i {
                    i = j;
                }
            }
        }
        Ok(())
    }

    /// 分支结束处把 acc 物化到 phi 变量（若该分支改变了 acc）。
    /// 三元表达式/短路运算/递归都靠这一步才能在合并点拿到值。
    fn materialize_acc(&mut self, phi: &str, before: &Option<Expr>) {
        // acc 的值此刻就在某个寄存器里（Star 过）→ 直接引用寄存器。
        // 重新渲染表达式会踩到"寄存器已被改写"的坑：`sum += v` 会被算成 `sum + v` 用新 sum
        // 再算一遍（输出里的 `r0 = r0 + r11; phi4 = r0 + r11;` 就是这个错）。
        let cur = match (self.acc_stored, self.acc_stored_reg) {
            (true, Some(r)) => Some(format!("r{r}")),
            _ => self.acc.as_ref().map(|e| e.render()),
        };
        let b = before.as_ref().map(|e| e.render()).unwrap_or_default();
        if let Some(a) = cur {
            if a != b && !a.is_empty() {
                self.line(&format!("{phi} = {a};"));
            }
        }
    }

    /// 无条件终结控制流的指令（其后的指令不可能由 fallthrough 到达）。
    fn block_terminates(&self, idx: usize) -> bool {
        let Some(ins) = self.instrs.get(idx) else {
            return false;
        };
        let base = ins.name.split('.').next().unwrap_or(&ins.name);
        matches!(base, "Return" | "Throw" | "ReThrow" | "Abort")
    }

    /// 该指令是否只能被跳转进入（前一条指令必然离开当前顺序流）。
    ///
    /// 例外：前一条无条件 Jump 的目标正好是它自己 → 等价于 fallthrough（V8 会用
    /// `… ; Jump L; L:` 这种"空跳转"做占位），不能当成冷块。
    fn is_detached(&self, idx: usize) -> bool {
        if idx == 0 {
            return true;
        }
        let Some(prev) = self.instrs.get(idx - 1) else {
            return true;
        };
        let base = prev.name.split('.').next().unwrap_or(&prev.name);
        if matches!(base, "Return" | "Throw" | "ReThrow" | "Abort") {
            return true;
        }
        match self.uncond_jump_target(prev) {
            Some(t) => t != self.instrs[idx].offset,
            None => false,
        }
    }

    /// 冷块的物理范围（下标区间 [start, ext)）与"尾部是否为共享终结点"。
    ///
    /// 从 start 沿 fallthrough 前进，遇到"块的共享入口"（被外部跳转指向的指令）即停，
    /// 保证不会把别的分支的代码吞进来；但若该共享入口本身是 return/throw（尾合并），
    /// 仍纳入本块——此时由调用方只把"独占前缀"记为跳过区间，尾部留给线性路径。
    fn detached_extent(&self, start: usize, end: usize) -> (usize, bool) {
        let targets = self.jump_targets();
        let mut j = start;
        while j < end {
            if j > start && targets.contains(&self.instrs[j].offset) {
                return if self.block_terminates(j) {
                    (j + 1, true)
                } else {
                    (j, false)
                };
            }
            if self.block_terminates(j) {
                return (j + 1, false);
            }
            j += 1;
        }
        (j, false)
    }

    /// 全部跳转目标偏移集合（冷块边界判定用）。
    fn jump_targets(&self) -> std::collections::HashSet<usize> {
        let mut s = std::collections::HashSet::new();
        for ins in &self.instrs {
            if let Some(t) = self
                .uncond_jump_target(ins)
                .or_else(|| self.cond_jump_target(ins))
            {
                s.insert(t);
            }
        }
        s
    }

    /// 区间 [start, end) 是否"自包含"：内部没有跳向区间外的跳转。
    /// 冷块必须是单入口单出口，否则会把流程里别的分支一起吞进来
    /// （曾把 `if (!cond) throw` 后面整段正常流程当成冷块，输出的 throw 跑到 if 外面）。
    fn self_contained(&self, start: usize, end: usize) -> bool {
        for k in start..end {
            let ins = &self.instrs[k];
            let target = self
                .cond_jump_target(ins)
                .or_else(|| self.uncond_jump_target(ins));
            if let Some(t) = target {
                match self.idx_of.get(&t) {
                    Some(&ti) => {
                        if ti < start || ti > end {
                            return false;
                        }
                    }
                    None => return false,
                }
            }
        }
        true
    }

    /// 循环检测：本指令（i）是否为某个后向跳转的目标；返回 (回边下标, 循环退出目标)。
    fn find_loop(&self, i: usize, end: usize) -> Option<(usize, usize)> {
        let head = self.instrs[i].offset;
        let mut back = None;
        for j in i + 1..end.min(self.instrs.len()) {
            let ins = &self.instrs[j];
            // 注意：非跳转指令必须 continue，不能提前 return（旧实现用 `?` 导致循环几乎检测不到）
            let Some(t) = self
                .uncond_jump_target(ins)
                .or_else(|| self.cond_jump_target(ins))
            else {
                continue;
            };
            if t == head && (ins.name.starts_with("JumpLoop") || t <= ins.offset) {
                back = Some(j);
            }
        }
        let back = back?;
        // 退出目标 = 回边之后第一条指令的偏移（V8 通常把退出标签放在 JumpLoop 之后）
        let exit = self
            .instrs
            .get(back + 1)
            .map(|x| x.offset)
            .unwrap_or(usize::MAX);
        Some((back, exit))
    }

    fn uncond_jump_target(&self, ins: &Instr) -> Option<usize> {
        let base = ins.name.split('.').next().unwrap_or(&ins.name);
        if !matches!(base, "Jump" | "JumpConstant" | "JumpLoop" | "JumpLoopConstant") {
            return None;
        }
        if base.starts_with("JumpLoop") {
            let rel = match ins.operands.first()? {
                Operand::Idx(v) => *v as usize,
                Operand::Imm(v) => *v as usize,
                _ => return None,
            };
            let prefix = if ins.scale > 1 { 1 } else { 0 };
            return Some(ins.offset + prefix - rel);
        }
        // JumpConstant：常量池取相对量
        if base == "JumpConstant" {
            let idx = match ins.operands.first()? {
                Operand::Idx(v) => *v as usize,
                _ => return None,
            };
            let rel = self
                .pool
                .and_then(|p| self.d.cache.array_elem(p, idx))
                .and_then(|e| e.as_smi())?;
            let prefix = if ins.scale > 1 { 1 } else { 0 };
            return Some((ins.offset as i64 + prefix as i64 + rel) as usize);
        }
        let rel = match ins.operands.first()? {
            Operand::Idx(v) => *v as usize,
            Operand::Imm(v) => *v as usize,
            _ => return None,
        };
        let prefix = if ins.scale > 1 { 1 } else { 0 };
        Some(ins.offset + prefix + rel)
    }

    fn cond_jump_target(&self, ins: &Instr) -> Option<usize> {
        let base = ins.name.split('.').next().unwrap_or(&ins.name);
        if !base.starts_with("JumpIf") {
            return None;
        }
        if base == "JumpIfJSReceiver" {
            // 也算条件分支
        }
        if base.starts_with("JumpIf") {
            let rel: i64 = if base.ends_with("Constant") {
                let idx = match ins.operands.first()? {
                    Operand::Idx(v) => *v as usize,
                    _ => return None,
                };
                self.pool
                    .and_then(|p| self.d.cache.array_elem(p, idx))
                    .and_then(|e| e.as_smi())?
            } else {
                match ins.operands.first()? {
                    Operand::Idx(v) => *v as i64,
                    Operand::Imm(v) => *v,
                    _ => return None,
                }
            };
            let prefix = if ins.scale > 1 { 1 } else { 0 };
            return Some((ins.offset as i64 + prefix as i64 + rel) as usize);
        }
        None
    }

    /// 条件表达式（由跳转类型 + acc 推出）。返回 Expr 以便复用取反化简。
    fn cond_of(&self, base: &str) -> Expr {
        let acc = self.acc.clone().unwrap_or(Expr::Bool(true));
        let neg = |e: Expr| Expr::Un {
            op: "!",
            e: Box::new(e),
            postfix: false,
        };
        let bin = |op: &'static str, r: Expr| Expr::Bin {
            op,
            l: Box::new(acc.clone()),
            r: Box::new(r),
        };
        match base {
            "JumpIfTrue" | "JumpIfToBooleanTrue" => acc,
            "JumpIfFalse" | "JumpIfToBooleanFalse" => neg(acc),
            "JumpIfNull" => bin("===", Expr::Null),
            "JumpIfNotNull" => bin("!==", Expr::Null),
            "JumpIfUndefined" => bin("===", Expr::Undefined),
            "JumpIfNotUndefined" => bin("!==", Expr::Undefined),
            "JumpIfUndefinedOrNull" => bin("==", Expr::Null),
            "JumpIfJSReceiver" => bin("!==", Expr::Undefined),
            "JumpIfNotHole" => bin("!==", Expr::Undefined),
            _ => acc,
        }
    }

    /// 赋值语句发射：目标非左值时退化为求值语句（保持语法合法并保留副作用）。
    fn emit_assign(&mut self, target: Expr, value: Expr, op: &str) {
        if is_lvalue(&target) {
            let t = guard_stmt_start(&target.render());
            let v = value.render();
            self.line(&format!("{t} {op} {v};"));
        } else {
            let t = comment_safe(&target.render());
            let v = comment_safe(&value.render());
            self.line(&format!("/* 非法赋值目标: {t} = {v} */"));
            if value.has_effect() {
                self.line(&format!("{};", Self::render_stmt(&value)));
            }
        }
    }

    /// 会**保留** acc 的指令（既不读也不写）。其余默认视为"写 acc"。
    /// 只有"下一条会覆盖 acc 且不读它"时，当前 acc 才算死（否则 `Mov`/`Star` 之间会丢值）。
    fn preserves_acc(&self, name: &str) -> bool {
        // 明确"不写累加器"（只读 / 完全不碰）→ 值一定保留。
        // "rw" 有歧义（如 StaGlobal 写回同值）→ 仍按手工名单判断。
        if let Some(&(_, w)) = self.d.acc_use.get(name) {
            if !w {
                return true;
            }
        }
        name_was_preserving(name)
    }


    /// acc 若已死且带副作用 → 单独成句。
    fn flush_acc_before(&mut self, next_base: &str) {
        // 只有"覆盖 acc 且不读 acc"才说明旧值已死；保留 acc 的指令（Mov/Star/Jump/存储…）不能 flush
        if self.reads_acc(next_base) || self.preserves_acc(next_base) {
            return;
        }
        if self.acc_stored {
            // 值已存进寄存器（Star）→ 死 acc 无需再次求值，否则会重复调用/赋值副作用
            self.acc = None;
            return;
        }
        if let Some(e) = self.acc.take() {
            if e.has_effect() {
                let s = Self::render_stmt(&e);
                self.line(&format!("{s};"));
            }
        }
    }

    /// 单条指令 → 表达式/语句。
    fn emit_expr_statement(&mut self, ins: &Instr) {
        let base = ins.name.split('.').next().unwrap_or(&ins.name).to_string();
        if !self.reads_acc(&base) {
            self.flush_acc_before(&base);
        }
        if !self.preserves_acc(&base) {
            self.acc_stored = false;
        }
        let ops: Vec<String> = ins
            .operands
            .iter()
            .map(|o| self.render_operand(o))
            .collect();
        let arg = |n: usize| ops.get(n).cloned().unwrap_or_default();
        let reg_of = |s: &str| -> u32 { s.trim_start_matches('r').parse().unwrap_or(0) };
        let idx_num = |s: &str| -> Option<usize> { s.trim_matches(['[', ']']).parse().ok() };

        match base.as_str() {
            // ── 字面量 / 寄存器 ──
            "LdaZero" => self.acc = Some(Expr::Num(0.0)),
            "LdaSmi" => {
                let v = ops.first().and_then(|s| idx_num(s)).unwrap_or(0);
                self.acc = Some(Expr::Num(v as f64));
            }
            "LdaConstant" | "LdaConstantWide" => {
                if let Some(i) = idx_num(&arg(0)) {
                    self.acc = Some(self.constant(i));
                }
            }
            "LdaUndefined" => self.acc = Some(Expr::Undefined),
            "LdaNull" => self.acc = Some(Expr::Null),
            "LdaTrue" => self.acc = Some(Expr::Bool(true)),
            "LdaFalse" => self.acc = Some(Expr::Bool(false)),
            "LdaTheHole" => self.acc = Some(Expr::Hole),
            "Ldar" => {
                self.acc = Some(self.operand_expr(&arg(0)));
            }
            "Star" => {
                let r = reg_of(&arg(0));
                self.store_reg(r, self.acc.clone());
                // Star 之后 acc 与 r 同值 → 直接用 r 表示，
                // 否则表达式文本会在 r 被改写后失真（`obj?.nope?.deep` 曾算成 `r7.deep`）
                self.acc = Some(Expr::Reg(r));
                self.acc_stored = true;
                self.acc_stored_reg = Some(r);
            }
            // 短 Star：StarN 直接编码寄存器号
            _ if base.starts_with("Star") && base[4..].chars().all(|c| c.is_ascii_digit()) => {
                let r: u32 = base[4..].parse().unwrap_or(0);
                self.store_reg(r, self.acc.clone());
                self.acc = Some(Expr::Reg(r));
                self.acc_stored = true;
                self.acc_stored_reg = Some(r);
            }
            "Mov" => {
                let dst = reg_of(&arg(1));
                let value = Some(self.operand_expr(&arg(0)));
                self.store_reg(dst, value);
            }
            "LdaGlobal" | "LdaGlobalInsideTypeof" => {
                let name = idx_num(&arg(0))
                    .map(|i| match self.constant(i) {
                        Expr::Str(s) => s,
                        other => other.render(),
                    })
                    .unwrap_or_else(|| "globalThis".into());
                self.acc = Some(Expr::Ident(sanitize_var(&name)));
            }
            "LdaLookupSlot" | "LdaLookupScriptContextSlot" | "LdaLookupSlotInsideTypeof" | "LdaLookupScriptContextSlotInsideTypeof" => {
                let name = idx_num(&arg(0))
                    .map(|i| match self.constant(i) {
                        Expr::Str(s) => s,
                        other => other.render(),
                    })
                    .unwrap_or_else(|| "__lookup".into());
                self.acc = Some(Expr::Ident(sanitize_var(&name)));
            }
            // 当前作用域取 [slot]；LdaContextSlot 形如 <context>, [slot], [depth] → 取倒数第二个
            "LdaCurrentContextSlot" | "LdaCurrentScriptContextSlot" | "LdaImmutableCurrentContextSlot" => {
                let slot = idx_num(&arg(0)).unwrap_or(0);
                self.acc = Some(Expr::Ident(self.context_name(slot)));
            }
            "LdaContextSlot" | "LdaScriptContextSlot" | "LdaImmutableContextSlot" => {
                let i = ops.len().saturating_sub(2);
                let slot = idx_num(&arg(i)).unwrap_or(0);
                self.last_ctx_slot = Some(slot);
                self.acc = Some(Expr::Ident(self.context_name(slot)));
            }
            "StaCurrentContextSlot" | "StaCurrentScriptContextSlot" | "StaContextSlot" | "StaScriptContextSlot" => {
                // 取正确的槽号：StaContextSlot 形如 <context>, [slot], [depth] → 取倒数第二个
                let slot = if base.starts_with("StaContextSlot") || base.starts_with("StaScriptContextSlot") {
                    let i = ops.len().saturating_sub(2);
                    idx_num(&arg(i)).unwrap_or(0)
                } else {
                    idx_num(&arg(0)).unwrap_or(0)
                };
                let value = self.acc.clone().unwrap_or(Expr::Undefined);
                // `LdaTheHole; StaContextSlot` 是 V8 的上下文槽初始化（内部簿记）→ 不输出噪声
                if matches!(value, Expr::Hole) {
                    return;
                }
                let name = sanitize_var(&self.context_name(slot));
                self.line(&format!("{name} = {};", value.render()));
                self.acc_consumed();
            }
            "PushContext" => {
                // 上下文栈：仅记录，不影响输出
            }


            // ── 属性访问 ──
            "LdaNamedProperty" | "GetNamedProperty" | "LdaNamedPropertyFromSuper" | "GetNamedPropertyFromSuper" => {
                let obj = self.reg_expr(&arg(0));
                let key = idx_num(&arg(1)).map(|i| self.prop_key(i)).unwrap_or(Key::Str("?".into()));
                self.acc = Some(Expr::Member {
                    obj: Box::new(obj),
                    key,
                });
            }
            "LdaKeyedProperty" | "GetEnumeratedKeyedProperty" | "GetKeyedProperty" => {
                // V8 语义：对象在寄存器操作数、键在累加器 → obj[key]
                let obj = self.operand_expr(&arg(0));
                let key = self.acc.clone().unwrap_or(Expr::Hole);
                self.acc = Some(Expr::Member {
                    obj: Box::new(obj),
                    key: Key::Computed(Box::new(key)),
                });
            }
            "StaNamedProperty" | "SetNamedProperty" | "StaNamedOwnProperty" | "DefineNamedOwnProperty" => {
                let obj = self.reg_expr(&arg(0));
                let key = idx_num(&arg(1)).map(|i| self.prop_key(i)).unwrap_or(Key::Str("?".into()));
                let value = self.acc.clone().unwrap_or(Expr::Undefined);
                let target = Expr::Member {
                    obj: Box::new(obj),
                    key,
                };
                self.emit_assign(target, value, "=");
                self.acc_consumed();
            }
            "StaKeyedProperty" | "SetKeyedProperty" => {
                let obj = self.reg_expr(&arg(0));
                let key = self.reg_expr(&arg(1));
                let value = self.acc.clone().unwrap_or(Expr::Undefined);
                let target = Expr::Member {
                    obj: Box::new(obj),
                    key: Key::Computed(Box::new(key)),
                };
                self.emit_assign(target, value, "=");
                self.acc_consumed();
            }
            "StaGlobal" => {
                let name = idx_num(&arg(0))
                    .map(|i| match self.constant(i) {
                        Expr::Str(s) => s,
                        other => other.render(),
                    })
                    .unwrap_or_else(|| "__global".into());
                let value = self.acc.clone().unwrap_or(Expr::Undefined);
                let name = sanitize_var(&name);
                self.line(&format!("{name} = {};", value.render()));
                self.acc_consumed();
            }
            "StaDataPropertyInLiteral" | "DefineKeyedOwnPropertyInLiteral" | "DefineKeyedOwnPropertyInLiteral" => {
                let obj = self.reg_expr(&arg(0));
                let key = self.reg_expr(&arg(1));
                let value = self.acc.clone().unwrap_or(Expr::Undefined);
                let target = Expr::Member {
                    obj: Box::new(obj),
                    key: Key::Computed(Box::new(key)),
                };
                self.emit_assign(target, value, "=");
                self.acc_consumed();
            }
            "StaInArrayLiteral" => {
                let arr = self.reg_expr(&arg(0));
                let idx = self.reg_expr(&arg(1));
                let value = self.acc.clone().unwrap_or(Expr::Undefined);
                let target = Expr::Member {
                    obj: Box::new(arr),
                    key: Key::Computed(Box::new(idx)),
                };
                self.emit_assign(target, value, "=");
            }

            // ── 运算 ──
            // V8 语义：`OP r` = `r OP acc`（寄存器是左操作数、累加器是右操作数）
            // 证据：sub(a,b)=a-b → Ldar a1; Sub a0；sub2=b-a → Ldar a0; Sub a1
            "Add" | "Sub" | "Mul" | "Div" | "Mod" | "Exp" | "BitwiseOr" | "BitwiseXor"
            | "BitwiseAnd" | "ShiftLeft" | "ShiftRight" | "ShiftRightLogical" => {
                let op = binop_of(&base);
                let l = self.reg_expr(&arg(0));
                let r = self.acc.clone().unwrap_or(Expr::Hole);
                self.acc = Some(Expr::Bin {
                    op,
                    l: Box::new(l),
                    r: Box::new(r),
                });
            }
            "AddSmi" | "SubSmi" | "MulSmi" | "DivSmi" | "ModSmi" | "ExpSmi" | "BitwiseOrSmi"
            | "BitwiseXorSmi" | "BitwiseAndSmi" | "ShiftLeftSmi" | "ShiftRightSmi"
            | "ShiftRightLogicalSmi" => {
                let op = binop_of(base.trim_end_matches("Smi"));
                let v = arg(0)
                    .trim_matches(['[', ']'])
                    .parse::<f64>()
                    .unwrap_or(0.0);
                let l = self.acc.clone().unwrap_or(Expr::Hole);
                self.acc = Some(Expr::Bin {
                    op,
                    l: Box::new(l),
                    r: Box::new(Expr::Num(v)),
                });
            }
            // V8 语义：`Inc`/`Dec` 是 **前缀**——结果（新值）留在累加器里。
            // 证据：`return ++n`（闭包 fixture）→ LdaCurrentContextSlot; Inc; Star0;
            //       StaCurrentContextSlot; Return —— 存回去的是 r0（新值）。
            // 后缀 `n++` 的旧值由 V8 另存寄存器，不经 Inc 体现。
            "Inc" => {
                let e = self.acc.take().unwrap_or(Expr::Hole);
                self.acc = Some(if is_lvalue(&e) && !is_literalish(&e) {
                    Expr::Un {
                        op: "++",
                        e: Box::new(e),
                        postfix: false,
                    }
                } else {
                    Expr::Bin {
                        op: "+",
                        l: Box::new(e),
                        r: Box::new(Expr::Num(1.0)),
                    }
                });
            }
            "Dec" => {
                let e = self.acc.take().unwrap_or(Expr::Hole);
                self.acc = Some(if is_lvalue(&e) && !is_literalish(&e) {
                    Expr::Un {
                        op: "--",
                        e: Box::new(e),
                        postfix: false,
                    }
                } else {
                    Expr::Bin {
                        op: "-",
                        l: Box::new(e),
                        r: Box::new(Expr::Num(1.0)),
                    }
                });
            }
            "Negate" => self.unary("-"),
            "BitwiseNot" => self.unary("~"),
            "LogicalNot" => self.unary("!"),
            "ToBooleanLogicalNot" => {
                let e = self.acc.take().unwrap_or(Expr::Hole);
                self.acc = Some(Expr::Un {
                    op: "!",
                    e: Box::new(e),
                    postfix: false,
                });
            }
            "TypeOf" => {
                let e = self.acc.take().unwrap_or(Expr::Hole);
                self.acc = Some(Expr::Un {
                    op: "typeof ",
                    e: Box::new(e),
                    postfix: false,
                });
            }
            "ToName" | "ToNumber" | "ToNumeric" | "ToString" | "ToObject" | "ToBoolean" => {
                // 带寄存器操作数时 V8 把转换结果写进寄存器（如类名 `ToName r6`），
                // 不落寄存器会出现"r6 被引用却从未赋值"。
                if let Some(Operand::Reg(r)) = ins.operands.first() {
                    if *r >= 0 {
                        let r = *r as u32;
                        let v = self.acc.clone();
                        self.store_reg(r, v);
                    }
                }
            }
            // 同理：`TestLessThan r` = `r < acc`（证据：x<0 → LdaZero; TestLessThan a0）
            "TestEqual" | "TestEqualStrict" | "TestLessThan" | "TestLessThanOrEqual"
            | "TestGreaterThan" | "TestGreaterThanOrEqual" | "TestInstanceOf" | "TestIn"
            | "TestReferenceEqual" => {
                let op = cmp_of(&base);
                let l = self.reg_expr(&arg(0));
                let r = self.acc.clone().unwrap_or(Expr::Hole);
                self.acc = Some(Expr::Bin {
                    op,
                    l: Box::new(l),
                    r: Box::new(r),
                });
            }
            "TestNull" => {
                let e = self.acc.clone().unwrap_or(Expr::Hole);
                self.acc = Some(Expr::Bin {
                    op: "===",
                    l: Box::new(e),
                    r: Box::new(Expr::Null),
                });
            }
            "TestUndefined" => {
                let e = self.acc.clone().unwrap_or(Expr::Hole);
                self.acc = Some(Expr::Bin {
                    op: "===",
                    l: Box::new(e),
                    r: Box::new(Expr::Undefined),
                });
            }
            "TestTypeOf" => {
                let e = self.acc.clone().unwrap_or(Expr::Hole);
                let want = match ops.first().map(|s| s.as_str()) {
                    Some("#0") => "number",
                    Some("#1") => "string",
                    Some("#2") => "symbol",
                    Some("#3") => "boolean",
                    Some("#4") => "bigint",
                    Some("#5") => "undefined",
                    Some("#6") => "function",
                    _ => "object",
                };
                self.acc = Some(Expr::Bin {
                    op: "===",
                    l: Box::new(Expr::Un {
                        op: "typeof ",
                        e: Box::new(e),
                        postfix: false,
                    }),
                    r: Box::new(Expr::Str(want.to_string())),
                });
            }

            // ── 调用 / 构造 ──
            // CallPropertyN callee, receiver, arg0..argN-1, [feedback]
            // 反汇编样例：CallProperty1 r1, r2, r3, [3] → 被调 r1（= obj.method）、接收者 r2、参数 r3
            "CallProperty0" | "CallProperty1" | "CallProperty2" | "CallProperty" => {
                let callee = self.operand_expr(&arg(0));
                let receiver = self.operand_expr(&arg(1));
                let n = match base.as_str() {
                    "CallProperty0" => 0,
                    "CallProperty1" => 1,
                    "CallProperty2" => 2,
                    _ => ops.len().saturating_sub(3),
                };
                let args: Vec<Expr> = (0..n)
                    .map(|k| self.operand_expr(&arg(2 + k)))
                    .collect();
                // 语义：`<callee-stored-in-reg>(<receiver>, args)` —— 接收者必须传进去，
                // 否则 `arr.join("-")` 会退化成 `fn("-")`（this=undefined → TypeError）。
                let callee = match callee {
                    Expr::Member { obj, key } => {
                        // 形如 `obj.method` 已自带接收者
                        let _ = receiver;
                        Expr::Member { obj, key }
                    }
                    other => Expr::Member {
                        obj: Box::new(other),
                        key: Key::Ident("call".to_string()),
                    },
                };
                let mut all = args;
                if matches!(callee, Expr::Member { ref key, .. } if matches!(key, Key::Ident(k) if k == "call"))
                {
                    all.insert(0, receiver);
                }
                self.acc = Some(Expr::Call {
                    callee: Box::new(callee),
                    args: all,
                    is_new: false,
                    spread_arg: None,
                });
            }
            // CallUndefinedReceiverN callable, arg0..argN-1, [feedback]
            // 被调是**第一个寄存器操作数**（接收者为 undefined），不是累加器。
            // 证据：`StringPrototypeIncludes(str, ch)` → LdaGlobal…Star8; LdaConstant…Star10; CallUndefinedReceiver2 r8, a0, r10
            "CallUndefinedReceiver0" | "CallUndefinedReceiver1" | "CallUndefinedReceiver2"
            | "CallUndefinedReceiver" => {
                let callee = self.operand_expr(&arg(0));
                let n = match base.as_str() {
                    "CallUndefinedReceiver0" => 0,
                    "CallUndefinedReceiver1" => 1,
                    "CallUndefinedReceiver2" => 2,
                    _ => ops.len().saturating_sub(2),
                };
                let args: Vec<Expr> = (0..n).map(|k| self.operand_expr(&arg(1 + k))).collect();
                self.acc = Some(Expr::Call {
                    callee: Box::new(callee),
                    args,
                    is_new: false,
                    spread_arg: None,
                });
            }
            // CallAnyReceiver receiver, arg0.., [feedback]（被调在 acc）
            "CallAnyReceiver" | "Call" | "CallNoFeedback" => {
                let callee = self.acc.clone().unwrap_or(Expr::Hole);
                let n = ops.len().saturating_sub(1).min(8);
                let args: Vec<Expr> = (1..=n).map(|k| self.operand_expr(&arg(k))).collect();
                self.acc = Some(Expr::Call {
                    callee: Box::new(callee),
                    args,
                    is_new: false,
                    spread_arg: None,
                });
            }
            "CallWithSpread" | "CallWithArrayLike" => {
                // V8: `CallWithSpread <callable>, <args+spread reglist>, [slot]`
                // 被调在**第一个寄存器操作数**，展开的实参是寄存器组最后一个
                let callee = self.reg_expr(&arg(0));
                let regs = self.reglist_exprs(&ops, 1);
                let (spread, fixed) = match regs.split_last() {
                    Some((last, head)) => (last.clone(), head.to_vec()),
                    None => (Expr::Hole, Vec::new()),
                };
                // V8 语义（interpreter-generator.cc）：寄存器组**第一个恒为接收者**，
                // 最后一个是展开实参 → `callee.call(recv, args…, ...spread)`
                if !fixed.is_empty() {
                    let recv = fixed[0].clone();
                    let mut call_args = vec![recv];
                    call_args.extend(fixed[1..].iter().cloned());
                    self.acc = Some(Expr::Call {
                        callee: Box::new(Expr::Member {
                            obj: Box::new(callee),
                            key: Key::Ident("call".into()),
                        }),
                        args: call_args,
                        is_new: false,
                        spread_arg: Some(Box::new(spread)),
                    });
                } else {
                    self.acc = Some(Expr::Call {
                        callee: Box::new(callee),
                        args: fixed,
                        is_new: false,
                        spread_arg: Some(Box::new(spread)),
                    });
                }
            }
            // Construct constructor, args-reglist, [feedback]（被调在操作数 0）
            "Construct" | "ConstructForwardAllArgs" => {
                let callee = self.operand_expr(&arg(0));
                let args = self.reglist_exprs(&ops, 1);
                self.acc = Some(Expr::Call {
                    callee: Box::new(callee),
                    args,
                    is_new: true,
                    spread_arg: None,
                });
            }
            "ConstructWithSpread" => {
                let callee = self.operand_expr(&arg(0));
                let regs = self.reglist_exprs(&ops, 1);
                let (spread, fixed) = match regs.split_last() {
                    Some((last, head)) => (last.clone(), head.to_vec()),
                    None => (Expr::Hole, Vec::new()),
                };
                self.acc = Some(Expr::Call {
                    callee: Box::new(callee),
                    args: fixed,
                    is_new: true,
                    spread_arg: Some(Box::new(spread)),
                });
            }
            "CallRuntime" | "CallJSRuntime" => {
                let raw = ops.first().cloned().unwrap_or_else(|| "runtime".into());
                let raw = raw.trim_matches(['[', ']']).to_string();
                // 渲染成裸数字说明名字表没覆盖这个 id → 退回表里查一次
                let name = if raw.chars().all(|c| c.is_ascii_digit()) {
                    raw.parse::<usize>()
                        .ok()
                        .and_then(|i| self.d.table.runtime_names.get(i).cloned())
                        .unwrap_or(raw)
                } else {
                    raw
                };
                // 参数是 RegList（r7-r8）→ 展开成逐个寄存器实参
                let args = self.reglist_exprs(&ops, 1);
                self.acc = Some(Expr::Call {
                    callee: Box::new(Self::stub_access("__runtime", &name)),
                    args,
                    is_new: false,
                    spread_arg: None,
                });
            }
            "InvokeIntrinsic" => {
                let name = ops
                    .first()
                    .map(|s| s.trim_matches(['[', ']']).trim_start_matches('_').to_string())
                    .unwrap_or_else(|| "intrinsic".into());
                let args = self.reglist_exprs(&ops, 1);
                // 对象展开有精确的 JS 对应写法，别丢成占位调用：
                // [_CopyDataProperties], r7-r8 → target=r7, source=r8
                if name == "CopyDataProperties" && args.len() >= 2 {
                    let target = args[0].clone();
                    let src = args[1].clone();
                    let call = Expr::Call {
                        callee: Box::new(Expr::Member {
                            obj: Box::new(Expr::Ident("Object".into())),
                            key: Key::Ident("assign".into()),
                        }),
                        args: vec![target.clone(), src],
                        is_new: false,
                        spread_arg: None,
                    };
                    // 原地并入（返回目标自身）→ 直接写成赋值，读起来就是 `t = Object.assign(t, s)`
                    self.emit_assign(target.clone(), call, "=");
                    self.acc = Some(target);
                } else {
                    self.acc = Some(Expr::Call {
                        callee: Box::new(Self::stub_access("__intrinsic", &name)),
                        args,
                        is_new: false,
                        spread_arg: None,
                    });
                }
            }

            // ── 对象/数组/闭包 ──
            "CreateObjectLiteral" => {
                if let Some(i) = idx_num(&arg(0)) {
                    self.acc = Some(self.constant(i));
                }
            }
            "CreateArrayLiteral" => {
                if let Some(i) = idx_num(&arg(0)) {
                    self.acc = Some(self.constant(i));
                }
            }
            "CreateEmptyObjectLiteral" => {
                self.acc = Some(Expr::ObjectLit(Vec::new()));
            }
            "CreateEmptyArrayLiteral" => {
                self.acc = Some(Expr::ArrayLit(Vec::new()));
            }
            "CreateRegExpLiteral" => {
                let pat = idx_num(&arg(0))
                    .map(|i| match self.constant(i) {
                        Expr::Str(s) => s,
                        other => other.render(),
                    })
                    .unwrap_or_default();
                // 用构造器形式，避免模式文本里的 `/` 与转义造成的字面量歧义
                self.acc = Some(Expr::Ident(format!(
                    "new RegExp({}, {})",
                    js_string(&pat),
                    js_string(&regexp_flags(&ops))
                )));
            }
            "CreateClosure" => {
                let idx = idx_num(&arg(0));
                let name = idx
                    .and_then(|i| {
                        self.pool.and_then(|p| self.d.cache.array_elem(p, i)).and_then(|e| {
                            e.as_ref().and_then(|r| match r {
                                Ref::Object(o)
                                    if self.d.cache.obj(o).ty.is(self.d.table, "SharedFunctionInfo") =>
                                {
                                    Some(self.d.dis.sfi_name(o))
                                }
                                _ => None,
                            })
                        })
                    })
                    .unwrap_or_default();
                self.acc = Some(Expr::Ident(if name.is_empty() {
                    "__anonymous".to_string()
                } else {
                    sanitize_var(&name)
                }));
            }
            "CreateFunctionContext" => {
                self.line("/* enter function scope */");
            }
            "CreateBlockContext" | "CreateCatchContext" | "CreateWithContext" | "CreateEvalContext"
            | "CreateScriptContext" => {
                // 压入块作用域：常量池里的 ScopeInfo 决定后续 context 槽的变量名
                let scope_id = idx_num(&arg(0)).and_then(|i| {
                    match self.pool.and_then(|p| self.d.cache.array_elem(p, i)) {
                        Some(Elem::Ref(Ref::Object(o)))
                            if self.d.cache.obj(o).ty.is(self.d.table, "ScopeInfo") =>
                        {
                            Some(o)
                        }
                        _ => None,
                    }
                });
                self.ctx_scopes.push(scope_id);
                self.line(&format!("/* {} */", base.to_lowercase()));
            }
            "PopContext" if !self.ctx_scopes.is_empty() => {
                self.ctx_scopes.pop();
            }
            "CreateMappedArguments" | "CreateUnmappedArguments" => {
                self.acc = Some(Expr::Ident("arguments".into()));
            }
            "CreateRestParameter" => {
                // rest 参数：V8 从实参里切出尾巴。摊平成普通函数后只能用 arguments 重建，
                // 固定参数个数 = 形参个数（含 rest 本身）− 1。
                let fixed = self.bca_param_count().saturating_sub(1);
                self.acc = Some(Expr::Call {
                    callee: Box::new(Expr::Member {
                        obj: Box::new(Expr::Member {
                            obj: Box::new(Expr::ArrayLit(Vec::new())),
                            key: Key::Ident("slice".into()),
                        }),
                        key: Key::Ident("call".into()),
                    }),
                    args: vec![Expr::Ident("arguments".into()), Expr::Num(fixed as f64)],
                    is_new: false,
                    spread_arg: None,
                });
            }
            "CreateArrayFromIterable" => {
                let e = self.acc.take().unwrap_or(Expr::Hole);
                self.acc = Some(Expr::ArrayLit(vec![Expr::Spread(Box::new(e))]));
            }
            "CreateObjectFromIterable" => {
                let e = self.acc.take().unwrap_or(Expr::Hole);
                self.acc = Some(Expr::ObjectLit(vec![(String::new(), e)]));
            }
            "GetTemplateObject" => {
                self.acc = Some(Expr::ArrayLit(Vec::new())); // 模板对象占位
            }
            "GetIterator" | "GetAsyncIterator" => {
                // V8: `GetIterator <object>, <slot>` —— 对象是寄存器操作数，结果写 acc
                let e = self.reg_expr(&arg(0));
                self.acc = Some(Expr::Call {
                    // 注意用**计算键**：`obj[Symbol.iterator]()`；
                    // `obj.Symbol.iterator` 是"名为 Symbol.iterator 的点属性"→ 取到 undefined
                    callee: Box::new(Expr::Member {
                        obj: Box::new(e),
                        key: Key::Computed(Box::new(Expr::Ident("Symbol.iterator".into()))),
                    }),
                    args: Vec::new(),
                    is_new: false,
                    spread_arg: None,
                });
            }
            "CopyDataProperties" => {
                let target = self.reg_expr(&arg(0));
                let src = self.acc.clone().unwrap_or(Expr::Hole);
                let src_s = src.render();
                self.line(&format!(
                    "Object.assign({}, {src_s}); // copy",
                    target.render()
                ));
            }

            // ── 控制流终结 ──
            "Return" => {
                // 模块/脚本顶层被我们铺成普通语句：顶层的 `return` 在脚本里非法，
                // 而它只是 V8 脚本体的收尾 → 不输出。
                if self.inline_body {
                    self.acc = None;
                    return;
                }
                let e = self.acc.clone().unwrap_or(Expr::Undefined);
                if matches!(e, Expr::Undefined) {
                    self.line("return;");
                } else {
                    self.line(&format!("return {};", e.render()));
                }
                self.acc = None;
            }
            "Throw" => {
                let e = self.acc.clone().unwrap_or(Expr::Undefined);
                self.line(&format!("throw {};", e.render()));
                self.acc = None;
            }
            "ReThrow" => {
                // V8 把"当前异常"存在 catch/finally context 的槽 0；重抛读的同一个槽。
                // 槽名与 catch 存储处同名 ⇒ 未发生异常时读到 undefined，此时不重抛
                // （completion-code 派发只重抛"throw"那一路；直接 throw 会误伤正常返回路径）。
                let name = self.context_name(0);
                self.line(&format!(
                    "if ({name} !== undefined) throw {name}; // rethrow（仅当有挂起异常）"
                ));
            }
            "ThrowReferenceErrorIfHole" => {
                let name = idx_num(&arg(0))
                    .map(|i| match self.constant(i) {
                        Expr::Str(s) => s,
                        other => other.render(),
                    })
                    .unwrap_or_default();
                // 名字可能来自未建表的 ro 引用（形如 <ro0_31880>）→ 变量名安全化
                let vname = sanitize_var(&name.replace(['<', '>'], "").replace('/', "_"));
                // V8 的 TDZ 检查紧跟在 context 槽读取之后 → 用它把槽号对应到真名字。
                // 模块级代码大量走外层 context，ScopeInfo 拿不到名字时会退化成 ctxN。
                self.line(&format!(
                    "if ({vname} === undefined) throw new ReferenceError({});",
                    js_string(&name)
                ));
            }
            "ThrowSuperNotCalledIfHole" | "ThrowSuperAlreadyCalledIfNotHole"
            | "ThrowIfNotSuperConstructor" => {
                self.line("/* super call check */");
            }

            // ── 生成器/异步 ──
            "Await" => {
                let e = self.acc.take().unwrap_or(Expr::Hole);
                self.acc = Some(Expr::Await(Box::new(e)));
            }
            "Yield" => {
                let e = self.acc.take().unwrap_or(Expr::Hole);
                self.acc = Some(Expr::Yield(Box::new(e)));
                let v = self.acc.clone().unwrap().render();
                self.line(&format!("{v};"));
                self.acc = None;
            }
            "SuspendGenerator" | "ResumeGenerator" | "SwitchOnGeneratorState" => {
                self.line(&format!("/* generator state: {base} */"));
            }

            // ── switch ──
            "SwitchOnSmiNoFeedback" => {
                if let Some(j) = self.emit_switch(&ins, &ops) {
                    self.skip_to = Some(j);
                }
            }

            // ── class ──
            "DefineClass" | "DefineMethod" | "DefineMethodProperty" | "SetHomeObject"
            | "CreatePrivateProperty" | "DefineKeyedOwnProperty" | "AddPrivateField"
            | "AddPrivateBrand" | "CreatePrivateBrandSymbol" | "CreatePrivateNameSymbol" => {
                self.line(&format!("/* class op: {base} */"));
            }

            // ── 其它常见指令 ──
            // TestUndetectable r：判断 r 是否为"不可检测"值（undefined / document.all）
            "TestUndetectable" => {
                let l = self.operand_expr(&arg(0));
                self.acc = Some(Expr::Bin {
                    op: "===",
                    l: Box::new(l),
                    r: Box::new(Expr::Undefined),
                });
            }
            // CloneObject r：{ ...r }
            "CloneObject" => {
                // `{...src, k: v}` 的 V8 编译结果：先浅克隆 src（原型即 Object.prototype），
                // 再用 StaNamedOwnProperty 补上后面的字面量属性。
                // 源是寄存器操作数（不在 acc）——曾当成 `({ ...undefined })`，属性全丢。
                let src = self.reg_expr(&arg(0));
                self.acc = Some(Expr::Call {
                    callee: Box::new(Expr::Member {
                        obj: Box::new(Expr::Ident("Object".into())),
                        key: Key::Ident("assign".into()),
                    }),
                    args: vec![Expr::ObjectLit(Vec::new()), src],
                    is_new: false,
                    spread_arg: None,
                });
            }
            // delete obj[key] / delete obj.name
            "DeletePropertyStrict" | "DeletePropertySloppy" => {
                // V8: `DeletePropertyStrict <object>, [slot]` —— 键在累加器
                // （曾把对象操作数当删除目标，输出 `delete r2;` → 删的是变量，属性还在）
                let obj = self.reg_expr(&arg(0));
                let key = self.acc.clone().unwrap_or(Expr::Hole);
                let target = Expr::Member {
                    obj: Box::new(obj),
                    key: Key::Computed(Box::new(key)),
                };
                self.acc = Some(Expr::Un {
                    op: "delete ",
                    e: Box::new(target),
                    postfix: false,
                });
                let stmt = self.acc.clone().unwrap();
                self.line(&format!("{};", Self::render_stmt(&stmt)));
                self.acc = None;
            }
            "GetSuperConstructor" => {
                self.acc = Some(Expr::Ident("Object.getPrototypeOf(this)".to_string()));
            }

            // ── for-in ──
            "ForInEnumerate" | "ForInPrepare" | "ForInNext" | "ForInStep" | "ForInContinue"
            | "JumpIfForInDone" | "JumpIfForInDoneConstant" => {
                self.acc = Some(Expr::Ident("__forin_keys".to_string()));
            }

            // ── 其他：安全退化 ──
            "SetPendingMessage" => {}
            "DebugBreak" | "Nop" => {}
            _ => {
                let args = ops.join(", ");
                self.line(&format!(
                    "/* TODO {base}{} */",
                    if args.is_empty() {
                        String::new()
                    } else {
                        comment_safe(&format!(" {}", args))
                    }
                ));
                // 保守假设：可能写 acc（用合法标识符占位，保证语法可解析）
                if !matches!(base.as_str(), "Jump" | "Star") {
                    self.acc = Some(Expr::Ident(format!("__unknown_{base}")));
                }
            }
        }
    }

    fn unary(&mut self, op: &'static str) {
        let e = self.acc.take().unwrap_or(Expr::Hole);
        self.acc = Some(Expr::Un {
            op,
            e: Box::new(e),
            postfix: false,
        });
    }

    /// 寄存器一律按"变量"处理（V8 的字节码就是寄存器机）：
    /// 赋值发射 `rN = <expr>;`，后续读取用 rN，从而在循环/分支间保持正确。
    /// 纯字面量/单次使用的临时值直接内联，避免噪声。
    /// 注意：V8 的 `Star` **不改变累加器**（`LdaGlobal; StarN; Call...` 依赖这一点），
    /// 因此这里只写寄存器、不动 acc。
    fn store_reg(&mut self, r: u32, value: Option<Expr>) {
        let v = value.unwrap_or(Expr::Undefined);
        // 即将覆盖 r：若累加器里还引用着 r（比如刚 Star 过来的 `Reg(r)`、或 `r.x` 这类
        // 表达式），先把它落到临时变量，否则之后用到 acc 时读到的是新值。
        if let Some(acc) = self.acc.clone() {
            let text = acc.render();
            let stored = Self::render_stmt(&v);
            // 待写值就是 acc 本身时不必落临时变量：赋值右侧先求值，写完再由 Star 分支
            // 把 acc 改写成 `Reg(r)`。否则会退化成 `t0 = r0 + x; r0 = r0 + x;`（且重复副作用）
            if text != stored && mentions_reg(&text, r) {
                let tmp = format!("t{}", self.tmp_counter);
                self.tmp_counter += 1;
                self.line(&format!("{tmp} = {text};"));
                self.phi_vars.push(tmp.clone());
                self.acc = Some(Expr::Ident(tmp));
                self.acc_stored = false;
                self.acc_stored_reg = None;
            }
        }
        // 只有覆盖"承载 acc 值的那一个寄存器"才让记录失效（写别的寄存器不影响）；
        // Star 之后会立刻重新标记（它存的正是 acc）。
        if self.acc_stored_reg == Some(r) {
            self.acc_stored = false;
            self.acc_stored_reg = None;
        }
        if self.regs.len() <= r as usize {
            self.regs.resize(r as usize + 1, None);
        }
        // 寄存器一律落成变量：内联字面量会在寄存器被改写后失真
        // （曾导致 `i++` 变成 `r1 = 0 + 1` 的死循环）。寄存器机语义 = 变量语义。
        let name = format!("r{r}");
        // `r = r++` / `r = r--` 是自赋值（保持原值 → 死循环），折叠为 `r++` / `r--`
        if let Expr::Un { op, e, postfix: true } = &v {
            if matches!(&**e, Expr::Reg(x2) if *x2 == r) {
                self.line(&format!("{name}{op};"));
                self.regs[r as usize] = Some(Expr::Reg(r));
                return;
            }
        }
        let rhs = Self::render_stmt(&v);
        self.line(&format!("{name} = {rhs};"));
        self.regs[r as usize] = Some(Expr::Reg(r));
    }

    /// 操作数文本 → 表达式（参数 aN、<this>、寄存器 rN、常量池 [n]、字面量）。
    fn operand_expr(&mut self, s: &str) -> Expr {
        let s = s.trim();
        if s.is_empty() {
            return Expr::Undefined; // 操作数缺失（版本差异）→ 不留空表达式
        }
        if s == "<this>" {
            return Expr::Ident("this".into());
        }
        if s == "<context>" {
            return Expr::Ident("__context".to_string());
        }
        if s == "<closure>" {
            // 脚本体被铺平后没有 `_anon` 这个名字 → 匿名的落到已声明的 __anonymous
            let n = self.fn_name();
            return Expr::Ident(if n.trim().is_empty() {
                "__anonymous".to_string()
            } else {
                sanitize_var(&n)
            });
        }
        if let Some(rest) = s.strip_prefix('a') {
            if let Ok(n) = rest.parse::<i32>() {
                return Expr::Ident(format!("a{n}"));
            }
        }
        if let Some(rest) = s.strip_prefix('r') {
            if let Ok(n) = rest.parse::<u32>() {
                if (n as usize) < self.regs.len() {
                    if let Some(v) = self.regs[n as usize].clone() {
                        return v;
                    }
                }
                return Expr::Reg(n);
            }
        }
        if s.starts_with('[') {
            if let Ok(i) = s.trim_matches(['[', ']']).parse::<usize>() {
                return self.constant(i);
            }
        }
        if let Ok(v) = s.parse::<f64>() {
            return Expr::Num(v);
        }
        Expr::Ident(s.to_string())
    }

    /// 寄存器/参数/立即数 → 表达式（与 operand_expr 同一套规则）。
    fn reg_expr(&mut self, s: &str) -> Expr {
        self.operand_expr(s)
    }

    fn call_args(&mut self, ops: &[String], skip: usize) -> Vec<Expr> {
        ops.iter().skip(skip + 1).map(|s| self.reg_expr(s)).collect()
    }

    /// RegList 操作数展开（"r5-r5" / "r1-r3" / 单个 "r0"）→ 表达式列表。
    fn reglist_exprs(&mut self, ops: &[String], idx: usize) -> Vec<Expr> {
        let Some(s) = ops.get(idx) else {
            return Vec::new();
        };
        let (head, _tail) = match s.split_once('-') {
            Some((a, b)) => (a.to_string(), Some(b.to_string())),
            None => (s.clone(), None),
        };
        if let Some(tail) = _tail {
            let (pfx, a) = split_reg(&head);
            let (_, b) = split_reg(&tail);
            if let (Some(a), Some(b)) = (a, b) {
                if b >= a && b - a < 16 {
                    return (a..=b)
                        .map(|n| self.operand_expr(&format!("{pfx}{n}")))
                        .collect();
                }
            }
        }
        vec![self.operand_expr(&head)]
    }

    /// switch 语句：按跳转表发射 case 本体，并返回 join 的指令下标。
    /// V8 把各 case 体顺序排在 switch 指令之后，每个 case 体以 `Jump <join>` 收尾。
    fn emit_switch(&mut self, ins: &Instr, ops: &[String]) -> Option<usize> {
        let disc = self.acc.clone().unwrap_or(Expr::Hole).render();
        let table_start = ops
            .first()
            .and_then(|s| s.trim_matches(['[', ']']).parse::<usize>().ok())
            .unwrap_or(0);
        let size = ops
            .get(1)
            .and_then(|s| s.trim_matches(['[', ']']).parse::<usize>().ok())
            .unwrap_or(0);
        let mut cases: Vec<(i64, usize)> = Vec::new();
        for i in 0..size {
            if let Some(v) = self
                .pool
                .and_then(|p| self.d.cache.array_elem(p, table_start + i))
                .and_then(|e| e.as_smi())
            {
                let prefix = if ins.scale > 1 { 1 } else { 0 };
                cases.push((i as i64, ins.offset + prefix + v as usize));
            }
        }
        if cases.is_empty() {
            return None;
        }
        cases.sort_by_key(|(_, t)| *t);
        let last_target = cases.last().map(|(_, t)| *t).unwrap_or(ins.offset);

        // join：case 体内 Jump 指向的、超过最后一个 case 起点的最远目标
        let mut join: Option<usize> = None;
        for (_, t) in &cases {
            let Some(&idx) = self.idx_of.get(t) else { continue };
            for j in idx..self.instrs.len().min(idx + 512) {
                if let Some(jt) = self.uncond_jump_target(&self.instrs[j]) {
                    if jt > last_target {
                        if join.map_or(true, |c| jt > c) {
                            join = Some(jt);
                        }
                        break;
                    }
                }
            }
        }

        self.line(&format!("switch ({disc}) {{"));
        self.indent += 1;
        for (k, (value, target)) in cases.iter().enumerate() {
            let Some(&t_idx) = self.idx_of.get(target) else { continue };
            // case 体结束于下一个 case 起点（或 join）
            let body_end = cases
                .get(k + 1)
                .map(|(_, t)| *t)
                .or(join)
                .and_then(|t| self.idx_of.get(&t).copied())
                .unwrap_or(self.instrs.len());
            self.line(&format!("case {value}:"));
            self.indent += 1;
            if let Err(e) = self.emit_range(t_idx, body_end.min(self.instrs.len())) {
                self.line(&format!("/* case body error: {} */", comment_safe(&e)));
            }
            self.line("break;");
            self.indent -= 1;
        }
        self.indent -= 1;
        self.line("}");
        // V8 的 switch 不改动累加器
        join.and_then(|j| self.idx_of.get(&j).copied())
    }
}

/// 把任意文本安全嵌入 `/* ... */` 注释（避免提前闭合）。
fn comment_safe(s: &str) -> String {
    s.replace("*/", "* /")
}

/// 字面量类节点（不能作为赋值基座）。
fn is_literalish(e: &Expr) -> bool {
    matches!(
        e,
        Expr::Num(_)
            | Expr::Str(_)
            | Expr::BigInt(_)
            | Expr::Bool(_)
            | Expr::Null
            | Expr::Undefined
            | Expr::Hole
            | Expr::ObjectLit(_)
            | Expr::ArrayLit(_)
    )
}

/// 是否可作为赋值/自增目标（左值）。`f().x` 合法，`0.x` 不合法。
fn is_lvalue(e: &Expr) -> bool {
    match e {
        Expr::Ident(_) | Expr::Reg(_) | Expr::Temp(_) => true,
        Expr::Member { obj, .. } => !is_literalish(obj),
        _ => false,
    }
}

/// 语句起始处的表达式若以 `{` / `function` / `class` 开头，需要用括号包裹。
fn guard_stmt_start(s: &str) -> String {
    if s.starts_with('{') || s.starts_with("function") || s.starts_with("class") {
        format!("({s})")
    } else {
        s.to_string()
    }
}

/// 拆分寄存器名 → (前缀, 编号)，如 "r5" / "a2"。
fn split_reg(s: &str) -> (char, Option<u32>) {
    let mut chars = s.chars();
    let p = chars.next().unwrap_or('r');
    (p, s[1..].parse().ok())
}

fn binop_of(name: &str) -> &'static str {
    match name {
        "Add" => "+",
        "Sub" => "-",
        "Mul" => "*",
        "Div" => "/",
        "Mod" => "%",
        "Exp" => "**",
        "BitwiseOr" => "|",
        "BitwiseXor" => "^",
        "BitwiseAnd" => "&",
        "ShiftLeft" => "<<",
        "ShiftRight" => ">>",
        "ShiftRightLogical" => ">>>",
        _ => "+",
    }
}

fn cmp_of(name: &str) -> &'static str {
    match name {
        "TestEqual" => "==",
        "TestEqualStrict" | "TestReferenceEqual" => "===",
        "TestLessThan" => "<",
        "TestLessThanOrEqual" => "<=",
        "TestGreaterThan" => ">",
        "TestGreaterThanOrEqual" => ">=",
        "TestInstanceOf" => "instanceof",
        "TestIn" => "in",
        _ => "===",
    }
}

fn regexp_flags(ops: &[String]) -> String {
    let mut flags = String::new();
    let bits: u32 = ops
        .iter()
        .find(|s| s.starts_with('#'))
        .and_then(|s| s[1..].parse().ok())
        .unwrap_or(0);
    if bits & 1 != 0 {
        flags.push('g');
    }
    if bits & 2 != 0 {
        flags.push('i');
    }
    if bits & 4 != 0 {
        flags.push('m');
    }
    if bits & 8 != 0 {
        flags.push('u');
    }
    if bits & 16 != 0 {
        flags.push('s');
    }
    if bits & 32 != 0 {
        flags.push('y');
    }
    if bits & 64 != 0 {
        flags.push('d');
    }
    flags
}

/// 读取 BytecodeArray 的 handler 表（异常处理区间）。
fn read_handler_table<'a>(d: &Decompiler<'a>, bca: ObjId) -> Vec<Handler> {
    let ts = d.ts;
    let Some(h) = d
        .cache
        .slot_at(bca, d.dis.bca_handler_table_slot())
        .and_then(|v| v.as_ref())
        .and_then(|r| d.cache.ref_object(r))
    else {
        return Vec::new();
    };
    let len = d.cache.array_len(h);
    if len < 6 {
        return Vec::new();
    }
    let rd = |i: usize| -> Option<i32> {
        d.cache
            .raw_at_ts(h, i, 4, ts)
            .map(|x| i32::from_le_bytes(x.try_into().unwrap()))
    };
    let mut out = Vec::new();
    // 布局：[start, end, handler, depth, ...]
    let mut i = 0usize;
    while i + 12 <= len {
        let (Some(start), Some(end), Some(target), Some(depth)) =
            (rd(i), rd(i + 4), rd(i + 8), rd(i + 12).or(Some(0)))
        else {
            break;
        };
        out.push(Handler {
            start: start as u32,
            end: end as u32,
            target: target as u32,
            depth: (depth as u32) / 2,
        });
        i += 16;
    }
    out
}

/// 把 `__uncompiled.<name>` 换成本文件里真实存在的摊平函数名。
///
/// 码缓存里对 SFI 常量的引用很多（模块 CI、类方法表、导出表）。这些函数在摊平输出里
/// 就是文件级定义，引用占位代理会让"函数明明定义了却调不到"。名字不存在时保留占位
/// （占位是安全的空实现，直接换成裸标识符会 ReferenceError）。
pub fn link_flat_functions(text: String) -> String {
    if !text.contains("__uncompiled.") {
        return text;
    }
    // 收集文件级定义名：function X / var X / let X / const X（含 `var X = class`）
    let mut defined: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for line in text.lines() {
        let l = line.trim_start();
        for kw in ["function ", "var ", "let ", "const "] {
            if let Some(rest) = l.strip_prefix(kw) {
                let name: String = rest
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '$')
                    .collect();
                if !name.is_empty() {
                    defined.insert(Box::leak(name.into_boxed_str()));
                }
                break;
            }
        }
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text.as_str();
    while let Some(pos) = rest.find("__uncompiled.") {
        out.push_str(&rest[..pos]);
        let after = &rest[pos + "__uncompiled.".len()..];
        let name: String = after
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '$')
            .collect();
        if !name.is_empty() && defined.contains(name.as_str()) {
            out.push_str(&name);
        } else {
            out.push_str("__uncompiled.");
            out.push_str(&name);
        }
        rest = &after[name.len()..];
    }
    out.push_str(rest);
    out
}

/// 该名字是不是可以直接写成裸标识符的 JS 全局（其余 V8 根只作注释）。
fn is_js_global(n: &str) -> bool {
    matches!(
        n,
        "Object" | "Function" | "Array" | "Number" | "parseFloat" | "parseInt" | "Infinity"
            | "NaN" | "undefined" | "Boolean" | "String" | "Symbol" | "Date" | "Promise"
            | "RegExp" | "Error" | "AggregateError" | "EvalError" | "RangeError"
            | "ReferenceError" | "SyntaxError" | "TypeError" | "URIError" | "globalThis"
            | "JSON" | "Math" | "Intl" | "ArrayBuffer" | "SharedArrayBuffer" | "Atomics"
            | "Uint8Array" | "Int8Array" | "Uint16Array" | "Int16Array" | "Uint32Array"
            | "Int32Array" | "Float32Array" | "Float64Array" | "Uint8ClampedArray"
            | "BigInt" | "BigInt64Array" | "BigUint64Array" | "Map" | "Set" | "WeakMap"
            | "WeakSet" | "WeakRef" | "FinalizationRegistry" | "DataView" | "Proxy"
            | "Reflect" | "decodeURI" | "decodeURIComponent" | "encodeURI"
            | "encodeURIComponent" | "escape" | "unescape" | "isFinite" | "isNaN"
            | "eval" | "structuredClone" | "queueMicrotask" | "process" | "Buffer"
            | "URL" | "URLSearchParams" | "TextEncoder" | "TextDecoder" | "AbortController"
            | "AbortSignal" | "Event" | "EventTarget" | "MessageChannel" | "MessagePort"
            | "console" | "setTimeout" | "setInterval" | "clearTimeout" | "clearInterval"
            | "setImmediate" | "clearImmediate" | "require" | "module" | "exports"
    )
}

/// 文本里是否引用了寄存器 `rN`（词边界匹配，`r1` 不会命中 `r10`）。
fn mentions_reg(text: &str, r: u32) -> bool {
    let needle = format!("r{r}");
    let bytes = text.as_bytes();
    let mut from = 0;
    while let Some(pos) = text[from..].find(&needle) {
        let start = from + pos;
        let end = start + needle.len();
        let before_ok = start == 0 || !is_ident_byte(bytes[start - 1]);
        let after_ok = end >= bytes.len() || !is_ident_byte(bytes[end]);
        if before_ok && after_ok {
            return true;
        }
        from = end;
    }
    false
}

fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'$'
}

/// 手工兜底名单（版本表缺失该字节码、或 acc 标记为 "rw" 有歧义时使用）。
fn name_was_preserving(name: &str) -> bool {
    matches!(
        name,
        "Star" | "Mov" | "PushContext" | "PopContext" | "Jump" | "JumpLoop" | "Nop"
            | "Debugger" | "SetPendingMessage" | "ThrowReferenceErrorIfHole"
            | "ThrowSuperNotCalledIfHole" | "ThrowSuperAlreadyCalledIfNotHole"
            | "ThrowIfNotSuperConstructor" | "ReThrow" | "CreateBlockContext"
            | "CreateFunctionContext" | "CreateCatchContext" | "CreateWithContext"
            | "CreateEvalContext" | "CreateScriptContext" | "SwitchOnGeneratorState"
            | "SuspendGenerator" | "ResumeGenerator" | "IncBlockCounter"
            | "StaCurrentContextSlot" | "StaContextSlot" | "StaCurrentScriptContextSlot"
            | "StaScriptContextSlot" | "StaGlobal" | "StaLookupSlot"
            | "StaNamedProperty" | "SetNamedProperty" | "StaNamedOwnProperty"
            | "DefineNamedOwnProperty" | "StaKeyedProperty" | "SetKeyedProperty"
            | "StaDataPropertyInLiteral" | "DefineKeyedOwnPropertyInLiteral"
            | "StaInArrayLiteral" | "DefineKeyedOwnProperty" | "CollectTypeProfile"
    ) || name.starts_with("Star") && name[4..].chars().all(|c| c.is_ascii_digit())
}
