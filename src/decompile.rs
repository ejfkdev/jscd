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
                if let Some(sp) = spread_arg {
                    let _ = write!(out, "...{}", sp.render());
                    first = false;
                }
                for a in args {
                    if !first {
                        out.push_str(", ");
                    }
                    first = false;
                    a.write_to(out, 0);
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
    pub fn render_all<W: FmtWrite>(&self, w: &mut W, filter: Option<&str>) -> Result<(), String> {
        // 前言：把各作用域的 context 变量提升为文件级 var。
        // 我们的输出把嵌套函数摊平成顶层函数，捕获变量只能靠共享的全局绑定才可运行。
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
            "// 共享绑定（闭包捕获的变量被摊平为文件级 var，便于直接运行）\nvar {};\nvar __context;\n\n",
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
        ctx.run()?;
        Ok(ctx.finish())
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
            out: String::new(),
            indent: 0,
            loops: Vec::new(),
            temps: Vec::new(),
            handlers,
            ctx_names: Vec::new(),
            ctx_scopes: Vec::new(),
            label_counter: 0,
            tmp_counter: 0,
            name,
            is_async: false,
            is_generator: false,
        })
    }

    fn render_operand(&self, op: &Operand) -> String {
        let dec = Decoder::new(self.d.table, self.d.layout, 0);
        dec.render_operand(op)
    }

    fn line(&mut self, s: &str) {
        for _ in 0..self.indent {
            self.out.push_str("  ");
        }
        self.out.push_str(s);
        self.out.push('\n');
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
        // 类构造器/方法必须在 class 体内才是合法语法 → 包成 class 表达式
        let is_class_ctor = self.scope.as_ref().map(|s| s.is_class_constructor()).unwrap_or(false);
        if is_class_ctor {
            let fname = sanitize_var(&self.fn_name());
            self.out.push_str(&format!("const __class_{fname}_{} = class {{\n", self.sfi));
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
        self.out.push_str("}\n");
        if is_class_ctor {
            self.out.push_str("};\n");
        }
        self.out.push('\n');
        Ok(())
    }

    fn finish(self) -> String {
        self.out
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
        // 兜底：从 BCA 的 parameter_size 推
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
                Some(name) => {
                    if is_ident(name) {
                        Expr::Str(name.to_string())
                    } else {
                        Expr::Ident(format!("/*ro*/ {name}"))
                    }
                }
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
            _ if is_ident(n) => Expr::Ident(n.to_string()),
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
                "/* function {} */ __uncompiled_{}",
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
            // 通常为 switch 跳转表；按数组字面量呈现
            let n = self.d.cache.array_len(o);
            let items: Vec<String> = (0..n.min(16))
                .map(|i| format!("/*{}*/", i))
                .collect();
            return Expr::Ident(format!("[{}]", items.join(", ")));
        }
        Expr::Ident(format!("__const_{o}"))
    }

    /// 对象字面量（ObjectBoilerplateDescription：keys + values 两个数组交替）。
    fn object_boilerplate(&mut self, o: ObjId) -> Expr {
        let len = self.d.cache.array_len(o);
        let mut parts = Vec::new();
        // 布局：[count, key0, val0, key1, val1, ...]（键在 FixedArray 的后半段）
        let half = (len.saturating_sub(1)) / 2;
        for i in 0..half.min(32) {
            let key_idx = 1 + i;
            let val_idx = 1 + half + i;
            let key = match self.d.cache.array_elem(o, key_idx) {
                Some(Elem::Smi(v)) => Some(v.to_string()),
                Some(Elem::Ref(r)) => crate::disasm::name_of_ref(self.d.cache, self.d.table, r),
                _ => None,
            };
            let val = match self.d.cache.array_elem(o, val_idx) {
                Some(Elem::Smi(v)) => Expr::Num(v as f64),
                Some(Elem::Ref(Ref::Object(v))) => self.object_constant(v),
                Some(Elem::Ref(Ref::Root(i))) => self.root_value(i),
                _ => Expr::Undefined,
            };
            let k = match key {
                Some(k) if is_ident(&k) => k,
                Some(k) => format!("[{}]", js_string(&k)),
                None => "[/*unknown*/0]".to_string(),
            };
            parts.push((k, val));
        }
        Expr::ObjectLit(parts)
    }

    /// 数组字面量（ArrayBoilerplateDescription：常量池元素序列）。
    fn array_boilerplate(&mut self, o: ObjId) -> Expr {
        let len = self.d.cache.array_len(o);
        let mut items = Vec::new();
        for i in 0..len.min(32) {
            match self.d.cache.array_elem(o, i) {
                Some(Elem::Smi(v)) => items.push(Expr::Num(v as f64)),
                Some(Elem::Ref(r)) => {
                    let s = crate::disasm::name_of_ref(self.d.cache, self.d.table, r);
                    items.push(match s {
                        Some(n) => Expr::Str(n),
                        None => Expr::Undefined,
                    });
                }
                _ => items.push(Expr::Undefined),
            }
        }
        Expr::ArrayLit(items)
    }

    /// context 槽名（ScopeInfo 的 context_local_names；不可得时回落 ctxN）。
    fn context_name(&self, slot: usize) -> String {
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
        format!("ctx{slot}")
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
    fn reads_acc(name: &str) -> bool {
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

            // ③ 条件跳转
            if let Some(target) = self.cond_jump_target(&ins) {
                let cond = self.cond_of(&base);
                self.flush_acc_before(&base);
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
                        self.line(&format!("if (!({cond})) {{"));
                        self.indent += 1;
                        self.emit_range(i + 1, t_idx.min(end))?;
                        self.indent -= 1;
                        let mut next = t_idx;
                        // 紧邻 target 之前的无条件 Jump → else 分支
                        if t_idx > 0 {
                            if let Some(else_target) = self.uncond_jump_target(&self.instrs[t_idx - 1])
                            {
                                if else_target > target {
                                    if let Some(&e_idx) = self.idx_of.get(&else_target) {
                                        self.line("} else {");
                                        self.indent += 1;
                                        self.emit_range(t_idx, e_idx.min(end))?;
                                        self.indent -= 1;
                                        next = e_idx;
                                    }
                                }
                            }
                        }
                        self.line("}");
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
        }
        Ok(())
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

    /// 条件表达式（由跳转类型 + acc 推出）。
    fn cond_of(&self, base: &str) -> String {
        let acc = self
            .acc
            .as_ref()
            .map(|e| e.render())
            .unwrap_or_else(|| "true".into());
        match base {
            "JumpIfTrue" | "JumpIfToBooleanTrue" => acc,
            "JumpIfFalse" | "JumpIfToBooleanFalse" => format!("!({acc})"),
            "JumpIfNull" => format!("({acc}) === null"),
            "JumpIfNotNull" => format!("({acc}) !== null"),
            "JumpIfUndefined" => format!("({acc}) === undefined"),
            "JumpIfNotUndefined" => format!("({acc}) !== undefined"),
            "JumpIfUndefinedOrNull" => format!("({acc}) == null"),
            "JumpIfJSReceiver" => format!("typeof ({acc}) === \"object\""),
            "JumpIfNotHole" => format!("({acc}) !== undefined"),
            "JumpIfReferenceError" => format!("/* reference error */({acc})"),
            "JumpIfNotReferenceError" => format!("/* ok */({acc})"),
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
    fn preserves_acc(name: &str) -> bool {
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

    /// acc 若已死且带副作用 → 单独成句。
    fn flush_acc_before(&mut self, next_base: &str) {
        // 只有"覆盖 acc 且不读 acc"才说明旧值已死；保留 acc 的指令（Mov/Star/Jump/存储…）不能 flush
        if Self::reads_acc(next_base) || Self::preserves_acc(next_base) {
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
        if !Self::reads_acc(&base) {
            self.flush_acc_before(&base);
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
            }
            // 短 Star：StarN 直接编码寄存器号
            _ if base.starts_with("Star") && base[4..].chars().all(|c| c.is_ascii_digit()) => {
                let r: u32 = base[4..].parse().unwrap_or(0);
                self.store_reg(r, self.acc.clone());
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
            // ++/-- 只能作用于左值；否则退化为 `+1`/`-1`（保持合法）
            "Inc" => {
                let e = self.acc.take().unwrap_or(Expr::Hole);
                self.acc = Some(if is_lvalue(&e) && !is_literalish(&e) {
                    Expr::Un {
                        op: "++",
                        e: Box::new(e),
                        postfix: true,
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
                        postfix: true,
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
                // 无语义损失：保持原表达式
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
                let n = match base.as_str() {
                    "CallProperty0" => 0,
                    "CallProperty1" => 1,
                    "CallProperty2" => 2,
                    _ => ops.len().saturating_sub(3),
                };
                let args: Vec<Expr> = (0..n)
                    .map(|k| self.operand_expr(&arg(2 + k)))
                    .collect();
                self.acc = Some(Expr::Call {
                    callee: Box::new(callee),
                    args,
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
                let callee = self.acc.clone().unwrap_or(Expr::Hole);
                let spread = self.operand_expr(&arg(1));
                self.acc = Some(Expr::Call {
                    callee: Box::new(callee),
                    args: Vec::new(),
                    is_new: false,
                    spread_arg: Some(Box::new(spread)),
                });
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
                let spread = self.operand_expr(&arg(1));
                self.acc = Some(Expr::Call {
                    callee: Box::new(callee),
                    args: Vec::new(),
                    is_new: true,
                    spread_arg: Some(Box::new(spread)),
                });
            }
            "CallRuntime" | "CallJSRuntime" => {
                let name = ops.first().cloned().unwrap_or_else(|| "runtime".into());
                let name = name.trim_matches(['[', ']']).to_string();
                let args: Vec<Expr> = ops
                    .iter()
                    .skip(1)
                    .filter(|s| s.contains('-') || s.starts_with('r') || s.starts_with('a'))
                    .map(|s| self.reg_expr(s))
                    .collect();
                self.acc = Some(Expr::Call {
                    callee: Box::new(Expr::Ident(format!("__runtime_{name}"))),
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
                self.acc = Some(Expr::Call {
                    callee: Box::new(Expr::Ident(format!("__intrinsic_{name}"))),
                    args: Vec::new(),
                    is_new: false,
                    spread_arg: None,
                });
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
                self.acc = Some(Expr::ArrayLit(Vec::new())); // rest 参数占位
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
                let e = self.acc.clone().unwrap_or(Expr::Hole);
                self.acc = Some(Expr::Call {
                    callee: Box::new(Expr::Member {
                        obj: Box::new(e),
                        key: Key::Ident("Symbol.iterator".into()),
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
                self.line("throw e; // rethrow");
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
                self.emit_switch(&ins, &ops);
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
                let src = self.acc.clone().unwrap_or(Expr::Hole);
                self.acc = Some(Expr::ObjectLit(vec![(String::new(), src.clone())]));
            }
            // delete obj[key] / delete obj.name
            "DeletePropertyStrict" | "DeletePropertySloppy" => {
                let target = self.operand_expr(&arg(0));
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
            return Expr::Ident(sanitize_var(&self.fn_name()));
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

    /// switch 语句（跳转表 → case 分支）。
    fn emit_switch(&mut self, ins: &Instr, ops: &[String]) {
        let disc = self.acc.clone().unwrap_or(Expr::Hole).render();
        let table_start = ops
            .first()
            .and_then(|s| s.trim_matches(['[', ']']).parse::<usize>().ok())
            .unwrap_or(0);
        let size = ops
            .get(1)
            .and_then(|s| s.trim_matches(['[', ']']).parse::<usize>().ok())
            .unwrap_or(0);
        let mut cases = Vec::new();
        for i in 0..size {
            let v = self
                .pool
                .and_then(|p| self.d.cache.array_elem(p, table_start + i))
                .and_then(|e| e.as_smi());
            if let Some(v) = v {
                cases.push((i as i64, ins.offset + v as usize));
            }
        }
        self.line(&format!("switch ({disc}) {{"));
        self.indent += 1;
        if cases.is_empty() {
            self.line("default: /* 跳转表为空 */");
            self.indent += 1;
            self.line("break;");
            self.indent -= 1;
        }
        for (case, target) in &cases {
            self.line(&format!("case {case}: /* → @{target} */"));
            self.indent += 1;
            self.line("break;");
            self.indent -= 1;
        }
        self.indent -= 1;
        self.line("} /* 各分支本体见下方（V8 将 case 体顺序排在 switch 之后） */");
        self.acc = None;
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