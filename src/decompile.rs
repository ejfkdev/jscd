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
use std::collections::{HashMap, HashSet};
use std::fmt::Write as FmtWrite;

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
    YieldStar(Box<Expr>),
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
        // `delete x.y` 有副作用（删属性），也要能作为语句被 flush/保留
        if matches!(self, Expr::Un { op, .. } if *op == "delete ") {
            return true;
        }
        match self {
            Expr::Call { .. } => true,
            Expr::Assign { .. } => true,
            Expr::Await(_) | Expr::Yield(_) | Expr::YieldStar(_) => true,
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
                    // `-0` 必须保住符号：`as i64` 会把它印成 `0`（`Object.is(x, -0)` 就错）
                    if *v == 0.0 && v.is_sign_negative() {
                        out.push_str("-0");
                    } else {
                        let _ = write!(out, "{}", *v as i64);
                    }
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
            Expr::YieldStar(e) => {
                out.push_str("yield* ");
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
            Expr::YieldStar(_) => 2,
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
/// 匿名 SFI 的稳定唯一名：同一份产物里每个匿名函数各不相同，
/// 闭包引用（CreateClosure）与函数定义用同一个名字才能对上。
fn anon_name(sfi: ObjId) -> String {
    format!("_anon_{sfi}")
}

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
        matches!(self.function_kind, 5..=10)
    }
    pub fn is_class_constructor(&self) -> bool {
        matches!(self.function_kind, 11..=13)
    }
}

/// 读取 ScopeInfo（布局随版本变，见 docs/VERSIONS.md §7）。
/// 读 ScopeInfo → Scope（变量名表）。
///
/// 数值域起点（Flags 在哪个槽）随版本变：≤8.4 是槽 2，9.x+ 是槽 1 —— 但 **9.0 例外**
/// （实测 node16.3 的 ScopeInfo 比 9.1+ 多一格）。这里两个起点都试，按**解出来的变量名
/// 数量**挑：起点错了名字表就整体错位，`context_locals` 会几乎全空（closure 的 `n`
/// 于是成了 `__ctx.ctx2`）。
pub fn read_scope<'a>(
    cache: &CodeCache<'a>,
    table: &VersionTable,
    ts: usize,
    scope_id: ObjId,
    ro_map: Option<&RoMap>,
) -> Option<Scope> {
    let legacy = table.v8.starts_with('8')
        || table.v8.starts_with('7')
        || table.v8.starts_with('6');
    let primary = if legacy { 2 } else { 1 };
    let mut best: Option<Scope> = None;
    let mut best_score = -1i32;
    for base in [primary, if primary == 1 { 2 } else { 1 }] {
        if let Some(scope) = read_scope_with(cache, table, ts, scope_id, ro_map, base) {
            let named = scope.context_locals.iter().filter(|n| !n.is_empty()).count() as i32;
            let score = named + if scope.name.is_empty() { 0 } else { 1 };
            if score > best_score {
                best_score = score;
                best = Some(scope);
            }
        }
    }
    best
}

fn read_scope_with<'a>(
    cache: &CodeCache<'a>,
    table: &VersionTable,
    ts: usize,
    scope_id: ObjId,
    ro_map: Option<&RoMap>,
    base: usize,
) -> Option<Scope> {
    let cfg = table.scope_info.clone()?;
    // 数值域起点随版本变：
    //   ≤8.4：ScopeInfo 是 FixedArray，头两格 map+length → Flags 在**槽 2**、
    //         ParameterCount 槽 3、ContextLocalCount 槽 4，变量区从槽 5 起；
    //         且数值域由 `FOR_EACH_SCOPE_INFO_NUMERIC_FIELD` 宏生成（`Smi::ToInt`），
    //         表里 `flags_smi` 标成了 false，实测低 4 字节恒 0 → 按 Smi 读。
    //   9.x+ ：Flags 在槽 1（本代码长期验证过的路径）。
    let legacy = base == 2;
    let flags = if legacy || cfg.flags_smi {
        cache
            .raw_at_ts(scope_id, base * ts, ts, ts)
            .and_then(crate::serializer::decode_smi_bytes)? as u64
    } else {
        u32::from_le_bytes(cache.raw_at_ts(scope_id, ts, 4, ts)?.try_into().ok()?) as u64
    };
    // 9.x–12.x：flags(Smi)@ts | param_count@2ts | context_local_count@3ts
    // 13.x：    flags(u32)+padding@ts..ts+8 | param_count@ts+8 | context_local_count@ts+16
    // ≤6.x：数值域多一格 StackLocalCount（V8 `FOR_EACH_SCOPE_INFO_NUMERIC_FIELD`：
    //        Flags/ParameterCount/StackLocalCount/ContextLocalCount），
    //        于是 ContextLocalCount@5、变量区@6；ParameterCount 仍在 clc_slot-2。
    let clc_slot = if legacy {
        cfg.context_local_count_slot
            .unwrap_or(if cfg.parameter_names_first { 5 } else { 4 })
    } else {
        cfg.context_local_count_slot.unwrap_or(3)
    };
    let (param_off, clc_off) = if cfg.position_info_early {
        (ts + 8, ts + 16)
    } else if cfg.parameter_names_first {
        // 6.x：数值域多一格 StackLocalCount（Flags/Param/Stack/CLC）→ ParamCount 在 CLC 前两格
        ((clc_slot - 2) * ts, clc_slot * ts)
    } else {
        // 7.8–12.x：Flags/ParameterCount/ContextLocalCount 连着放 → ParamCount 在 CLC 前一格。
        // 用 -2 会读到 Flags（node12/16/22 的 param 读成 47554/94658 这种"旗标值"）。
        ((clc_slot - 1) * ts, clc_slot * ts)
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
    let vpart_slot = if legacy {
        cfg.variable_part
            .unwrap_or(if cfg.parameter_names_first { 6 } else { 5 })
    } else {
        cfg.variable_part.unwrap_or(4)
    };
    let mut off = vpart_slot * ts;
    if cfg.parameter_names_first {
        // ≤6.x 变量区顺序：形参名(ParameterCount) + 栈局部首槽(1) + 栈局部名(StackLocalCount)
        // + context 名 + context 信息 …（V8 `ScopeInfo::ContextLocalNamesIndex()`）。
        // 少了这一步就会把形参名当成 context 局部名（node10 的函数名因此读成局部变量名）。
        let stack = cache
            .raw_at_ts(scope_id, (clc_slot - 1) * ts, ts, ts)
            .and_then(crate::serializer::decode_smi_bytes)
            .unwrap_or(0) as usize;
        off += (param_count as usize + 1 + stack) * ts;
    }
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

    if std::env::var("JSCD_DBG_SCOPE").is_ok() {
        let raw: Vec<String> = (0..cache.array_len(scope_id).min(16))
            .map(|i| match cache.array_elem(scope_id, i) {
                Some(Elem::Smi(v)) => format!("smi{v}"),
                Some(Elem::Ref(r)) => {
                    crate::disasm::name_of_ref(cache, table, r).unwrap_or_else(|| format!("{r:?}"))
                }
                other => format!("{other:?}"),
            })
            .collect();
        eprintln!("[scope-raw] id={scope_id} elems={raw:?}");
        eprintln!(
            "[scope] id={scope_id} len={} base={base} flags={flags:#x} scope_type={} param={param_count} clc={n} varpart_off={names_off} inlined={inlined} names={:?}",
            cache.array_len(scope_id),
            flags & 0xF,
            (0..n.min(6))
                .map(|i| {
                    let slot = (names_off / ts) + i;
                    match cache.slot_at(scope_id, slot).and_then(|v| v.as_ref()) {
                        Some(Ref::RoRef(c, o)) => ro_map
                            .and_then(|m| m.get(c, o))
                            .map(|s| s.to_string())
                            .unwrap_or_else(|| format!("<ro{c}_{o}>")),
                        Some(r) => crate::disasm::name_of_ref(cache, table, r).unwrap_or_default(),
                        None => String::from("?"),
                    }
                })
                .collect::<Vec<_>>()
        );
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
                Some(Ref::RoRef(c, off)) => {
                    // 11.3+ 的 context 局部名常是只读堆字符串 → 只能靠 ro-map 还原；
                    // 名字解不出会连带"共享绑定"前言为空，捕获变量全部丢失（closure 就栽在这）。
                    scope.context_locals.push(
                        ro_map
                            .and_then(|m| m.get(c, off))
                            .map(|s| s.to_string())
                            .unwrap_or_default(),
                    );
                }
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
    if std::env::var("JSCD_DBG_SCOPE").is_ok() {
        eprintln!(
            "[scope-outer] id={scope_id} flags={flags:#x} bit={} cursor_slot={} len={}",
            cfg.has_outer_bit.unwrap_or(if table.v8.starts_with('6') { 20 } else if table.v8.starts_with('7') { 21 } else { 22 }),
            cursor / ts,
            cache.array_len(scope_id)
        );
    }
    let has_outer = {
        // `HasOuterScopeInfo` 的 flag 位随版本变（V8 scope-info.h 的字段顺序）：
        //   6.x = 20（FunctionKind 5 位在 15..19）、7.8 = 21（多一个 HasClassBrand）、
        //   8.x+ = 22（再多一个字段）。写死 22 会让 6.2/6.8/7.8 的 outer 链读不到 ——
        //   闭包捕获的槽于是只剩 `__ctx.ctxN`，与外层写的名字对不上（arrow_opt 的
        //   `nest(2)(3)` 因此算成 NaN）。
        let bit = cfg
            .has_outer_bit
            .unwrap_or(if table.v8.starts_with('6') {
                20
            } else if table.v8.starts_with('7') {
                21
            } else {
                22
            });
        (flags >> bit) & 1 == 1
    };
    if has_outer {
        if let Some(Ref::Object(o)) = cache.slot_at(scope_id, cursor / ts).and_then(|v| v.as_ref()) {
            if cache.obj(o).ty.is(table, "ScopeInfo") {
                scope.outer = Some(o);
            }
        }
    }
    // 兜底：老族（6.x/7.8）的 flag 位与变量区排布和 8.x+ 不一致（6.2 的 FunctionKind
    // 是 **10 位** → HasOuterScopeInfo 在 25 而不是 22；ReceiverInfo/PositionInfo 有无
    // 也不同）→ 按 flag+cursor 读不到。直接在变量区里扫一个 ScopeInfo 引用：外层作用域
    // 是本作用域变量区里唯一的 ScopeInfo，而且这里只拿它解析变量名（有类型校验）。
    if scope.outer.is_none() {
        let start = names_off / ts;
        let end = cache.array_len(scope_id) + 2;
        for k in (start..end).rev() {
            if let Some(Ref::Object(o)) = cache.slot_at(scope_id, k).and_then(|v| v.as_ref()) {
                if o != scope_id && cache.obj(o).ty.is(table, "ScopeInfo") {
                    scope.outer = Some(o);
                    break;
                }
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
    /// 内嵌表里按 **精确 V8 版本**取只读堆名表（`jscd` 自带，Node 22+ 不必再手动 `--ro-map`）。
    ///
    /// 必须精确匹配：只读堆地址 (chunk/offset) 是按 V8 版本编号的，同 minor 的不同 patch
    /// 都可能不同 —— 错配会**静默给出错误的属性名**，比留 `<ro0_…>` 占位更糟。
    pub fn embedded(v8: &str) -> Option<Self> {
        let text = crate::ro_embed::RO_MAPS
            .iter()
            .find(|(m, _)| *m == v8)
            .map(|(_, t)| *t)?;
        Self::from_json(text).ok()
    }

    pub fn load(path: &std::path::Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        Self::from_json(&text).map_err(|e| format!("{}: {e}", path.display()))
    }

    pub fn from_json(text: &str) -> Result<Self, String> {
        let v: serde_json::Value =
            serde_json::from_str(text).map_err(|e| e.to_string())?;
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
    /// 只读堆名表（可选）；与 `dis.ro_map` 共享同一份（13.x 的 SFI 名字也要用它）
    ro_map: Option<std::rc::Rc<RoMap>>,
    /// 字节码 → (读 acc, 写 acc)：来自版本表（codegen 从 bytecodes.h 提取）
    acc_use: HashMap<String, (bool, bool)>,
    /// 正在解码字面量的常量池条目。6.x 的对象字面量常量（BoilerplateDescription）与
    /// 顶层脚本的 DeclareGlobals 数组**都是 FixedArray**，只有调用点知道是哪一种。
    lit_ctx: std::cell::Cell<bool>,
    /// bytenode 用 `Module.wrap` 包出来的 CommonJS 包装函数（`(exports, require,
    /// module, __filename, __dirname) => …`）对应哪个 SFI。
    ///
    /// `.jsc` 顶层因此是一个"闭包工厂"（脚本体只有 `LdaConstant [k]; Return`），
    /// 而 bytenode 运行时真正调用的是那个包装函数 —— 不把它摊平，产物加载后什么都不执行。
    wrapper_sfi: std::cell::Cell<Option<ObjId>>,
    /// 是否输出"可运行前导"（`__runtime`/`__intrinsic` 占位实现 + 上下文变量提升 + 别名块）。
    /// 默认 **false**：只给原始代码本身（剥离运行时噪声）；`jscd --runtime` 打开。
    runtime: std::cell::Cell<bool>,
    /// SFI → 发射名（重名去重）。V8 的推断名会撞车（`const nest = (x) => (y) => x*y`
    /// 里内外层箭头都叫 `nest`）—— 同名两条 `function nest()` 声明会互相覆盖，
    /// 引用（DeclareGlobals/闭包常量）也就指向错的那个（arrow_opt 的 `nest(2)(3)` = NaN）。
    emit_names: std::cell::RefCell<Option<HashMap<ObjId, String>>>,
}

/// V8 14 起脚本上下文不再用 cell：`LdaCurrentScriptContextSlot` 等六条被
/// `*ContextSlotNoCell` 取代（操作数形状逐条对齐：1/3 个操作数都一样）。
/// 解码后统一折回老名字，后面那一大批按名字分派的逻辑不必逐处加分支；
/// 反汇编（`jscd disasm`）仍按表中的真名输出，保持如实。
///
/// 另外几条 14.x 新指令在这里一并给出等价的老名字/直译：
/// - `CreateFunctionContextWithCells` ≡ `CreateFunctionContext`（槽是 cell，不影响命名）
/// - `Add_StringConstant_Internalize` 由渲染层按"寄存器 + 常量"处理（见 `render_ins`）
pub(crate) fn canonical_name(name: &str) -> &str {
    match name {
        // `*NoCell` 是"**普通**上下文槽"的 V8 14 拼写（14.x 的头文件里
        // `LdaContextSlotNoCell` 与 `LdaContextSlot` 并存、操作数形状一致；cells 被删了）。
        // 早先当成 `*ScriptContextSlot`（脚本上下文，基准 +1）是错的 —— 那会整体错一槽。
        "LdaCurrentContextSlotNoCell" => "LdaCurrentContextSlot",
        "LdaContextSlotNoCell" => "LdaContextSlot",
        "StaContextSlotNoCell" => "StaContextSlot",
        "StaCurrentContextSlotNoCell" => "StaCurrentContextSlot",
        "LdaLookupContextSlotNoCell" => "LdaLookupContextSlot",
        "LdaLookupContextSlotNoCellInsideTypeof" => "LdaLookupContextSlotInsideTypeof",
        "CreateFunctionContextWithCells" => "CreateFunctionContext",
        _ => name,
    }
}

/// 循环上下文（break/continue 目标）。
#[derive(Debug, Clone)]
struct LoopCtx {
    /// continue 跳转目标（= 循环判断处）
    continue_target: usize,
    /// break 跳转目标（= 循环结束）
    break_target: usize,
    /// 循环标签（内层 `break outer` 需要 `break L1;`）。没嵌套循环是为空串。
    label: String,
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
    /// 上一条 / 当前指令的 base 名（TDZ 检查要回看前一条是不是 context 读取）
    prev_base: String,
    cur_base: String,
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
    /// 本函数是上述"闭包工厂"（只创建包装函数并返回）—— 不输出，避免多一个没人调的函数
    wrapper_factory: bool,
    /// 最近一次 context 槽读取的槽号（供紧随其后的 TDZ 检查反推变量名）
    last_ctx_slot: Option<usize>,
    /// 槽号 → 变量名（由 TDZ 检查常量池反推，弥补外层 ScopeInfo 缺失）
    slot_aliases: HashMap<usize, String>,
    /// 生成器序言把参数搬进 context（`Ldar aK; StaCurrentContextSlot [S]`）——
    /// 序言被折叠掉之后，函数体里对槽 S 的读写要还原成参数名 `aK`
    /// （否则输出 `n` 这种源码名，而形参叫 a0 → 引用不到，count(2) 一次都不迭代）。
    gen_param_slots: HashMap<usize, String>,
    /// 6.2 的 `r = yield v`：恢复值在续体 shim 里经 `GeneratorGetInputOrDebugPos` 落到某个
    /// 寄存器 —— 挂起点下标 → 该寄存器名（表达式形态要写 `r = yield v;`）
    gen_yield_store: HashMap<usize, String>,
    /// 6.8 async 的"完成码 0"出口折叠：`Jump <分派>` 的下标 → 值寄存器名
    /// （该出口就地发 `return <值>;`，不再走尾部完成码分派）
    async_returns: HashMap<usize, String>,
    /// 参数默认值：形参下标 → 默认值表达式文本（渲染进**签名**，`a1 = 2`）。
    param_defaults: Vec<Option<String>>,
    /// 被重写的默认值 prologue 里那条 `Star rV` 的下标 → (形参下标, 目标寄存器)
    /// （发射时改成 `rV = aK;`，保持寄存器模型一致。寄存器必须在**计划期**取 ——
    /// 短形式 `Star1` 把寄存器编码在名字里，改名成 `__pregs` 后就取不到了）
    pregs_fixups: HashMap<usize, (usize, u32)>,
    /// plan_async 开始时记下的全部跳转（源下标, 目标下标）—— 改名后仍要能查（见 early_jumps 用法）
    early_jumps: Vec<(usize, usize)>,
    /// async 的 `_AsyncFunctionEnter`（6.2 是数字名 `CallJSRuntime`）下标 —— 改名后仍要能查
    async_enter: Option<usize>,
    /// async 序言的右端（Enter 之后那条 Star 的下标）：6.2 只能在它之前整段跳过，
    /// 之后就是函数体本体（见 six_x_generator）
    async_prologue_end: Option<usize>,
    /// 本函数是 async（函数头发 `async function`，挂起点发 `await`）
    is_async_fn: bool,
    /// catch 体里的"抛出对象"别名（当前上下文的 `MIN_CONTEXT_SLOTS` 槽 → 通常 `e`）；
    /// 只对 `Lda*CurrentContextSlot` 生效（见 `context_name`）
    catch_alias: Option<String>,
    /// catch 体里"显式上下文读取"要跳过的最内层作用域个数（读的是外层上下文 → 跳 1 层）
    explicit_scope_skip: usize,
    /// 本函数是 async generator（函数头发 `async function*`）
    is_async_gen: bool,
    /// 6.x async 的完成码分派区间（指令下标 [start, end)）：里面的"数字名"调用就是
    /// Resolve/Reject 的机器码（6.2 走 `CallJSRuntime [槽号]`，名字表解不出）
    async_dispatch: Option<(usize, usize)>,
    /// 已被 try/finally 规则消费的 handler 起点（旧 try/catch 规则不再重复包）
    used_handler_starts: Vec<usize>,
    /// emit_range 递归深度（护栏，防栈溢出）
    emit_depth: usize,
    /// 寄存器 → 它当前持有的属性访问表达式（`r2 = o.m` → `r2 => o.m`）。
    /// 用来把属性调用还原成 `o.m(args)`，而不是 `r2.call(o, args)`。
    reg_prop: HashMap<u32, Expr>,
    /// 已生成的语句
    out: String,
    indent: usize,
    loops: Vec<LoopCtx>,
    /// try/catch 的 handler 表
    handlers: Vec<Handler>,
    /// 需要声明的 context 名（按当前作用域顺序）
    ctx_names: Vec<String>,
    /// 块/catch 作用域栈（CreateCatchContext 等压入的 ScopeInfo）
    ctx_scopes: Vec<Option<ObjId>>,
    /// 寄存器 → 它持有的**上下文**对应的 ScopeInfo。`Create*Context` 产出的上下文经
    /// `Star rN` 落进寄存器后记在这里；`LdaContextSlot rN, [slot], [depth]` 这类显式读取
    /// 必须按**这个**上下文的作用域解析名字 —— 只看最内层作用域会把不同 block context 里
    /// **同号槽**的变量串名（`for (const ctor of …) { const rab = …; rab.X → ctor.X }`）。
    reg_scope: HashMap<u32, ObjId>,
    /// 刚创建的上下文（等待 Star/Mov 落进寄存器后登记到 reg_scope）
    pending_ctx: Option<ObjId>,
    label_counter: usize,
    tmp_counter: usize,
    /// 分支间物化的累加器变量（phi）
    phi_vars: Vec<String>,
    /// 指令内吞掉后续区间时（如 switch 的 case 体）主循环下次的起点
    skip_to: Option<usize>,
    /// 已被 guard 子句就地发射的"冷块"区间（起点下标, 终点下标 exclusive）→ 线性扫描时跳过
    skip_spans: Vec<(usize, usize)>,
    is_async: bool,
    is_generator: bool,
    /// 生成器重写（plan_generator）：挂起指令下标 → 被 yield 的值操作数文本（None = 用 acc）
    gen_yields: HashMap<usize, Option<String>>,
    /// 生成器重写：GetIterator 下标 → (被委托的迭代器操作数文本, 委托结果寄存器文本)
    gen_delegates: HashMap<usize, (String, String)>,
}

#[derive(Debug, Clone, Copy)]
struct Handler {
    start: u32,
    end: u32,
    target: u32,
}

/// 双精度元素数组（FixedDoubleArray）：元素是 **8 字节原始 f64**，不是 tagged 值。
/// 判定看 holder 的 map 名（`FixedDoubleArrayMap`）。
impl<'a> Decompiler<'a> {
    /// 版本比较（`table.v8` 形如 "6.2.414.78"）——少数操作码语义随 V8 版本变
    /// （如 `TestTypeOf` 的 flag 枚举在 6.7 插入 BigInt 后整体后移一位）。
    fn v8_at_least(&self, major: u32, minor: u32) -> bool {
        let mut it = self.table.v8.split('.').filter_map(|s| s.parse::<u32>().ok());
        let (Some(maj), Some(min)) = (it.next(), it.next()) else {
            return true; // 解不出来就按现代版本处理（覆盖的版本里绝大多数是现代）
        };
        (maj, min) >= (major, minor)
    }

    fn is_double_array(&self, o: ObjId) -> bool {
        self.cache.obj(o).ty.name(self.table).contains("DoubleArray")
    }

    /// FixedDoubleArray 第 i 个元素（8 字节 f64；压缩构建里一个 double 占 2 个 4 字节槽，
    /// 所以字节偏移恒为 `2*ts + i*8`，与 ts 无关）。
    fn double_elem(&self, o: ObjId, i: usize) -> Option<f64> {
        let ts = self.ts;
        let d = self.cache.raw_at_ts(o, 2 * ts + i * 8, 8, ts)?;
        Some(f64::from_le_bytes(d.try_into().ok()?))
    }
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
            lit_ctx: std::cell::Cell::new(false),
            emit_names: std::cell::RefCell::new(None),
            runtime: std::cell::Cell::new(false),

            wrapper_sfi: std::cell::Cell::new(None),
            // **全部**字节码都入表：acc 为空串是"完全不碰累加器"（V8 的
            // ImplicitRegisterUse::kNone），过滤掉会让 preserves_acc 落到手工名单，
            // 于是 acc 在 `LdaConstant "x"; CreateObjectLiteral …, r5; Star r4` 中间被当成
            // 死值 flush 掉（`"x" in {x:1}` 里 r4 变成 undefined）。
            acc_use: table
                .bytecodes
                .iter()
                .map(|b| {
                    (
                        b.name.clone(),
                        (b.acc.contains('r'), b.acc.contains('w')),
                    )
                })
                .collect(),
        }
    }

    /// 注入只读堆名表（提升属性名可读性）。反汇编器共用同一份：
    /// 13.x 起函数名/属性名常以 `ro{chunk}/{offset}` 形式出现，`sfi_name` 也要查表。
    pub fn with_ro_map(mut self, m: Option<RoMap>) -> Self {
        let rc = m.map(std::rc::Rc::new);
        self.ro_map = rc.clone();
        self.dis = self.dis.with_ro_map(rc);
        self
    }

    /// 是否带"可运行前导"（默认否：只给原始代码）。
    pub fn with_runtime(self, on: bool) -> Self {
        self.runtime.set(on);
        self
    }

    pub fn layout(&self) -> FamilyLayout {
        self.layout
    }

    /// 函数名 / function_data 槽位（供 CLI 的 ro-map 提取复用）。
    pub fn sfi_name(&self, sfi: ObjId) -> String {
        self.dis.sfi_name(sfi)
    }

    /// 脚本上下文的变量基准：12.4 起脚本上下文带 extension 槽
    /// （V8 `MIN_CONTEXT_EXTENDED_SLOTS = MIN_CONTEXT_SLOTS + 1`），变量从 `min+1` 起。
    /// 实测：9.4–11.3 的顶层 `let A` 在槽 2、12.4/13.6 在槽 3（`StaCurrentScriptContextSlot [3]`）。
    /// 这个作用域是不是脚本作用域（`ScopeType` 的数值随版本变）：
    /// - 8.4 起：`CLASS=0, EVAL=1, FUNCTION=2, MODULE=3, SCRIPT=4`
    /// - 13.x 起：`SCRIPT=0, REPL=1, CLASS=2, …, FUNCTION=4`
    ///   只有 12.4+ 的"脚本上下文 +1"用得到它；更早版本两个基准相同，判错也无害。
    fn is_script_scope(&self, flags: u64) -> bool {
        let major: u32 = self
            .table
            .v8
            .split('.')
            .next()
            .and_then(|x| x.parse().ok())
            .unwrap_or(0);
        let t = flags & 0xF;
        // ScopeType 顺序在 **12.9（node23）** 就换了（`scope-info.tq`：SCRIPT_SCOPE 排第一）；
        // 12.4（node22）还是老的（SCRIPT_SCOPE = 4）。判错时脚本作用域被当成函数作用域 →
        // 槽基准少 1 → 两个类变量 A/B 全读成 B（twoclasses 的 `r2 is not a constructor`）。
        let minor: u32 = self
            .table
            .v8
            .split('.')
            .nth(1)
            .and_then(|x| x.parse().ok())
            .unwrap_or(0);
        if major >= 13 || (major == 12 && minor >= 9) {
            t == 0
        } else {
            t == 4
        }
    }

    pub fn script_ctx_base(&self) -> usize {
        let min = self.table.min_context_slots;
        let mut it = self.table.v8.split('.');
        let major: u32 = it.next().and_then(|x| x.parse().ok()).unwrap_or(0);
        let minor: u32 = it.next().and_then(|x| x.parse().ok()).unwrap_or(0);
        // 12.4–13.6：脚本上下文多一个 extension 槽，变量从 min+1 起。
        // **14.x 起这个槽没了**（`LdaCurrentScriptContextSlot` 一族被
        // `LdaCurrentContextSlot` 取代，槽号整体 -1）——实测 twoclasses：
        // 13.6 是 `LdaCurrentScriptContextSlot [3]/[4]`，14.6 是
        // `LdaCurrentContextSlot [2]/[3]`，此时再 +1 会把两个槽都算成第一个变量
        // （输出里 A/B 都成了 A）。
        if (major == 12 && minor >= 4) || major == 13 {
            min + 1
        } else {
            min
        }
    }

    /// 该 SFI 在产物里用的名字（**全文件唯一**）。V8 的推断名会撞车 ——
    /// `const nest = (x) => (y) => x * y` 的内外两层箭头都叫 `nest`；同名声明会互相
    /// 覆盖、引用（DeclareGlobals、闭包常量）全都指到最后一个。这里按对象顺序去重：
    /// 先到的用本名，后来的加 `_2`/`_3` 后缀（闭包里对它的引用同步用这个名字）。
    pub fn function_emit_name(&self, sfi: ObjId) -> String {
        let mut slot = self.emit_names.borrow_mut();
        if slot.is_none() {
            let mut map: HashMap<ObjId, String> = HashMap::new();
            let mut used: HashSet<String> = HashSet::new();
            for id in 0..self.cache.objects.len() {
                if !self.cache.obj(id).ty.is(self.table, "SharedFunctionInfo") {
                    continue;
                }
                let raw = self.dis.sfi_name(id);
                let base = if raw.trim().is_empty() {
                    anon_name(id)
                } else {
                    sanitize_var(&raw)
                };
                let mut name = base.clone();
                let mut k = 2;
                while used.contains(&name) {
                    name = format!("{base}_{k}");
                    k += 1;
                }
                used.insert(name.clone());
                map.insert(id, name);
            }
            *slot = Some(map);
        }
        slot.as_ref()
            .and_then(|m| m.get(&sfi).cloned())
            .unwrap_or_else(|| anon_name(sfi))
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
  // DefineClass(boilerplate, ctor, parent, ...methods)：方法键只在 boilerplate 里
  // （形参看不到），而 10.2 起实例方法的 SFI 连推断名都没有 → 必须按 boilerplate 的
  // 键挂，不然 `c.bump is not a function`。boilerplate 已解码成
  // `{ n: 参数数, i: { 属性名: 下标 | {get,set} } }`（见 class_boilerplate）。
  DefineClass: function (bp, ctor, parent) {
    // 6.2 的签名是 `DefineClass(extends, ctor, startPosition, endPosition)` ——
    // **返回的是原型**（bytecode-generator 的 BuildClassLiteral：结果存进 prototype
    // 寄存器，实例方法随后按这个 receiver 挂）。返回 ctor 会让 `bump` 挂到构造函数
    // 自身上（`c.bump is not a function`）。判据：第 3/4 个实参都是数字（位置 Smi）。
    if (typeof parent === 'number' && typeof arguments[3] === 'number') {
      var parentCls = bp;
      var base = (parentCls && parentCls.prototype) || Object.prototype;
      var proto = Object.create(base);
      proto.constructor = ctor;
      try { ctor.prototype = proto; } catch (e) {}
      if (typeof parentCls === 'function') {
        try { Object.setPrototypeOf(ctor, parentCls); } catch (e) {}
      }
      return proto;
    }
    // 9.x+ 语义：本体就是传进来的那个构造函数（DefineClass 原地装配并返回它），
    // 调用点随后绑定的也是这个闭包 —— 所以这里必须原地改造，不能另造一个新函数。
    var Cls = ctor;
    if (parent) {
      Cls.prototype = Object.create(parent.prototype || Object.prototype);
      Object.setPrototypeOf(Cls, parent);
    }
    // 实参顺序：0=boilerplate 1=ctor 2=parent 3..=方法闭包
    var dyn = arguments;
    var defined = {};
    if (bp && bp.i) {
      // 按源码里的键挂（V8 的 SubstituteValues 也是这么做的：下标 → 闭包）
      for (var key in bp.i) {
        if (key === 'constructor') continue;
        var spec = bp.i[key];
        if (spec && typeof spec === 'object') {
          var d = {};
          if (spec.get != null && dyn[spec.get]) d.get = dyn[spec.get];
          if (spec.set != null && dyn[spec.set]) d.set = dyn[spec.set];
          if (d.get || d.set) {
            try { Object.defineProperty(Cls.prototype, key, d); defined[key] = 1; } catch (e) {}
          }
        } else if (spec >= 0 && dyn[spec]) {
          Cls.prototype[key] = dyn[spec];
          defined[key] = 1;
        }
      }
    }
    // 兜底：boilerplate 没给出映射的闭包（静态方法记在 static 模板里、值是 ClassPositions
    // 而不是下标；9.4 及更早的实例方法也没有键表）→ 按 SFI 名挂，
    // 静态/实例分不清就两边都挂
    for (var i = 3; i < dyn.length; i++) {
      var f = dyn[i];
      // 键用**原始 V8 名**：重名的函数在产物里被去重成 `bump` / `bump_2`
      // （见 Decompiler::function_emit_name），这类函数自带 `__v8name`
      var fname = f && f.__v8name ? f.__v8name : (f && f.name);
      if (typeof f !== 'function' || !fname || defined[fname]) continue;
      // getter/setter：V8 给这类 SFI 起名 `get value` / `set value`，摊平后成了
      // `get_value` / `set_value` → 按后缀定义成访问器，`obj.value` 才取得到
      var m = /^(get|set)_([A-Za-z_$][A-Za-z0-9_$]*)$/.exec(fname);
      if (m) {
        var d2 = Object.getOwnPropertyDescriptor(Cls.prototype, m[2]) || {};
        d2[m[1]] = f;
        try { Object.defineProperty(Cls.prototype, m[2], d2); } catch (e) {}
      } else {
        Cls.prototype[fname] = f;
      }
      Cls[fname] = f;
      defined[fname] = 1;
    }
    return Cls;
  },
  CreatePrivateNameSymbol: function (d) { return typeof Symbol === 'function' ? Symbol(d) : d; },
  // DefineAccessorPropertyUnchecked(obj, key, getter, setter)：对象字面量里的 get/set
  DefineAccessorPropertyUnchecked: function (obj, key, getter, setter) {
    var d = {};
    if (typeof getter === 'function') d.get = getter;
    if (typeof setter === 'function') d.set = setter;
    try { Object.defineProperty(obj, key, d); } catch (e) {}
    return obj;
  },
  // 对象剩余属性（`const {a, ...rest} = obj`）：排除已列举的键后收集其余自有可枚举属性
  // `const {a, ...rest} = obj`：被排除的键由 V8 放在寄存器里当参数传
  // （OnStack 变体的 excluded_count/栈基址由解释器补，对 JS 层等价于"其余参数都是键"）
  CopyDataPropertiesWithExcludedProperties: function (src) {
    if (src == null) throw new TypeError('Cannot convert undefined or null to object');
    var out = {};
    var excl = Array.prototype.slice.call(arguments, 1);
    var o = Object(src);
    Object.keys(o).forEach(function (k) {
      if (excl.indexOf(k) < 0) out[k] = o[k];
    });
    return out;
  },
  CopyDataPropertiesWithExcludedPropertiesOnStack: function (src) {
    return __runtime.CopyDataPropertiesWithExcludedProperties.apply(null, arguments);
  },
  // 对象展开 / rest：`{...src}` 与 `{...t, ...src}` 都走它（目标在前、源在后；
  // 10.x 起还有 excluded 变体）。缺了它 Proxy 会给个空函数 → 源属性整批丢失
  // （spread_rest 的 `merged` 只剩自己新加的键）。
  CopyDataProperties: function (target, src) {
    if (src == null) return target;
    var o = Object(src);
    Object.keys(o).forEach(function (k) { target[k] = o[k]; });
    return target;
  },
  // InvokeIntrinsic 里最常用的几个（迭代协议 / 调用桥）。缺了它们 __intrinsic 的
  // Proxy 会给个返回 undefined 的空函数 —— `!IsJSReceiver(r)` 于是恒真，
  // 迭代器结果一律被判成非法（destructure/for_of_in/generator 全挂在这）。
  IsJSReceiver: function (v) {
    return (typeof v === 'object' && v !== null) || typeof v === 'function';
  },
  // 直接 eval 的解析：V8 用它决定"调用原函数还是间接 eval"。
  // 我们的产物里顺序就是 `r2 = Resolve(...); r2(code)` → 返回原被调者即可。
  ResolvePossiblyDirectEval: function (fn) { return fn; },
  // 宽松模式的 `delete <动态名>`：V8 发的是 CallRuntime（名字当字符串传进来）
  DeleteLookupSlot: function (name) {
    try { return delete globalThis[name]; } catch (e) { return false; }
  },
  Call: function (fn, receiver) {
    var a = Array.prototype.slice.call(arguments, 2);
    return fn.apply(receiver, a);
  },
  ToString: function (v) { return String(v); },
  CreateIterResultObject: function (value, done) { return { value: value, done: done }; },
  // 类装配（6.2 逐条装方法）、以及一批"原样返回/空操作"的运行时。
  // 缺了它们 Proxy 会给出**返回 undefined** 的空函数：`Counter = ToFastProperties(r3)`
  // 于是把类绑定写成 undefined（`new Counter()` → "r2 is not a constructor"）。
  ToFastProperties: function (o) { return o; },
  InstallClassNameAccessor: function (o) { return o; },
  DefineGetterPropertyUnchecked: function (obj, key, getter, setter) {
    var d = {};
    if (typeof getter === 'function') d.get = getter;
    if (typeof setter === 'function') d.set = setter;
    try { Object.defineProperty(obj, key, d); } catch (e) {}
    return obj;
  },
  DefineOwnPropertyIgnoreAttributes: function (obj, key, value) {
    try { Object.defineProperty(obj, key, { value: value, writable: true, enumerable: true, configurable: true }); } catch (e) {}
    return value;
  },
  NewScriptContext: function () { return {}; },
  NewTypeError: function (kind, name) { return new TypeError(String(name)); },
  ReThrow: function (v) { throw v; },
  StackCheck: function () {},
  SetCode: function (o) { return o; },
  RestoreGeneratorState: function (gen) { return gen; },
  TestEqualStrictNoFeedback: function (a, b) { return a === b; },
  ThrowThrowMethodMissing: function () { throw new TypeError('iterator has no throw method'); },
  Abort: function () {},
  // 6.2 的数组字面量展开（`[0, ...rest, a]`）用 Runtime_AppendElement(array, value)
  // 逐个追加（9.x 起改成 StaInArrayLiteral）。缺了它元素整批丢失。
  AppendElement: function (arr, v) { arr[arr.length] = v; return v; },
  // 生成器版本 6.x 直接用 `%GeneratorGetInputOrDebugPos` 一类 intrinsic 读内部字段；
  // 这些字段码缓存里没有对应实现，只能给保守值（详见 docs/VERSIONS.md §10）。
  GeneratorGetResumeMode: function (gen) {
    return gen && typeof gen === 'object' && '__resumeMode' in gen ? gen.__resumeMode : 0;
  },
  GeneratorGetInputOrDebugPos: function (gen) {
    return gen && typeof gen === 'object' && '__input' in gen ? gen.__input : undefined;
  },
  GeneratorGetContext: function (gen) {
    return gen && typeof gen === 'object' ? gen.__context || __ctx : __ctx;
  },
  ThrowSymbolIteratorInvalid: function () { throw new TypeError('Invalid iterator'); },
  ThrowIteratorResultNotAnObject: function (v) { throw new TypeError('bad iterator result'); },
  // `using`/`await using`：V8 用 DisposableStack 协议（GetDisposeMethod → Call → …）。
  // 码缓存里只有调用点；这里给最小实现：AddDisposableValue 记下 `[方法, 接收者]`，
  // DisposeDisposableStack 倒序调用（同步/异步各一套）。
  InitializeDisposableStack: function () { return { stack: [], async: false }; },
  InitializeAsyncDisposableStack: function () { return { stack: [], async: true }; },
  GetDisposeMethod: function (v, hint) { return v != null ? v[Symbol.dispose] : undefined; },
  GetAsyncDisposeMethod: function (v) { return v != null ? v[Symbol.asyncDispose] : undefined; },
  AddDisposableValue: function (s, v) {
    var m = v != null ? (v[Symbol.dispose] || v[Symbol.asyncDispose]) : undefined;
    if (m === undefined) throw new TypeError('not disposable');
    if (typeof m !== 'function') throw new TypeError('dispose is not a function');
    s.stack.push([m, v]);
    return s;
  },
  DisposeDisposableStack: function (s) {
    if (!s || !s.stack) return;
    for (var i = s.stack.length - 1; i >= 0; i--) { s.stack[i][0].call(s.stack[i][1]); }
    s.stack.length = 0;
  },
}, { get: function (t, k) { return k in t ? t[k] : function () {}; } });
// V8 的 intrinsic（`InvokeIntrinsic [_X]`）是 C++ 内建：多数无实现可用，但少数
// （CopyDataPropertiesWithExcludedPropertiesOnStack 这类）在 __runtime 里有等价实现
// —— 先查 __runtime，查不到才退化成空实现。
var __intrinsic = new Proxy(__runtime, {
  get: (t, k) => (k in t ? t[k] : function () { return undefined; }),
});
var __context, __ctx = {};
// V8 把"非直接 eval"的调用点记成 `eval_`（源码里的 `eval(...)` 在某些作用域下被编译成
// 间接 eval）→ 不声明它产物会抛 ReferenceError（`eval("/\\rn/;")` 本该抛 SyntaxError 的
// 测试因此永远看不到真正的语法错误）。间接 eval 的语义就是"在全局作用域求值"。
var eval_ = function (code) { return (0, eval)(code); };
// `using` / `await using` 的资源管理 intrinsic（DisposableStack 系列）在码缓存里只有调用点：
// 给最小实现，让 `using` 至少真的调用 dispose（否则静默不释放）。
// for-in 的键枚举协议尚未重建 → 用到就抛清晰错误（不再 ReferenceError / 死循环）
function __forin_unsupported() { throw new Error('jscd: for-in 枚举协议尚未重建'); }
function __anonymous() {}
var __uncompiled = new Proxy({}, { get: () => function () {} });

"#;

    pub fn render_all<W: FmtWrite>(&self, w: &mut W, filter: Option<&str>) -> Result<(), String> {
        // 前言分两层：
        //   ① 可运行前导（`__runtime`/`__intrinsic` 占位实现）—— 只在 `--runtime` 时输出，
        //      默认剥掉：多数人是**读**反编译结果，200 行运行时噪声会把原始代码淹没。
        //   ② 各作用域 context 变量提升为文件级 `var` —— 摊平后的代码依赖它才说得通，
        //      与"运行时噪声"无关，两种模式都留（很短，且只在真有闭包变量时出现）。
        let with_runtime = self.runtime.get();
        if with_runtime {
            let _ = w.write_str(Self::STUBS);
        } else {
            // 默认连横幅都不发：输出就是代码本身。（要可运行前导/说明用 `--runtime`。）
        }
        let preamble = self.shared_bindings();
        if !preamble.is_empty() {
            let _ = w.write_str(&preamble);
        }
        // 重名去重过的函数带一份**原始 V8 名**（`tag` 与第二个类的 `tag_2` 都该挂成 `tag`，
        // 见 STUBS 的 DefineClass 兜底）。必须放在**顶层代码之前**：类装配在加载时就跑，
        // 而这些名字是函数声明（有 hoisting），赋值本身没有。
        let mut alias = String::new();
        for id in 0..self.cache.objects.len() {
            if !self.cache.obj(id).ty.is(self.table, "SharedFunctionInfo") {
                continue;
            }
            let raw = self.dis.sfi_name(id);
            if raw.trim().is_empty() {
                continue;
            }
            let emit = self.function_emit_name(id);
            if sanitize_var(&raw) != emit {
                // `typeof` 守卫：类构造器在产物里是 `var X = class {...}`（**不提升**），
                // 这个别名块在顶层代码之前 → 直接赋值会 `undefined.__v8name` TypeError。
                // 带守卫时对 `var` 形式静默跳过（构造器不需要别名：兜底挂键只处理 dyn[3..] 的方法）。
                alias.push_str(&format!(
                    "if (typeof {emit} === \"function\") {emit}.__v8name = {};\n",
                    js_string(&raw)
                ));
            }
        }
        if with_runtime && !alias.is_empty() {
            let _ = w.write_str(&alias);
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
            // 用 sanitize_var：保留字（"function" 之类）直接输出会得到 `function function()`
            let fname = self.function_emit_name(sfi);
            return Ok(format!(
                "function {fname}() {{ /* 未编译（UncompiledData）：源码不在 code cache 中 */ }}\n\n"
            ));
        };
        let scope_id = self.scope_id_of(sfi);
        let scope = scope_id.and_then(|id| self.scope_by_id(id));
        if std::env::var("JSCD_DBG_SCOPE").is_ok() {
            eprintln!(
                "[scope] sfi={sfi} name={:?} locals={} table={:?} outer={:?} params={} flags={:#x}",
                self.dis.sfi_name(sfi),
                scope.as_ref().map(|s| s.context_locals.len()).unwrap_or(0),
                scope.as_ref().and_then(|s| s.locals_table),
                scope.as_ref().and_then(|s| s.outer),
                scope.as_ref().map(|s| s.param_count).unwrap_or(0),
                scope.as_ref().map(|s| s.flags).unwrap_or(0),
            );
        }
        let mut ctx = FnCtx::new(self, sfi, bca, scope, scope_id)?;
        if ctx.wrapper_factory {
            // 闭包工厂只是"把包装函数交出去"，没有可执行的顶层语句
            return Ok(String::new());
        }
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
        self.context_name_in_chain_base(scope_id, slot, self.table.min_context_slots)
    }

    /// 带槽基准的版本（脚本上下文的变量从 `min_context_slots + 1` 起，见 FnCtx::context_name_base）
    pub fn context_name_in_chain_base(
        &self,
        scope_id: Option<ObjId>,
        slot: usize,
        base: usize,
    ) -> Option<String> {
        let mut cur = scope_id;
        for _ in 0..8 {
            let id = cur?;
            let scope = self.scope_by_id(id)?;
            // 槽属于哪个作用域就用哪个基准：脚本作用域的上下文多一个 extension 槽
            let eff = if self.is_script_scope(scope.flags) { self.script_ctx_base() } else { base };
            // `slot < eff`（典型：catch context 的槽 0 = 抛出对象）**不能**索引 ——
            // `saturating_sub` 会落到 0，读成作用域第一个变量名（22.12/24.12 的
            // for-of 收尾守卫因此写成 `if (returnCalls !== undefined) throw returnCalls`，
            // 循环体一抛错就抛出一个数字）。
            if slot < eff {
                cur = scope.outer;
                continue;
            }
            let idx = slot - eff;
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
        let s = read_scope(self.cache, self.table, self.ts, scope_id, self.ro_map.as_deref())?;
        self.scope_cache.borrow_mut().insert(scope_id, s.clone());
        Some(s)
    }

    /// SFI → ScopeInfo 对象 id。
    ///
    /// 槽位随版本变：6.2 同时有 `name_or_scope_info`（名字）与 `scope_info`（ScopeInfo），
    /// 只看前者会读到名字字符串、ScopeInfo 整个丢失（context 局部名全成 `__ctx.ctxN`）；
    /// 6.8 起两者合并成一个槽。两个槽都试，谁真的装着 ScopeInfo 就用谁。
    fn scope_id_of(&self, sfi: ObjId) -> Option<ObjId> {
        let mut slots: Vec<usize> = Vec::new();
        if let Some(l) = self.table.shared_function_info.as_ref() {
            for f in ["scope_info", "name_or_scope_info"] {
                if let Some(v) = l.slot(self.ts, f) {
                    slots.push(v);
                }
            }
        }
        if slots.is_empty() {
            slots.push(2);
        }
        for slot in slots {
            if let Some(Ref::Object(o)) = self.cache.slot_at(sfi, slot).and_then(|v| v.as_ref()) {
                if self.cache.obj(o).ty.is(self.table, "ScopeInfo") {
                    return Some(o);
                }
            }
        }
        None
    }
}

/// V8 内部符号根名 → JS 众所周知的符号（ECMAScript 里这批名字固定）。
///
/// 7.x 的 roots 表存的是 `iterator_symbol` 这类**内部拼写**（8.x+ 才给 `Symbol.iterator`）——
/// 直接按标识符输出会变成 `undefined`（`obj[undefined]()` → "not a function"），
/// destructure / spread_rest / for_of_in / generator 全挂在这。
fn well_known_symbol(internal: &str) -> Option<&'static str> {
    Some(match internal {
        "async_iterator_symbol" => "asyncIterator",
        "has_instance_symbol" => "hasInstance",
        "is_concat_spreadable_symbol" => "isConcatSpreadable",
        "iterator_symbol" | ".iterator" => "iterator",
        "match_all_symbol" => "matchAll",
        "match_symbol" => "match",
        "replace_symbol" => "replace",
        "search_symbol" => "search",
        "species_symbol" => "species",
        "split_symbol" => "split",
        "to_primitive_symbol" => "toPrimitive",
        "to_string_tag_symbol" => "toStringTag",
        "unscopables_symbol" => "unscopables",
        _ => return None,
    })
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
        if std::env::var("JSCD_DBG_PARAM").is_ok() {
            eprintln!(
                "[param] sfi={sfi} bca={bca} parameter_size={parameter_size} direct={} -> param_count={param_count} name={:?}",
                d.dis.parameter_count_direct(),
                d.sfi_name(sfi)
            );
        }
        let decoder = Decoder::new(d.table, d.layout, param_count as i32);
        let mut instrs = decoder.decode(code).map_err(|e| format!("decode bytecode: {e}"))?;
        for ins in &mut instrs {
            let base = ins.name.split('.').next().unwrap_or(&ins.name).to_string();
            let canon = canonical_name(&base);
            if canon != base {
                ins.name = match ins.name.split_once('.') {
                    Some((_, suffix)) => format!("{canon}.{suffix}"),
                    None => canon.to_string(),
                };
            }
        }
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
        let code_len = instrs.last().map(|i| i.offset).unwrap_or(0);
        let handlers = read_handler_table(d, bca, code_len, &idx_of);
        if std::env::var("JSCD_DBG_POOL").is_ok() {
            match pool {
                Some(pid) => {
                    let ty = d.cache.obj(pid).ty.name(d.table);
                    let len = d.cache.array_len(pid);
                    let mut elems = Vec::new();
                    for k in 0..len.min(6) {
                        let e = match d.cache.array_elem(pid, k) {
                            Some(Elem::Ref(Ref::Object(o))) => {
                                let t2 = d.cache.obj(o).ty;
                                if t2.is_string(d.table) {
                                    format!("str({:?})", d.dis.string_value(o))
                                } else {
                                    format!("obj{o}:{}", t2.name(d.table))
                                }
                            }
                            Some(Elem::Ref(Ref::RoRef(c, off))) => format!("ro{c}/{off}"),
                            Some(Elem::Ref(Ref::Root(r))) => format!("root{r}"),
                            Some(Elem::Smi(v)) => format!("smi{v}"),
                            other => format!("{other:?}"),
                        };
                        elems.push(e);
                    }
                    eprintln!(
                        "[pool] bca={bca} slot={} pool={pid} ty={ty} len={len} {:?}",
                        d.dis.bca_constant_pool_slot(),
                        elems
                    );
                }
                None => eprintln!("[pool] bca={bca} 池未解析"),
            }
        }

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

        // bytenode 的 .jsc：CommonJS 源码被 `Module.wrap` 包成
        //   (function (exports, require, module, __filename, __dirname) { <源码> });
        // → 脚本体只剩"取闭包、返回"，即所谓闭包工厂。工厂本身不输出，包装函数按
        // 顶层摊平（见下面的 inline_body 判定），参数名按 CJS 约定给回去。
        let mut wrapper_factory = false;
        {
            let is_top = sfi == d.cache.top_sfi;
            let mut child: Option<ObjId> = None;
            let mut ok = is_top && !instrs.is_empty();
            if ok {
                for ins in &instrs {
                    let b = ins.name.split('.').next().unwrap_or(&ins.name);
                    match b {
                        // 取闭包：常量池里的 SFI 就是包装函数
                        "LdaConstant" | "LdaConstantWide" | "CreateClosure" => {
                            if let Some(Operand::Idx(i)) = ins.operands.first() {
                                child = pool
                                    .and_then(|p| d.cache.array_elem(p, *i as usize))
                                    .and_then(|e| e.as_ref())
                                    .and_then(|r| match r {
                                        Ref::Object(o)
                                            if d.cache.obj(o).ty.is(d.table, "SharedFunctionInfo") =>
                                        {
                                            Some(o)
                                        }
                                        _ => None,
                                    });
                            }
                        }
                        // 工厂壳里允许出现的"机器码"：栈检查、寄存器搬运、返回
                        // （`Star0`…`Star15` 这类短名也在内）
                        b if b.starts_with("Star") => {}
                        "Return" | "Ldar" | "Mov" | "LdaTheHole" | "SetPendingMessage"
                        | "StackCheck" | "LdaUndefined" => {}
                        _ => {
                            ok = false;
                            break;
                        }
                    }
                }
            }
            if std::env::var("JSCD_DBG_WRAP").is_ok() {
                eprintln!(
                    "[wrap] sfi={sfi} top={} instrs={} ok={} child={child:?}",
                    d.cache.top_sfi,
                    instrs.len(),
                    ok
                );
            }
            if ok {
                if let Some(c) = child {
                    d.wrapper_sfi.set(Some(c));
                    wrapper_factory = true;
                }
            }
        }

        // 脚本/模块顶层：V8 只在脚本顶层发 `DeclareGlobals`。
        // 这类函数的语句必须铺在文件里执行，否则模块级代码（类定义、require、初始化）永不运行。
        //
        // **载荷根 SFI 一定就是脚本顶层**，哪怕它一条声明都没有（`console.log("hi");`
        // 这种"只有语句"的脚本没有 DeclareGlobals）—— 漏掉这一条会把它当匿名函数输出成
        // 没人调用的 `function _anon_0()`，整个程序静默不执行。
        let inline_body = sfi == d.cache.top_sfi
            || d.wrapper_sfi.get() == Some(sfi)
            || instrs.iter().any(|i| {
            let b = i.name.split('.').next().unwrap_or(&i.name);
            if b != "CallRuntime" && b != "CallJSRuntime" {
                return false;
            }
            // `CallRuntime [DeclareGlobals], …`：id 在操作数里，按名字表判定。
            // 6.x 的解释器发的是 `DeclareGlobalsForInterpreter`（同一件事），
            // 只认前者会让 node8/10 的顶层代码永不执行。
            matches!(i.operands.first(), Some(Operand::RuntimeId(v))
                if d.table.runtime_names.get(*v as usize)
                    .map(|n| n == "DeclareGlobals" || n == "DeclareGlobalsForInterpreter")
                    .unwrap_or(false))
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
            prev_base: String::new(),
            cur_base: String::new(),
            acc: None,
            acc_stored: false,
            acc_stored_reg: None,
            last_line: String::new(),
            body: String::new(),
            inline_body,
            wrapper_factory,
            last_ctx_slot: None,
            slot_aliases,
            gen_param_slots: HashMap::new(),
            gen_yield_store: HashMap::new(),
            async_returns: HashMap::new(),
            param_defaults: Vec::new(),
            pregs_fixups: HashMap::new(),
            early_jumps: Vec::new(),
            async_enter: None,
            async_prologue_end: None,
            is_async_fn: false,
            catch_alias: None,
            explicit_scope_skip: 0,
            is_async_gen: false,
            async_dispatch: None,
            used_handler_starts: Vec::new(),
            emit_depth: 0,
            reg_prop: HashMap::new(),
            out: String::new(),
            indent: 0,
            loops: Vec::new(),
            handlers,
            ctx_names: Vec::new(),
            ctx_scopes: Vec::new(),
            reg_scope: HashMap::new(),
            pending_ctx: None,
            label_counter: 0,
            tmp_counter: 0,
            phi_vars: Vec::new(),
            skip_to: None,
            skip_spans: Vec::new(),
            is_async: false,
            is_generator: false,
            gen_yields: HashMap::new(),
            gen_delegates: HashMap::new(),
        })
    }

    fn render_operand(&self, op: &Operand) -> String {
        // 形参个数必须传真的：≤8.4 的寄存器命名是 `index - param_base + parameter_count`，
        // 传 0 会让形参渲染成 `a-2`/`a-3`（node14 的产物就是这么错的）。
        let dec = Decoder::new(self.d.table, self.d.layout, self.param_count() as i32);
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

    /// 主流程：函数签名 + 语句体。
    fn run(&mut self) -> Result<(), String> {
        // 脚本/模块顶层：不包 function 外壳，直接出语句（原样铺在文件里才会执行）
        if self.inline_body {
            self.declare_locals();
            let decl_at = self.out.len();
            self.emit_range(0, self.instrs.len())?;
            if !self.phi_vars.is_empty() {
                let decl = format!("let {};\n", self.phi_vars.join(", "));
                self.out.insert_str(decl_at, &decl);
            }
            self.body = std::mem::take(&mut self.out);
            return Ok(());
        }
        // 函数种类：按出现的 opcode 判定（比枚举稳）
        // 只看**操作码 + 形态**：ScopeInfo 的 function_kind 位在版本间不一致
        // （曾经把箭头函数误判成类构造器；这次把它当"是不是生成器"兜底又让箭头函数渲染成
        // `function*` → 产物返回 `[object Generator]`、arithmetic 全错）。
        //   async：`Await` 操作码，或 6.x 的机器形态（`looks_async_6x`：具名结算调用 /
        //          `CreateJSGeneratorObject` 后的数字名调用 / 尾部 Switch 的数字名结算）
        //   生成器：`SuspendGenerator`（6.2 起的生成器都有）
        self.is_async = self.instrs.iter().any(|i| i.name.starts_with("Await")) || self.looks_async_6x();
        self.is_generator = self.instrs.iter().any(|i| i.name.starts_with("SuspendGenerator"));
        // 参数默认值：prologue 里的"有默认值就走默认"整段折进**签名**
        // （必须在 plan_async/plan_generator 之前 —— 那两个会改写 prologue/状态机）
        self.plan_param_slots();
        self.plan_param_defaults();
        // async：与生成器同构的状态机 → 折回 `async function` + `await`
        self.plan_async();
        // 生成器：把状态机外壳折回成 `yield` / `yield*`（须在判定之后、发射之前）
        self.plan_generator();

        let params = self.signature_param_count();
        let with_default = |i: usize, n: String| -> String {
            match self.param_defaults.get(i).and_then(|d| d.clone()) {
                Some(d) => format!("{n} = {d}"),
                None => n,
            }
        };
        let names: Vec<String> = if params > 64 {
            vec![format!("/* {params} 个参数（异常值，忽略） */")]
        } else if self.inline_body && self.d.wrapper_sfi.get() == Some(self.sfi) && params >= 4 {
            // bytenode 的 CommonJS 包装函数：名字固定是这五个（Node 的 Module.wrap 同款），
            // 叫回真名，摊平后的 `require(...)`/`module.exports` 才既好读又能直接跑
            const CJS: [&str; 5] = ["exports", "require", "module", "__filename", "__dirname"];
            (0..params)
                .map(|i| {
                    let n = CJS.get(i as usize).map(|s| (*s).to_string()).unwrap_or_else(|| format!("a{i}"));
                    with_default(i as usize, n)
                })
                .collect()
        } else {
            (0..params).map(|i| with_default(i as usize, format!("a{i}"))).collect()
        };

        if self.d.runtime.get() {
            self.out.push_str("// @generated by jscd — 源码文本不在 code cache 中，以下是按字节码重建的伪 JS\n");
        }
        // 需要 class 体的只有"真的用到 super"的构造器（派生类），判据用字节码证据：
        // ScopeInfo 的 function_kind 位在版本间不一致（曾把箭头函数 kind=11 误判成类构造器，
        // 于是 `const __class_add_7 = class{...}` 既不是箭头也无法当普通函数调用）。
        // 普通（基类）构造器用 `function X(){...}` 完全合法：`new X()` 一样工作。
        let is_class_ctor = self.instrs.iter().any(|i| {
            let b = i.name.split('.').next().unwrap_or(&i.name);
            b == "ThrowSuperNotCalledIfHole" || b == "GetSuperConstructor"
        });
        if is_class_ctor {
            let fname = self.flat_name();
            // 名字要与调用点一致（V8 里 `Counter.zero()` / `new Counter()` 都按这个名字引用）
            self.out.push_str(&format!("var {fname} = class {{\n"));
        }
        let header = if is_class_ctor {
            format!("constructor({})", names.join(", "))
        } else {
            let fname = self.flat_name();
            // async 与生成器正交：`async function` / `async function*` / `function*`
            let star = if self.is_async_fn {
                if self.is_async_gen { "*" } else { "" }
            } else if self.is_generator {
                "*"
            } else {
                ""
            };
            let prefix = if self.is_async_fn || self.is_async { "async " } else { "" };
            format!("{prefix}function{star} {fname}({})", names.join(", "))
        };
        self.out.push_str(&header);
        self.out.push_str(" {\n");
        self.indent = 1;

        // 声明寄存器与 context 局部名（保持语法合法、便于阅读）
        self.declare_locals();
        let decl_at = self.out.len();
        self.emit_range(0, self.instrs.len())?;
        // phi 统一回填到函数头（发射过程中按需产生）。**必须带缩进** ——
        // 行首无缩进的 `let phi…;` 会被 `regions()` 当成"新的文件级声明"，
        // 于是这个函数的后半段被划进另一个区域（区域化的声明裁剪曾据此把
        // `let r0…r15;` 整个删掉 → ReferenceError: r13 is not defined）。
        if !self.phi_vars.is_empty() {
            let decl = format!("  let {};\n", self.phi_vars.join(", "));
            self.out.insert_str(decl_at, &decl);
        }

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

    /// 本函数在摊平产物里的名字：走全文件唯一的 `function_emit_name`
    /// （ScopeInfo 的推断名会撞车，见 `Decompiler::function_emit_name`）。
    fn flat_name(&self) -> String {
        self.d.function_emit_name(self.sfi)
    }

    fn param_count(&self) -> u32 {
        // 口径不同：BCA 的 `parameter_size/8` 是**寄存器文件**口径（含 this），
        // 而 ≤8.4 的 `ScopeInfo::ParameterCount()` 只数形参（不含 this）——
        // 老族若采信 ScopeInfo，签名会少一个参数、寄存器名会算成 `a-2`（node14 产物
        // 直接语法错误）。9.x+ 两口径一致，继续用 ScopeInfo（长期验证过的路径）。
        let legacy = self.d.table.v8.starts_with('8')
            || self.d.table.v8.starts_with('7')
            || self.d.table.v8.starts_with('6');
        if !legacy {
            if let Some(s) = &self.scope {
                if s.param_count > 0 && s.param_count <= 64 {
                    return s.param_count;
                }
            }
            // ScopeInfo 读不出形参时（0 可能是"真 0 个"也可能是缺省）：BCA 的口径**含 `this`**
            // —— 直接采信会把 0 形参函数渲染成 `f(a0)`（closure 的 `inc`、class_basic 的
            // `static zero` 都栽过）。非老族一律 −1。
            return self.bca_param_count().saturating_sub(1);
        }
        self.bca_param_count()
    }

    /// **签名**用的形参个数（`.length` 的口径）：
    /// ≤8.4 老族的 BCA 口径含 `this`（+1），而 ScopeInfo 的 `ParameterCount()` 只数形参。
    /// 两者**交叉验证一致**（`scope.param_count + 1 == bca_param_count()`）时才敢少写一个 —
    /// 直接无脑 −1 曾让老族寄存器名算成 `a-2`（node14 = V8 8.4 产物直接语法错），
    /// 所以只在能证明"那个多出来的就是 this"时替换（`8.17` 的 `function bump(a0,a1)` → `bump(a0)`）。
    fn signature_param_count(&self) -> u32 {
        let bc = self.bca_param_count();
        let legacy = self.d.table.v8.starts_with('8')
            || self.d.table.v8.starts_with('7')
            || self.d.table.v8.starts_with('6');
        if legacy {
            if let Some(s) = &self.scope {
                // 不排除 0：0 参数函数的 BCA 恰为 1（只有 this），`0+1==1` 同样成立且正确；
                // ScopeInfo 读不出来时也是 0，此时只有 BCA==1 才会命中 —— 那也确实是 0 个形参。
                if s.param_count + 1 == bc {
                    return s.param_count;
                }
            }
        }
        self.param_count()
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
        if std::env::var("JSCD_DBG_POOL").is_ok() {
            match self.d.cache.array_elem(pool, idx) {
                Some(Elem::Ref(r)) => {
                    let extra = match &r {
                        Ref::Root(k) => format!(" rootidx={k}"),
                        Ref::Object(o) => format!(
                            " obj={o} ty={} name={:?}",
                            self.d.cache.obj(*o).ty.name(self.d.table),
                            self.d.dis.describe_ref(&r)
                        ),
                        other => format!(" {other:?}"),
                    };
                    eprintln!("[pool] {idx} -> {}", extra.trim_start());
                }
                Some(Elem::Smi(v)) => eprintln!("[pool] {idx} -> smi {v}"),
                Some(_) => eprintln!("[pool] {idx} -> bytes"),
                None => eprintln!("[pool] {idx} -> None"),
            }
        }
        match self.d.cache.array_elem(pool, idx) {
            Some(Elem::Smi(v)) => Expr::Num(v as f64),
            Some(Elem::Ref(Ref::Object(o))) => self.object_constant(o),
            Some(Elem::Ref(Ref::RoRef(c, o))) => match self.d.ro_map.as_deref().and_then(|m| m.get(c, o)) {
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
        let raw = self.d.table.root_name(i).unwrap_or("");
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
            // `{ ...x }` 的基底与"只有计算键的对象字面量"（`{ [k]: v }`）：
            // 常量池条目就是空对象根（9.x 起改发 CreateEmptyObjectLiteral）。
            // 给 undefined 的话 `r0[k] = v` 直接 TypeError（`{ ...x }` 的
            // CopyDataProperties 目标同样会挂）。
            "EmptyBoilerplateDescription" | "EmptyObjectBoilerplateDescription" => {
                Expr::ObjectLit(Vec::new())
            }
            // 空数组字面量（`const out = []`）在某些版本走这个根 —— 给 undefined 会
            // `out.push(...)` 直接 TypeError（node12 的探针就是这么崩的）。
            "EmptyArrayBoilerplateDescription" | "EmptyFixedArrayLiteral" => {
                Expr::ArrayLit(Vec::new())
            }
            _ if raw.starts_with("String:") => Expr::Str(n.to_string()),
            // 众所周知的符号：表里存的是 heap-symbols.h 给的 JS 名（`Symbol.iterator` 等），
            // 直接用标识符形式（`obj[Symbol.iterator]()`）。私有符号没有 JS 名 → 占位。
            _ if raw.starts_with("Symbol:") && n.starts_with("Symbol.") => Expr::Ident(n.to_string()),
            // 7.x 的内部拼写（`Symbol:iterator_symbol`）折成 `Symbol.iterator`
            _ if raw.starts_with("Symbol:") && well_known_symbol(n).is_some() => {
                Expr::Ident(format!("Symbol.{}", well_known_symbol(n).unwrap()))
            }
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
        if std::env::var("JSCD_DBG_OBJ").is_ok() {
            let n = self.d.cache.array_len(o);
            let mut slots = Vec::new();
            for i in 0..n.min(8) {
                slots.push(match self.d.cache.array_elem(o, i) {
                    Some(Elem::Ref(Ref::Root(r))) => format!("{i}:root{r}"),
                    Some(Elem::Ref(Ref::Object(x))) => {
                        format!("{i}:obj{x}:{}", self.d.cache.obj(x).ty.name(self.d.table))
                    }
                    Some(Elem::Ref(Ref::RoRef(c, off))) => format!("{i}:ro{c}/{off}"),
                    Some(Elem::Smi(v)) => format!("{i}:smi{v}"),
                    other => format!("{i}:{other:?}"),
                });
            }
            eprintln!(
                "[obj] o={o} ty={:?} name={} len={n} slots={slots:?}",
                ty,
                ty.name(self.d.table)
            );
        }
        if ty.is_string(self.d.table) {
            return Expr::Str(self.d.dis.string_value(o).unwrap_or_default());
        }
        if ty.is(self.d.table, "HeapNumber") {
            if let Some(d) = self.d.cache.raw_at_ts(o, self.d.ts, 8, self.d.ts) {
                return Expr::Num(f64::from_le_bytes(d.try_into().unwrap()));
            }
        }
        // BigInt 的 map 名是 "BigIntMap"（`ty.is(table,"BigInt")` 永远匹配不上）——
        // 之前落进未知常量分支，`[10n]` 变成 `/* BigIntMap(46) */ undefined`。
        if ty.is(self.d.table, "BigInt") || ty.name(self.d.table).contains("BigInt") {
            if let Some(v) = self.d.dis.bigint_value(o) {
                return Expr::BigInt(v);
            }
        }
        if ty.is(self.d.table, "SharedFunctionInfo") {
            let n = self.d.dis.sfi_name(o);
            // 必须用 sanitize_var（保留字如 V8 偶尔给 SFI 起的 "function"）+ 去重：
            // 重名会让引用指到错的那个函数上（见 function_emit_name）
            let key = self.d.function_emit_name(o);
            return Expr::Ident(format!("/* function {n} */ __uncompiled.{key}"));
        }
        if ty.is(self.d.table, "ObjectBoilerplateDescription") {
            return self.object_boilerplate(o);
        }
        if ty.is(self.d.table, "ArrayBoilerplateDescription") {
            return self.array_boilerplate(o);
        }
        if let Some(c) = self.class_boilerplate(o) {
            return c;
        }
        // 6.x（≤8.4）：数组字面量的常量池条目是 ConstantElementsPair —— 一个 Tuple2
        // {elements_kind(Smi), constant_values(FixedArrayBase)}。Tuple2 是 Struct、
        // 没有长度字段（array_len 读回 0），值落在固定槽位上：array_elem(o,0) 的
        // (2+0)*ts 偏移正好是 value2，也就是常量值数组。
        if let Some(e) = self.six_x_constant_elements(o) {
            return e;
        }
        // 6.8 的对象字面量常量带专属 map（BoilerplateDescriptionMap）；6.2 共用
        // FixedArrayMap，只能按形状认 —— 那种情况只在字面量调用点（lit_ctx）才敢解。
        if ty.name(self.d.table) == "BoilerplateDescriptionMap" {
            if let Some(e) = self.six_x_object_boilerplate(o) {
                return e;
            }
        }
        if ty.is(self.d.table, "FixedArray") {
            if self.d.lit_ctx.get() {
                if let Some(e) = self.six_x_object_boilerplate(o) {
                    return e;
                }
            }
            // 类方法表 / 跳转表 / 元素数组：把真实元素解出来（原先只给 /*0*/ 占位，
            // 于是类的构造函数、方法、私有名全看不出内容）。
            let n = self.d.cache.array_len(o);
            let mut items = Vec::new();
            for i in 0..n.min(16) {
                items.push(self.fixed_elem_expr(o, i));
            }
            return Expr::ArrayLit(items);
        }
        // 未知常量：给出类型线索（注释）但落到语法合法、运行不抛错的值上
        Expr::Ident(format!("/* {}({o}) */ undefined", ty.name(self.d.table)))
    }

    /// FixedArray/常量值数组的单个元素 → 表达式（Smi / 字符串 / 嵌套字面量 / root）。
    /// 嵌套的对象/数组字面量也以同样形态折在父 boilerplate 里 → 解码元素时保持 lit_ctx。
    fn fixed_elem_expr(&mut self, o: ObjId, i: usize) -> Expr {
        let save = self.d.lit_ctx.get();
        self.d.lit_ctx.set(true);
        let e = self.fixed_elem_expr_inner(o, i);
        self.d.lit_ctx.set(save);
        e
    }

    fn fixed_elem_expr_inner(&mut self, o: ObjId, i: usize) -> Expr {
        if self.d.is_double_array(o) {
            return match self.d.double_elem(o, i) {
                Some(v) => Expr::Num(v),
                None => Expr::Undefined,
            };
        }
        match self.d.cache.array_elem(o, i) {
            Some(Elem::Smi(v)) => Expr::Num(v as f64),
            Some(Elem::Ref(Ref::Object(x))) => self.object_constant(x),
            Some(Elem::Ref(Ref::Root(r))) => self.root_value(r),
            Some(Elem::Ref(Ref::RoRef(c, off))) => {
                match self.d.ro_map.as_deref().and_then(|m| m.get(c, off)) {
                    Some(name) => Expr::Str(name.to_string()),
                    None => Expr::Str(format!("<ro{c}_{off}>")),
                }
            }
            _ => Expr::Hole,
        }
    }

    /// 6.x 数组字面量：ConstantElementsPair{kind, values}（详见 object_constant 的说明）。
    fn six_x_constant_elements(&mut self, o: ObjId) -> Option<Expr> {
        let tyname = self.d.cache.obj(o).ty.name(self.d.table).to_string();
        if !tyname.ends_with("Tuple2Map") {
            return None;
        }
        // 槽位 (map, value1=elements_kind, value2=constant_values)：array_elem(o,0)
        // 的偏移 (2+0)*ts 正好落在 value2，也就是常量值数组。
        let holder = match self.d.cache.array_elem(o, 0) {
            Some(Elem::Ref(Ref::Object(inner))) => inner,
            // **空数组**的 constant_values 是 `EmptyFixedArray` 这个**根**（不是对象）——
            // 6.x 的 `[...gen()]`（先建空数组再 StaInArrayLiteral 填）整段靠它：
            // 漏掉就渲染成 `/* Tuple2Map */ undefined`，`r3[r4] = v` 直接 TypeError。
            Some(Elem::Ref(Ref::Root(r))) => {
                let n = self.d.table.root_name(r).unwrap_or("");
                return if n.contains("EmptyFixedArray") {
                    Some(Expr::ArrayLit(Vec::new()))
                } else {
                    None
                };
            }
            _ => return None,
        };
        let n = self.d.cache.obj(holder).ty.name(self.d.table).to_string();
        if n.contains("Fixed") && n.contains("Array") {
            return Some(self.fixed_array_literal(holder));
        }
        None
    }

    /// FixedArray 常量值序列 → `[ … ]`（非全常量位在 6.x 里是 the_hole）。
    fn fixed_array_literal(&mut self, holder: ObjId) -> Expr {
        let n = self.d.cache.array_len(holder);
        let mut items = Vec::new();
        for i in 0..n.min(64) {
            items.push(self.fixed_elem_expr(holder, i));
        }
        Expr::ArrayLit(items)
    }

    /// 6.x 对象字面量：BoilerplateDescription = FixedArray，可选首格属性数（Smi），
    /// 随后 (键, 值) 交替。形状不符时返回 None（普通 FixedArray 交回原来的渲染）。
    fn six_x_object_boilerplate(&mut self, o: ObjId) -> Option<Expr> {
        let len = self.d.cache.array_len(o);
        if len == 0 || len > 512 {
            return None;
        }
        // 计数格的位置随形态变（实测）：
        //   偶数长度：[k, v, …]（如 `{b: 2}` → ["b", 2]）
        //   奇数长度：[count, k, v, …]（首格计数）或 [k, v, …, count]（**尾格**计数，
        //   6.2/6.8/7.8 的 `{a, ...x, c: 3}` 就是 ["a", hole, 3]，count=3 是含展开属性的总数）
        let (start, count) = if len.is_multiple_of(2) {
            (0usize, len / 2)
        } else {
            match self.d.cache.array_elem(o, 0) {
                Some(Elem::Smi(c)) if c >= 0 && c as usize * 2 + 1 == len => (1usize, c as usize),
                _ => match self.d.cache.array_elem(o, len - 1) {
                    Some(Elem::Smi(_)) => (0usize, (len - 1) / 2),
                    _ => return None,
                },
            }
        };
        if count == 0 {
            return None;
        }
        // 键必须是字符串：跳转表/方法表之类会被这一条挡掉
        for i in 0..count {
            let ok = match self.d.cache.array_elem(o, start + 2 * i) {
                Some(Elem::Ref(Ref::Object(k))) => self.d.cache.obj(k).ty.is_string(self.d.table),
                Some(Elem::Ref(Ref::Root(r))) => {
                    self.d.table.root_name(r).is_some_and(|n| n.starts_with("String:"))
                }
                Some(Elem::Smi(_)) => true,
                _ => false,
            };
            if !ok {
                return None;
            }
        }
        let mut parts = Vec::new();
        for i in 0..count.min(64) {
            let key = self.elem_key(o, start + 2 * i);
            let val = self.fixed_elem_expr(o, start + 2 * i + 1);
            let k = match key {
                Some(k) if is_ident(&k) => k,
                Some(k) => format!("[{}]", js_string(&k)),
                None => "[__ctx.__computed]".to_string(),
            };
            parts.push((k, val));
        }
        Some(Expr::ObjectLit(parts))
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

    /// 类字面量的 boilerplate → `{ n: 动态参数数, i: { 属性名: 下标 | {get,set} } }`。
    ///
    /// V8 布局（FixedArray；13.6 起类型是 ClassBoilerplateMap、少了 args_count 格）：
    ///   [args_count, 静态属性模板, 静态元素字典, 静态计算名数组,
    ///    实例属性模板, 实例元素字典, 实例计算名数组]
    /// 模板是 DescriptorArray：头三格 [map, 打包计数, enum cache]，条目三格
    /// **(key, details, value)**（V8 `kEntryKeyIndex/kEntryDetailsIndex/kEntryValueIndex`）。
    /// 方法条目的 value = 动态参数下标（Smi，3 起是闭包），访问器条目 = AccessorPair
    /// （[map, getter, setter]，未设置的一侧是 null）—— 见 runtime-classes.cc 的
    /// SubstituteValues / GetMethodAndSetName。旧版把模板整块当数组打出来，
    /// 于是 `bump`/`value` 这类**只存在于 boilerplate 里**的方法名（10.2 起 SFI 不再带
    /// 推断名）全丢，node18+ 的类只能靠函数名挂方法 → `c.bump is not a function`。
    fn class_boilerplate(&mut self, o: ObjId) -> Option<Expr> {
        let len = self.d.cache.array_len(o);
        let tyname = self.d.cache.obj(o).ty.name(self.d.table).to_string();
        let is_typed = tyname == "ClassBoilerplateMap";
        if std::env::var("JSCD_DBG_CLS").is_ok() {
            eprintln!("[cls] enter o={o} len={len} ty={tyname}");
        }
        // 形状判定：7 格 FixedArray，首格是 Smi 参数数，第 2/5 格是模板对象
        let (args_idx, inst_idx) = if is_typed && len >= 7 {
            // ClassBoilerplateMap（13.6 起）：[静态模板, 静态元素, 静态计算名, 实例模板, …]
            (None, 4usize)
        } else if is_typed && len >= 4 {
            // 12.4 的 ClassBoilerplateMap 只有 6 格，没有 args_count
            (None, 3usize)
        } else if len == 7 {
            // ≤12.x 的普通 FixedArray 形态：[args_count, 静态模板, …, 实例模板, …]
            (Some(0usize), 4usize)
        } else if len >= 8 {
            (Some(0usize), 5usize)
        } else {
            return None;
        };
        let args = args_idx
            .and_then(|i| self.d.cache.array_elem(o, i))
            .and_then(|e| e.as_smi())
            .map(|v| v as f64);
        if args_idx.is_some() && args.is_none() {
            return None;
        }
        // 模板必须是"对象"（DescriptorArray / 字典）而不是 Smi/根
        let inst = match self.d.cache.array_elem(o, inst_idx) {
            Some(Elem::Ref(Ref::Object(x))) => x,
            _ => return None,
        };
        if std::env::var("JSCD_DBG_CLS").is_ok() {
            eprintln!("[cls] o={o} len={len} ty={tyname} args={args:?} inst={inst}");
        }
        let entries = self.class_template_entries(inst)?;
        let n = args.unwrap_or(0.0);
        let inst_lit = Expr::ObjectLit(
            entries
                .iter()
                .map(|(k, v)| (js_string(k), v.clone()))
                .collect(),
        );
        Some(Expr::ObjectLit(vec![
            ("n".to_string(), Expr::Num(n)),
            ("i".to_string(), inst_lit),
        ]))
    }

    /// 类模板（DescriptorArray / 字典）→ [(属性名, 下标 | {get,set})]。
    ///
    /// DescriptorArray 的第 1 格不是 Smi 而是**打包头**（低 16 位 = 含 slack 的总数、
    /// 次 16 位 = 描述符个数、再往上是被标记数与对齐填充），所以 `array_len()` 读成 0，
    /// 必须直接解字节。条目三格是 (key, details, value)；字典形态则是 (key, value, details)，
    /// 这里按"值像不像下标/AccessorPair"来分辨。
    fn class_template_entries(&mut self, o: ObjId) -> Option<Vec<(String, Expr)>> {
        let ts = self.d.ts;
        let packed = self
            .d
            .cache
            .raw_at_ts(o, ts, ts, ts)
            .and_then(|d| <[u8; 8]>::try_from(d).ok())
            .map(u64::from_le_bytes);
        // 打包头判定：高 32 位为 0 且整个值非 0（Smi 的 8 字节形态高 32 位是值本身）
        let n_desc = match packed {
            Some(v) if v != 0 && (v >> 32) == 0 => ((v >> 16) & 0xffff) as usize,
            _ => 0,
        };
        let entries = self.class_entries_scan(o, n_desc);
        if std::env::var("JSCD_DBG_CLS").is_ok() {
            eprintln!(
                "[cls] template o={o} packed={packed:?} n_desc={n_desc} entries={}",
                entries.len()
            );
        }
        if entries.is_empty() {
            None
        } else {
            Some(entries)
        }
    }

    /// 扫模板条目：`n_desc > 0` 走 DescriptorArray（元素 1 起、3 格一组、值在第 3 格）；
    /// 否则按字典形态扫（值在第 2 格）。
    fn class_entries_scan(&mut self, o: ObjId, n_desc: usize) -> Vec<(String, Expr)> {
        let len = self.d.cache.obj(o).byte_size / self.d.ts;
        if std::env::var("JSCD_DBG_CLS").is_ok() {
            eprintln!(
                "[cls] scan o={o} n_desc={n_desc} len={len} byte_size={} ty={}",
                self.d.cache.obj(o).byte_size,
                self.d.cache.obj(o).ty.name(self.d.table)
            );
            for i in 0..len.min(12) {
                let desc = match self.d.cache.array_elem(o, i) {
                    Some(Elem::Smi(v)) => format!("smi {v}"),
                    Some(Elem::Ref(Ref::Object(x))) => {
                        let t = self.d.cache.obj(x).ty.name(self.d.table).to_string();
                        match self.d.dis.string_value(x) {
                            Some(sv) => format!("obj {x} {t} str={sv:?}"),
                            None => format!("obj {x} {t}"),
                        }
                    }
                    Some(Elem::Ref(r)) => format!("ref {r:?}"),
                    Some(Elem::Bytes(b)) => format!("bytes {}", b.len()),
                    None => "None".to_string(),
                };
                eprintln!("[cls]   cell[{i}] = {desc}");
            }
        }
        let mut out = Vec::new();
        if n_desc > 0 {
            // 元素空间（array_elem 的下标从 map/length 之后算起）：描述符 3 格一组
            // (key, details, value)。**起点随版本不同**：≤13.6 是 1 格头（enum cache），
            // 14.x 起多一格（实测模板数组 len 9→10、首格变成裸数据）→ 起点 2。
            // 与其按版本硬编码，不如按"第一组能不能解出键"试：键必须是字符串/字符串根，
            // 起点错了第一格就是 Smi/根，立刻判否。
            for start in 1usize..=3 {
                if start + 2 >= len {
                    break;
                }
                if self.template_key(o, start).is_none() {
                    continue;
                }
                let mut k = start;
                for _ in 0..n_desc.min(128) {
                    if k + 2 >= len {
                        break;
                    }
                    if let Some(key) = self.template_key(o, k) {
                        let v = self.template_value(o, k + 2);
                        if std::env::var("JSCD_DBG_CLS").is_ok() {
                            eprintln!("[cls]   desc start={start} k={k} key={key:?}");
                        }
                        out.push((key, v));
                    }
                    k += 3;
                }
                if !out.is_empty() {
                    return out;
                }
            }
            if std::env::var("JSCD_DBG_CLS").is_ok() {
                eprintln!("[cls]   desc 起点 1..3 都解不出键");
            }
            return out;
        }
        // 字典形态：从第 1 格起按 3 格一组，值在第 2 格（键、值、details）
        let mut k = 1usize;
        while k + 2 < len {
            if let Some(key) = self.template_key(o, k) {
                let v = self.template_value(o, k + 1);
                out.push((key, v));
                k += 3;
            } else {
                k += 1;
            }
            if out.len() >= 128 {
                break;
            }
        }
        out
    }

    /// 模板条目第 k 格若是"名字"（字符串对象 / String:/Symbol: 根）→ 名字文本。
    fn template_key(&mut self, o: ObjId, k: usize) -> Option<String> {
        let r = match self.d.cache.array_elem(o, k) {
            Some(Elem::Ref(r)) => r,
            other => {
                if std::env::var("JSCD_DBG_CLS").is_ok() {
                    eprintln!("[cls] key k={k} -> {other:?}");
                }
                return None;
            }
        };
        if let Ref::Root(i) = r {
            let n = self.d.table.root_name(i)?;
            let is_name = n.starts_with("String:") || n.starts_with("Symbol:");
            if !is_name {
                return None;
            }
        }
        // 只读堆字符串（单字符名等）只有经 ro-map 才能解出——`name_of_ref` 是自由函数、
        // 拿不到表，所以这里必须自己走一遍 map（与 `elem_key` 一致；漏掉会丢类方法名，
        // 于是 DefineClass 把方法挂成 `_anon_N`）。
        let s = if let Ref::RoRef(c, off) = r {
            self.d
                .ro_map
                .as_ref()
                .and_then(|m| m.get(c, off))
                .map(|s| s.to_string())?
        } else {
            crate::disasm::name_of_ref(self.d.cache, self.d.table, r)?
        };
        if s.is_empty() {
            return None;
        }
        Some(s)
    }

    /// 原始槽号处的 Smi（对象字段，如 AccessorPair 的 getter/setter）。
    fn raw_smi(&self, o: ObjId, slot: usize) -> Option<i64> {
        match self.d.cache.slot_at(o, slot)? {
            SlotValue::Raw(d) => {
                crate::serializer::decode_smi_bytes(self.d.cache.raw_bytes(*d))
            }
            _ => None,
        }
    }

    /// 模板条目的"值"格 → 动态参数下标（Smi）或访问器对 `{get,set}`（AccessorPair）。
    fn template_value(&mut self, o: ObjId, k: usize) -> Expr {
        match self.d.cache.array_elem(o, k) {
            Some(Elem::Smi(i)) => Expr::Num(i as f64),
            Some(Elem::Ref(Ref::Object(p))) => {
                // AccessorPair 是普通字段对象 [map, getter, setter]（不是 FixedArray），
                // 所以按**原始槽号**取；未设置的一侧是 null（不是 Smi）
                let g = self.raw_smi(p, 1);
                let s = self.raw_smi(p, 2);
                if g.is_none() && s.is_none() {
                    Expr::Num(-1.0)
                } else {
                    let mut kv = Vec::new();
                    if let Some(g) = g {
                        kv.push(("get".to_string(), Expr::Num(g as f64)));
                    }
                    if let Some(s) = s {
                        kv.push(("set".to_string(), Expr::Num(s as f64)));
                    }
                    Expr::ObjectLit(kv)
                }
            }
            _ => Expr::Num(-1.0),
        }
    }

    /// 对象字面量的"键为字符串"打分（用于自动判定 kDescriptionStartIndex）。
    fn obp_string_keys(&self, o: ObjId, start: usize, len: usize) -> usize {
        let count = if start >= 2 { len / 2 } else { len.saturating_sub(start) / 2 };
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
        // 版本给先验，语料给证据：字符串键更多的那种布局胜出。
        // 12.x 起头部多了 BackingStoreSize/Flags 两个额外字段（ObjectBoilerplateDescriptionShape
        // 的 kBackingStoreSizeOffset/kFlagsOffset），元素从第 2 格起 —— 只按 major>=13
        // 判会让 node22（12.4）把 flags 当键、条目数算成 0，对象字面量整块变 `{ }`。
        let v8_minor: u32 = self
            .d
            .table
            .v8
            .split('.')
            .nth(1)
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let has_extra_fields = v8_major >= 12;
        let _ = v8_minor;
        let (pref, alt) = if has_extra_fields {
            (2usize, 1usize)
        } else {
            (1usize, 2usize)
        };
        let start = if len >= 4 && self.obp_string_keys(o, alt, len) > self.obp_string_keys(o, pref, len) {
            alt
        } else {
            pref
        };
        // 13.x 的 ObjectBoilerplateDescription 前两格是 backing_store_size/flags（额外字段），
        // 元素从第 2 格起、数量正好是 len/2；≤12.4 则从第 1 格起、数量 (len-1)/2。
        let count = if start >= 2 {
            len / 2
        } else {
            len.saturating_sub(start) / 2
        };
        let mut parts = Vec::new();
        for i in 0..count.min(64) {
            let key = self.elem_key(o, start + 2 * i);
            let val = match self.d.cache.array_elem(o, start + 2 * i + 1) {
                Some(Elem::Smi(v)) => Expr::Num(v as f64),
                Some(Elem::Ref(Ref::Object(v))) => self.object_constant(v),
                Some(Elem::Ref(Ref::Root(i))) => self.root_value(i),
                Some(Elem::Ref(Ref::RoRef(c, off))) => {
                    match self.d.ro_map.as_deref().and_then(|m| m.get(c, off)) {
                        Some(n) => Expr::Str(n.to_string()),
                        None => Expr::Str(format!("<ro{c}_{off}>")),
                    }
                }
                _ => Expr::Undefined,
            };
            let k = match key {
                Some(k) if is_ident(&k) => k,
                Some(k) => format!("[{}]", js_string(&k)),
                // 空 computed 键是语法错误 → 落到已声明的命名空间对象（求值为 undefined）
                None => "[__ctx.__computed]".to_string(),
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
            // 双精度数组的元素是原始 f64 —— 必须走 8 字节解码；否则 Smi 判定
            // （低 32 位为 0）会与低半为零的 double（1.5、-0、0.5…）撞车，
            // 把高半截成整数（`[1.5]` → `[1073217536]`）。
            if self.d.is_double_array(holder) {
                items.push(match self.d.double_elem(holder, i) {
                    Some(v) => Expr::Num(v),
                    None => Expr::Undefined,
                });
                continue;
            }
            match self.d.cache.array_elem(holder, i) {
                Some(Elem::Smi(v)) => items.push(Expr::Num(v as f64)),
                Some(Elem::Ref(Ref::Object(oid))) => items.push(self.object_constant(oid)),
                Some(Elem::Ref(Ref::Root(r))) => items.push(self.root_value(r)),
                // 只读堆里的字符串（如 "a"）只能靠 ro-map 还原
                Some(Elem::Ref(Ref::RoRef(c, off))) => {
                    items.push(match self.d.ro_map.as_deref().and_then(|m| m.get(c, off)) {
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
    ///
    /// `via_current` 区分两类读取：
    /// - `Lda*CurrentContextSlot`：读的是**当前**上下文。catch 体里当前上下文就是 catch 上下文，
    ///   槽 `MIN_CONTEXT_SLOTS` 是抛出对象 ⇒ 用 `catch_alias`（`e`）与最内层作用域的名字。
    /// - `LdaContextSlot <reg>, [slot], [depth]`：上下文在**寄存器**里。6.x/8.x 的
    ///   `PushContext <reg>`（RegOut）写进寄存器的是**旧上下文**（`PopContext <reg>` 靠它恢复），
    ///   所以 catch 体里这类读取读的是外层（函数）上下文 —— 不能用 catch 别名/最内层作用域，
    ///   否则 `await q`（外层参数）会被读成 `e`（node10 的 retry 就栽在这）。
    ///
    /// 6.x async 的形态判据（与 `plan_async` 内联的那套同源；那边要拿它算 `enter`/续体，
    /// 这里只回答"是不是 async 机器"）。
    ///
    /// - ① 具名结算调用：`ResolvePromise`/`RejectPromise`/`AsyncFunctionResolve/Reject`
    ///   （6.8+ 名字能解出来）
    /// - ② `CreateJSGeneratorObject` 之后 12 条内出现"数字名调用"（6.2：async 拿它当状态
    ///   对象，紧跟 `AsyncFunctionEnter`；生成器那里是 StackCheck）
    /// - ③ 尾部 `SwitchOnSmiNoFeedback` 的 case 0 块里有数字名调用（6.2 的 async **函数体里
    ///   一个 await 都没有** → 连 CreateJSGeneratorObject 都没有；普通生成器的 case 块
    ///   是续体，不会有数字名结算调用）
    ///
    /// 为什么要单独判：6.2 的 ScopeInfo function_kind 恒 0、也没有 `Await` 操作码 ——
    /// 漏判会把 async 函数当普通函数输出，调用返回 undefined 而不是 promise。
    fn looks_async_6x(&self) -> bool {
        let n = self.instrs.len();
        let base = |k: usize| -> String {
            self.instrs[k]
                .name
                .split('.')
                .next()
                .unwrap_or(&self.instrs[k].name)
                .to_string()
        };
        let numeric_call = |k: usize| -> bool {
            matches!(base(k).as_str(), "CallJSRuntime" | "CallRuntime")
                && self
                    .call_name(k)
                    .map(|x| !x.is_empty() && x.chars().all(|c| c.is_ascii_digit()))
                    .unwrap_or(false)
        };
        for k in 0..n {
            if matches!(
                self.call_name(k).as_deref(),
                Some("ResolvePromise")
                    | Some("RejectPromise")
                    | Some("AsyncFunctionResolve")
                    | Some("AsyncFunctionReject")
            ) {
                return true;
            }
        }
        if let Some(p) = (0..n).find(|&k| matches!(self.call_name(k).as_deref(), Some("CreateJSGeneratorObject"))) {
            if (p..n.min(p + 12)).any(numeric_call) {
                return true;
            }
        }
        if let Some(sw) = (0..n).rev().find(|&k| base(k) == "SwitchOnSmiNoFeedback") {
            for (v, t) in self.switch_cases(sw) {
                if v != 0 {
                    continue;
                }
                if let Some(&ci) = self.idx_of.get(&t) {
                    if (ci..n.min(ci + 12)).any(numeric_call) {
                        return true;
                    }
                }
            }
        }
        false
    }

    /// 把 [from, to) 的指令段渲染成**一个表达式文本**（纯寄存器临时赋值会被就地内联）。
    ///
    /// 用于"参数默认值"重写：默认值的机器码一般只算进 acc（必要时经一两个临时寄存器），
    /// 能整段折成表达式就能搬进签名（求值时机/`fn.length` 都跟着回正）。
    /// 出现真正的语句（循环、分支、非临时赋值…）就返回 None —— 调用方保持原样，不冒险。
    fn render_range_as_expr(
        &mut self,
        from: usize,
        to: usize,
        seed: &HashMap<String, String>,
    ) -> Option<String> {
        if to <= from {
            return None;
        }
        // 快照（这一段只是"试渲染"，不能污染真发射的状态）
        let saved_out = std::mem::take(&mut self.out);
        let saved_acc = self.acc.take();
        let saved_indent = self.indent;
        let saved_last = std::mem::take(&mut self.last_line);
        let phi_len = self.phi_vars.len();
        let skip_len = self.skip_spans.len();
        let tmp = self.tmp_counter;
        self.indent = 1;
        let r = self.emit_range(from, to);
        let scratch = std::mem::replace(&mut self.out, saved_out);
        let acc = self.acc.take();
        self.acc = saved_acc;
        self.indent = saved_indent;
        self.last_line = saved_last;
        self.phi_vars.truncate(phi_len);
        self.skip_spans.truncate(skip_len);
        self.tmp_counter = tmp;
        if let Err(e) = &r {
            if std::env::var("JSCD_DBG_PDEF").is_ok() {
                eprintln!("[pdef-x] 段 [{from},{to}) 试渲染失败: {e}");
            }
        }
        r.ok()?;
        let Some(expr) = acc else {
            if std::env::var("JSCD_DBG_PDEF").is_ok() {
                eprintln!("[pdef-x] 段 [{from},{to}) 没有产出 acc；scratch={scratch:?}");
            }
            return None;
        };
        // 纯临时赋值 → 收进替换表；其余任何一行（含注释）都放行不了
        let mut map = seed.clone();
        for line in scratch.lines() {
            let t = line.trim();
            if t.is_empty() || t.starts_with("/*") || t.starts_with("//") {
                continue;
            }
            let body = t.strip_suffix(';').unwrap_or(t);
            let Some((lhs, rhs)) = body.split_once('=') else {
                if std::env::var("JSCD_DBG_PDEF").is_ok() {
                    eprintln!("[pdef-x] 段 [{from},{to}) 出现非赋值行 {t:?}");
                }
                return None;
            };
            let (lhs, rhs) = (lhs.trim(), rhs.trim());
            if !(lhs.starts_with('r') || lhs.starts_with('t')) || rhs.starts_with('=') {
                if std::env::var("JSCD_DBG_PDEF").is_ok() {
                    eprintln!("[pdef-x] 段 [{from},{to}) 非临时赋值行 {t:?}");
                }
                return None;
            }
            map.insert(lhs.to_string(), substitute_names(rhs, &map));
        }
        let text = substitute_names(&expr.render(), &map);
        // 结果里不能再残留没解析的寄存器/临时名（那会引用签名里不存在的变量）
        if references_unknown_reg(&text) {
            if std::env::var("JSCD_DBG_PDEF").is_ok() {
                eprintln!("[pdef-x] 段 [{from},{to}) 替换后仍含寄存器: {text:?} map={map:?}");
            }
            return None;
        }
        Some(text)
    }


    /// 参数下落槽：序言里的 `Ldar aK; StaCurrentContextSlot/StaContextSlot [S]` → 槽 S 就是参数 K。
    ///
    /// 生成器/async 的计划里各自扫过一遍，但**条件不同**：6.x 的 async 走 CallRuntime、
    /// 没有 `Await` 操作码 → 那两处的循环区间为空、一次都不执行；于是 6.2 async 的形参拷贝槽
    /// 按脚本作用域解名（`out`/`log`），把脚本级变量覆盖掉（探针里 `.then` 回读 `out.push`
    /// 直接 TypeError）。这里与机器形态无关地统一扫一遍序言。
    fn plan_param_slots(&mut self) {
        let n = self.instrs.len();
        let base = |this: &Self, k: usize| -> String {
            this.instrs
                .get(k)
                .map(|x| x.name.split('.').next().unwrap_or(&x.name).to_string())
                .unwrap_or_default()
        };
        // 序言很短（生成器/async 的包装器 + 参数下落），限个前缀避免扫进循环体
        for k in 0..n.min(64).saturating_sub(1) {
            if base(self, k) != "Ldar" {
                continue;
            }
            let Some(op) = self.instrs[k].operands.first().cloned() else {
                continue;
            };
            if !matches!(op, Operand::Reg(_)) {
                continue;
            }
            let param = self.render_operand(&op);
            if !param.starts_with('a') || param == "a" {
                continue;
            }
            let nxt = base(self, k + 1);
            let is_ctx_store = matches!(
                nxt.as_str(),
                "StaCurrentContextSlot"
                    | "StaContextSlot"
                    | "StaCurrentScriptContextSlot"
                    | "StaCurrentContextSlotNoCell"
                    | "StaContextSlotNoCell"
            );
            if !is_ctx_store {
                continue;
            }
            if let Some(Operand::Idx(slot)) = self.instrs[k + 1].operands.first() {
                let slot = *slot as usize;
                self.gen_param_slots.insert(slot, param.clone());
                // 形参先落上下文、再 `Star rN` 拷进寄存器（6.x 的生成器/async：
                // 那份 prologue 会被 plan_generator/plan_async 整段丢掉）→ 直接把该
                // 寄存器预置成参数名，体里读 `rN` 就渲染成 `aK`。
                if let Some(r) = self.star_reg_text(k + 2) {
                    if let Ok(r) = r.trim_start_matches('r').parse::<u32>() {
                        if self.regs.len() <= r as usize {
                            self.regs.resize(r as usize + 1, None);
                        }
                        self.regs[r as usize] = Some(Expr::Ident(param.clone()));
                    }
                }
            }
        }
        // 反向：`Star rN` 之后紧接着 `Lda*ContextSlot [S]`（S→aK）也把 rN 当参数名
        // —— 上面的顺序依赖 `Ldar aK; Sta…`，这里覆盖 `Lda…Slot; Star rN` 的写法。
        for k in 0..n.min(64).saturating_sub(2) {
            let b = base(self, k);
            if !(b.starts_with("Lda") && b.contains("ContextSlot")) {
                continue;
            }
            let Some(Operand::Idx(slot)) = self.instrs[k].operands.first() else {
                continue;
            };
            let Some(param) = self.gen_param_slots.get(&(*slot as usize)).cloned() else {
                continue;
            };
            if let Some(r) = self.star_reg_text(k + 1) {
                if let Ok(r) = r.trim_start_matches('r').parse::<u32>() {
                    if self.regs.len() <= r as usize {
                        self.regs.resize(r as usize + 1, None);
                    }
                    self.regs[r as usize] = Some(Expr::Ident(param));
                }
            }
        }
    }

    /// 参数默认值重写：`Ldar aK; JumpIfNotUndefined <else>; <默认值…>; Jump <join>;
    /// <else: Ldar aK>; <join: Star rV>` → 签名里的 `aK = <expr>`。
    ///
    /// 为什么要搬进签名：默认值在源码里是**调用时**求值（参数绑定阶段）——生成器/async
    /// 也一样（体要等第一次 next()），内联进体会让"抛错的默认值"不再同步抛、副作用延后；
    /// 形参表不带默认值还会让 `fn.length` 变大。
    fn plan_param_defaults(&mut self) {
        // **普通 async 函数跳过**：它的形参绑定在 async 机制内部，默认值抛错会变成
        // **rejected promise**（实测：sync/生成器/async 生成器都是同步抛，只有普通 async 不是）。
        // 搬进签名会把它变成同步抛 —— 那是语义改动，不做；保留原来的体内内联。
        //
        // 判定要**版本无关**：6.x 的 async 机器走 `CallRuntime`，没有 `Await` 操作码 →
        // 只看 opcode 会漏（10.24 的 async 因此曾被重写、默认值变成同步抛）。
        // ScopeInfo 的 function_kind 两代都有（`Scope::is_async/is_generator`）。
        // 普通 async 函数**也**搬进签名：原生语义下 async 的形参绑定就在 async 机制内部
        // （实测 `async function f(a = <抛错>)` 的 `f()` 同步不抛、promise reject）——
        // 搬到签名 == 把机器码换回"真正的 async 形参"，两头都对（`.length` 也回正）。
        // 6.x 的 async 机器重建会把序言搬出 async 外壳，那种形态下面再单独兜（见 probe 记录）。
        let n = self.instrs.len();
        let base = |this: &Self, k: usize| -> String {
            this.instrs
                .get(k)
                .map(|x| x.name.split('.').next().unwrap_or(&x.name).to_string())
                .unwrap_or_default()
        };
        // 前导里 `Mov aK, rN` 的拷贝：让默认值表达式里的 `rN` 直接换成参数名 `aK`
        // （默认值 prologue 会把先前的形参拷进寄存器给后面的默认值用）
        let mut seed: HashMap<String, String> = HashMap::new();
        // 生成器的前导窗口：**首个 `SuspendGenerator` 之前**。那段里除了包装器建立就是
        // 参数/默认值的机器码（用户代码最早也要到第一个 yield 才可能执行）→ 可以宽松地
        // 往前扫；普通函数没有这个边界，仍走严格白名单（免得把体里的 `if (a !== undefined)`
        // 之类误当成默认值）。
        let generator_like = self.is_generator;
        let window_end = if generator_like {
            self.instrs
                .iter()
                .position(|x| x.name.starts_with("SuspendGenerator"))
                .unwrap_or(n)
        } else {
            n
        };
        let mut k = 0usize;
        // 6.x 生成器的"新调用路径"在入口分派**之后**（线性扫描先撞上 `SwitchOnSmiNoFeedback`）：
        // 记下第一个前向跳转的目标，撞墙时重扫一次那里（有界，只重扫一次）。
        let mut restart: Option<usize> = None;
        let mut restarted = false;
        while k < window_end {
            // 允许前导杂项穿插：StackCheck/生成器与 async 的包装器建立
            // （`SwitchOnGeneratorState`、`Mov <closure>/<this>/<context>`、
            // `_CreateJSGeneratorObject`/`_AsyncFunctionEnter`、存进寄存器的 Star、
            // 上下文搬运…）。**只有**这些能跳过 —— 碰到别的（调用/挂起/分支合并）
            // 就停下，宁可保持现状。
            let b = base(self, k);
            let preamble = matches!(
                b.as_str(),
                "StackCheck"
                    | "Nop"
                    | "SwitchOnGeneratorState"
                    | "InvokeIntrinsic"
                    | "PushContext"
                    | "PopContext"
                    | "CreateFunctionContext"
                    | "CreateFunctionContextWithCells"
                    | "LdaTheHole"
                    | "SetPendingMessage"
                    | "LdaUndefined"
                    | "Ldar"
                    | "StaCurrentContextSlot"
                    | "StaContextSlot"
                    | "GetNamedProperty"
                    | "LdaConstant"
                    | "LdaGlobal"
                    | "RestoreGeneratorState"
                    | "RestoreGeneratorRegisters"
                    | "SuspendGenerator"
            ) || b.starts_with("Star")
                // 生成器入口的 `Ldar r0; JumpIfUndefined <fresh>`（"新调用还是续体"）
                // 与普通分支都在前导里 —— 扫描时跳过它们**不影响发射**（这里只是找模式）
                || b.starts_with("Jump")
                || (b.starts_with("Lda") && b.contains("ContextSlot"))
                // 6.x async 序言里的 `CallJSRuntime [数字]`（= AsyncFunctionEnter，
                // 名字表解不出）也是机器码；不认它会让默认值的扫描在它这里断掉
                // （10.24 的 async 默认值因此一直没被搬进签名）
                || (matches!(b.as_str(), "CallJSRuntime" | "CallRuntime")
                    && self
                        .call_name(k)
                        .map(|x| !x.is_empty() && x.chars().all(|c| c.is_ascii_digit()))
                        .unwrap_or(false))
                || b == "Mov";
            // 默认值起点有两种形态（都要求紧跟 `JumpIfNotUndefined`）：
            //   ① `Ldar aK`（9.x+：形参直接从参数寄存器读）
            //   ② `Lda*CurrentContextSlot [S]`（6.x 的生成器/async：形参先被搬进上下文槽，
            //      槽→形参由 `plan_param_slots` 给出）
            let (is_default_start, ctx_form) = if b == "Ldar" {
                let pn = param_slot_name(
                    &self
                        .instrs[k]
                        .operands
                        .first()
                        .map(|o| self.render_operand(o))
                        .unwrap_or_default(),
                );
                (pn.is_some() && base(self, k + 1) == "JumpIfNotUndefined", false)
            } else if b.starts_with("Lda") && b.ends_with("CurrentContextSlot") {
                let slot = self.instrs[k]
                    .operands
                    .first()
                    .and_then(|o| match o {
                        Operand::Idx(v) => Some(*v as usize),
                        _ => None,
                    })
                    .unwrap_or(usize::MAX);
                (
                    self.gen_param_slots.contains_key(&slot)
                        && base(self, k + 1) == "JumpIfNotUndefined",
                    true,
                )
            } else {
                (false, false)
            };
            if std::env::var("JSCD_DBG_PDEF").is_ok() {
                eprintln!("[pdef-scan] k={k} base={b} start={is_default_start} preamble={preamble}");
            }
            // 任何前向跳转都记下目标；扫描撞墙时重扫一次那里（生成器/async 的入口分派）
            if !is_default_start && restart.is_none() && b.starts_with("Jump") {
                if let Some(t) = self.cond_jump_target(&self.instrs[k].clone()).or_else(|| {
                    self.uncond_jump_target(&self.instrs[k].clone())
                }) {
                    if let Some(&ti) = self.idx_of.get(&t) {
                        if ti > k {
                            restart = Some(ti);
                        }
                    }
                }
            }
            if !is_default_start {
                if preamble || generator_like {
                    // 前导里的 `Mov aK, rN`：让默认值表达式里的 `rN` 直接换成参数名 `aK`
                    // （默认值 prologue 会把先前的形参拷进寄存器给后面的默认值用）
                    if b == "Mov" {
                        let ops: Vec<String> = self.instrs[k]
                            .operands
                            .iter()
                            .map(|o| self.render_operand(o))
                            .collect();
                        if let (Some(src), Some(dst)) = (ops.first(), ops.get(1)) {
                            if src.starts_with('a') && dst.starts_with('r') {
                                seed.insert(dst.clone(), src.clone());
                                // 预置寄存器映射：**生成器/async 的 prologue 会被
                                // plan_generator/plan_async 整段丢弃**（连 `Mov a0, r0`
                                // 与 fixup 一起），体里再读 `r0` 就是 undefined →
                                // 直接把该寄存器渲染成参数名（体里读 a0 ✓）。
                                if let Ok(r) = dst.trim_start_matches('r').parse::<u32>() {
                                    if self.regs.len() <= r as usize {
                                        self.regs.resize(r as usize + 1, None);
                                    }
                                    self.regs[r as usize] = Some(Expr::Ident(src.clone()));
                                }
                            }
                        }
                    }
                    k += 1;
                    continue;
                }
                if let Some(ti) = restart.filter(|_| !restarted) {
                    restarted = true;
                    k = ti;
                    continue;
                }
                break;
            }
            if !ctx_form && base(self, k) != "Ldar" {
                break;
            }
            let param = if ctx_form {
                let slot = match self.instrs[k].operands.first() {
                    Some(Operand::Idx(v)) => *v as usize,
                    _ => break,
                };
                match self.gen_param_slots.get(&slot) {
                    Some(p) => p.clone(),
                    None => break,
                }
            } else {
                match param_slot_name(
                    &self
                        .instrs[k]
                        .operands
                        .first()
                        .map(|o| self.render_operand(o))
                        .unwrap_or_default(),
                ) {
                    Some(p) => p,
                    None => break,
                }
            };
            let Some(param_idx) = param.strip_prefix('a').and_then(|x| x.parse::<usize>().ok()) else {
                break;
            };
            let bail = |why: &str| {
                if std::env::var("JSCD_DBG_PDEF").is_ok() {
                    eprintln!("[pdef-bail] k={k} {why}");
                }
            };
            if base(self, k + 1) != "JumpIfNotUndefined" {
                bail("下一跳不是 JumpIfNotUndefined");
                break;
            }
            let Some(else_off) = self.cond_jump_target(&self.instrs[k + 1].clone()) else {
                bail("取不到 else 目标");
                break;
            };
            let Some(&else_idx) = self.idx_of.get(&else_off) else {
                bail("else 目标不在指令表");
                break;
            };
            if else_idx <= k + 1 {
                bail("else 落后于起点");
                break;
            }
            let jump_k = else_idx - 1;
            if !base(self, jump_k).starts_with("Jump") {
                bail(&format!("else 前一条 {} 不是 Jump", base(self, jump_k)));
                break;
            }
            let Some(join_off) = self.uncond_jump_target(&self.instrs[jump_k].clone()) else {
                bail("取不到 join 目标");
                break;
            };
            let Some(&join_idx) = self.idx_of.get(&join_off) else {
                bail("join 目标不在指令表");
                break;
            };
            // else 分支必须是"同一个形参的同一种读法"，join 紧跟其后
            let else_base = base(self, else_idx);
            let else_ok = if ctx_form {
                else_base.starts_with("Lda")
                    && else_base.ends_with("CurrentContextSlot")
                    && self.instrs[else_idx]
                        .operands
                        .first()
                        .map(|o| match o {
                            Operand::Idx(v) => self.gen_param_slots.get(&(*v as usize)).map(|p| p == &param).unwrap_or(false),
                            _ => false,
                        })
                        .unwrap_or(false)
            } else {
                let else_param = self.instrs[else_idx]
                    .operands
                    .first()
                    .map(|o| self.render_operand(o))
                    .unwrap_or_default();
                else_base == "Ldar" && param_slot_name(&else_param).as_deref() == Some(param.as_str())
            };
            if !else_ok || join_idx != else_idx + 1 {
                bail(&format!(
                    "else/join 形态不符（{} / join_idx={join_idx} else_idx={else_idx}）",
                    else_base
                ));
                break;
            }
            let Some(expr) = self.render_range_as_expr(k + 2, jump_k, &seed) else {
                bail("默认值折不成表达式");
                break;
            };
            if std::env::var("JSCD_DBG_PDEF").is_ok() {
                eprintln!("[pdef] a{param_idx} = {expr}  (指令 {k}..={join_idx})");
            }
            if self.param_defaults.len() <= param_idx {
                self.param_defaults.resize(param_idx + 1, None);
            }
            self.param_defaults[param_idx] = Some(expr);
            // 这条 `Star rV` 改成 `rV = aK;`（体里可能还按寄存器读这个形参）
            // 目标寄存器：短形式 `Star1`/`Star0` 把寄存器编码在名字里、**没有操作数** →
            // 必须走 star_reg_text（早先只用 operands.first() 会静默漏掉：fixup 发不出来、
            // 后面的默认值也引用不到前一个默认值的寄存器 —— `c = a + b` 折不出来）。
            let dst_reg = self
                .star_reg_text(join_idx)
                .and_then(|n| n.trim_start_matches('r').parse::<u32>().ok());
            if let Some(r) = dst_reg {
                self.pregs_fixups.insert(join_idx, (param_idx, r));
            }
            // 记录 `rV → aK` 供**后面的**默认值使用（`c = a + b` 读的是 r1）
            if let Some(n) = self.star_reg_text(join_idx) {
                if n.starts_with('r') {
                    seed.insert(n.clone(), param.clone());
                }
            }
            // 同理预置：join 的目标寄存器按参数名渲染
            if let Some(r) = dst_reg {
                if self.regs.len() <= r as usize {
                    self.regs.resize(r as usize + 1, None);
                }
                self.regs[r as usize] = Some(Expr::Ident(param.clone()));
            }
            for j in k..=join_idx {
                if j != join_idx {
                    self.instrs[j].name = "__gskip".into();
                }
            }
            self.instrs[join_idx].name = "__pregs".into();
            k = join_idx + 1;
        }
    }

    fn context_name(&self, slot: usize, via_current: bool) -> String {
        self.context_name_base(slot, via_current, self.d.table.min_context_slots)
    }

    /// 显式上下文读取（`LdaContextSlot rN, [slot], [depth]` / `StaContextSlot …`）的名字：
    /// 从 rN 持有的上下文作用域出发、按 `depth` 沿外层链上溯，命中就地取名。
    ///
    /// 只看"最内层作用域"会让不同 block context 的**同号槽**串名：`for (const ctor of …)
    /// { const rab = …; }` 里 `ctor` 与 `rab` 都是各自上下文的槽 2，读 `ctor` 却取到
    /// 最内层的 `rab`（样本 coerced-searchelement 的 `rab.BYTES_PER_ELEMENT` 本该是
    /// `ctor.BYTES_PER_ELEMENT`，`new (rab = …)(…)` 直接 TypeError）。
    /// 从 `start` 作用域沿外层链走 `depth` 跳（**只数有 context 的作用域**：
    /// 闭包自己的 ScopeInfo 常常 `context_locals` 为空、根本没有独立上下文，
    /// 它的"当前上下文"就是外层那个）→ 命中就地取名。
    ///
    /// 只看"最内层作用域"会让不同 block context 的**同号槽**串名：`for (const ctor of …)
    /// { const rab = …; }` 里 `ctor` 与 `rab` 都是各自上下文的槽 2，读 `ctor` 却取到
    /// 最内层的 `rab`（样本 coerced-searchelement 的 `rab.BYTES_PER_ELEMENT` 本该是
    /// `ctor.BYTES_PER_ELEMENT`，`new (rab = …)(…)` 直接 TypeError）。
    fn context_name_walk(&self, start: ObjId, slot: usize, depth: usize, base: usize) -> Option<String> {
        let mut sid = start;
        let mut hops = depth;
        loop {
            let scope = self.d.scope_by_id(sid)?;
            let has_ctx = !scope.context_locals.is_empty() || scope.locals_table.is_some();
            if has_ctx {
                if hops == 0 {
                    let eff = if self.d.is_script_scope(scope.flags) {
                        self.d.script_ctx_base()
                    } else {
                        base
                    };
                    if slot < eff {
                        return None;
                    }
                    let n = scope.context_locals.get(slot - eff)?;
                    if n.is_empty() {
                        return None;
                    }
                    return Some(sanitize_var(n));
                }
                hops -= 1;
            }
            sid = scope.outer?;
        }
    }

    /// 显式上下文读取（`LdaContextSlot rN, [slot], [depth]`）的名字：从 rN 持有的
    /// 上下文作用域出发。
    fn context_name_via_reg(&self, reg: u32, slot: usize, depth: usize, base: usize) -> Option<String> {
        self.context_name_walk(*self.reg_scope.get(&reg)?, slot, depth, base)
    }

    /// 从**当前上下文**出发走 `depth` 跳外层链解槽名（`LdaImmutableContextSlot <context>,
    /// [slot], [depth]` 这类：depth=1 就是外层 block 作用域）。
    /// 当前上下文：块/闭包作用域栈顶；栈空（无 Create* 的闭包）→ 从函数自身作用域起。
    fn context_name_by_depth(&self, slot: usize, depth: usize, base: usize) -> Option<String> {
        let start = match self.ctx_scopes.last().copied().flatten() {
            Some(sid) => sid,
            None => self.scope_id?,
        };
        self.context_name_walk(start, slot, depth, base)
    }

    /// 操作数里显式的 `<reg>, [slot], [depth]` 三元组。
    fn context_reg_slot_depth(&self, ops: &[String]) -> Option<(u32, usize, usize)> {
        let reg: u32 = ops.first()?.trim_start_matches('r').parse().ok()?;
        let slot: usize = ops.get(1)?.trim_matches(['[', ']']).parse().ok()?;
        let depth: usize = ops
            .get(2)
            .and_then(|o| o.trim_matches(['[', ']']).parse().ok())
            .unwrap_or(0);
        Some((reg, slot, depth))
    }

    /// 显式上下文读取的 depth 操作数（第 3 个；缺省 0）。
    fn context_depth(&self, ops: &[String]) -> usize {
        ops.get(2)
            .and_then(|o| o.trim_matches(['[', ']']).parse().ok())
            .unwrap_or(0)
    }

    /// 带槽基准的版本：`*ScriptContextSlot`（13.x 新增）读的是**脚本上下文**，
    /// 它比普通上下文多一个 extension 槽（V8 `MIN_CONTEXT_EXTENDED_SLOTS =
    /// MIN_CONTEXT_SLOTS + 1`，`HasContextExtensionSlot`）→ 变量从 `min_context_slots + 1`
    /// 起。用 2 去减会让槽 3(A) 读成 `context_locals[1]`(B)：`class A{}; class B{}` 在
    /// node24 上互换了绑定（A.bump() 返回 B 的值）。
    fn context_name_base(&self, slot: usize, via_current: bool, base: usize) -> String {
        if via_current && slot == self.d.table.min_context_slots {
            if let Some(a) = &self.catch_alias {
                return a.clone();
            }
        }
        // 由 TDZ 检查反推出的别名最可靠（外层/模块 ScopeInfo 常常不在链上）
        if let Some(n) = self.slot_aliases.get(&slot) {
            if std::env::var("JSCD_DBG_SLOT").is_ok() {
                eprintln!("[slot] {slot} via_current={via_current} alias -> {n}");
            }
            return n.clone();
        }
        // 生成器序言搬进来的参数：形参就叫 aK
        if let Some(n) = self.gen_param_slots.get(&slot) {
            return n.clone();
        }
        // 块/catch 作用域优先（其 context 的槽 0 为该作用域的 ScopeInfo）
        let skip = if via_current { 0 } else { self.explicit_scope_skip };
        let len = self.ctx_scopes.len();
        if len > skip {
            if let Some(sid) = self.ctx_scopes[len - 1 - skip] {
                if let Some(s) = self.d.scope_by_id(sid) {
                    let eff = if self.d.is_script_scope(s.flags) { self.d.script_ctx_base() } else { base };
                    if std::env::var("JSCD_DBG_SLOT").is_ok() {
                        eprintln!(
                            "[slot] {slot} via_current={via_current} base={base} eff={eff} script={} locals={:?}",
                            self.d.is_script_scope(s.flags),
                            s.context_locals
                        );
                    }
                    // `slot < eff` 时**不能**索引：`saturating_sub` 会落到 0，把槽 0
                    // （catch context 的"抛出对象"格）读成作用域里的**第一个变量名**
                    // （22.12/24.12 的收尾守卫因此写成 `if (returnCalls !== undefined) throw
                    // returnCalls`，凭空抛一个数字出去）。
                    if slot >= eff {
                        if let Some(n) = s.context_locals.get(slot - eff) {
                            if !n.is_empty() {
                                return sanitize_var(n);
                            }
                        }
                    }
                }
            }
        }
        if let Some(n) = self.d.context_name_in_chain_base(self.scope_id, slot, base) {
            if !n.is_empty() {
                if std::env::var("JSCD_DBG_CTX").is_ok() {
                    eprintln!("[ctxname] slot={slot} base={base} -> {n} (chain)");
                }
                return sanitize_var(&n);
            }
        }
        // 名字解不出来时不要留裸标识符（会 ReferenceError）：
        // 落到一个已声明的命名空间对象上，读出来是 undefined、写进去也只是记账。
        if std::env::var("JSCD_DBG_CTX").is_ok() {
            eprintln!(
                "[ctxname] slot={slot} via_current={via_current} base={base} -> __ctx.ctx{slot} (scopes={:?})",
                self.ctx_scopes
            );
        }
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
            // 众所周知的符号（`Symbol.iterator` 等）：必须是**计算键**。
            // 写成 `obj["Symbol.iterator"]` 是"名为 Symbol.iterator 的字符串属性"，
            // 取到 undefined（6.x 的 @@iterator 走的就是这条命名属性加载）。
            Expr::Ident(n) if n.starts_with("Symbol.") => {
                Key::Computed(Box::new(Expr::Ident(n)))
            }
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
                | "Return" | "Throw" | "ReThrow" | "StaGlobal" | "StaGlobalSloppy" | "StaGlobalStrict" | "StaLookupSlot"
                | "StaNamedProperty" | "SetNamedProperty" | "StaKeyedProperty" | "SetKeyedProperty" | "StaNamedOwnProperty" | "DefineNamedOwnProperty"
                | "StaKeyedPropertyAsDefine" | "StaDataPropertyInLiteral" | "DefineKeyedOwnPropertyInLiteral" | "StaInArrayLiteral"
                | "DefineKeyedOwnProperty"
                | "Add" | "Sub" | "Mul" | "Div" | "Mod" | "Exp" | "BitwiseOr" | "BitwiseXor"
                | "BitwiseAnd" | "ShiftLeft" | "ShiftRight" | "ShiftRightLogical" | "TestEqual"
                | "TestEqualStrict" | "TestLessThan" | "TestLessThanOrEqual" | "TestGreaterThan"
                | "TestGreaterThanOrEqual" | "TestInstanceOf" | "TestIn" | "TestNull"
                | "TestUndefined" | "TestTypeOf" | "ToBooleanLogicalNot" | "LogicalNot" | "TestReferenceEqual"
                | "Negate" | "BitwiseNot" | "Inc" | "Dec" | "ToName" | "ToNumber" | "ToNumeric"
                | "ToString" | "ToObject" | "TypeOf" | "GetIterator" | "GetAsyncIterator"
                | "ForInEnumerate" | "CreateArrayFromIterable"
                | "CreateObjectFromIterable" | "Await" | "Yield" | "YieldStar" | "SuspendGenerator"
                | "ThrowReferenceErrorIfHole" | "JumpIfTrue" | "JumpIfFalse" | "JumpIfToBooleanTrue"
                | "JumpIfToBooleanFalse" | "JumpIfNull" | "JumpIfNotNull" | "JumpIfUndefined"
                | "JumpIfUndefinedOrNull" | "JumpIfNotUndefined" | "JumpIfJSReceiver"
                | "JumpIfNotHole" | "ThrowIfNotSuperConstructor" | "ThrowSuperNotCalledIfHole"
                | "TestUndetectable" | "CloneObject" | "DeletePropertyStrict" | "DeletePropertySloppy"
                | "ThrowSuperAlreadyCalledIfNotHole" | "CopyDataProperties"
                | "StaCurrentContextSlot" | "StaCurrentScriptContextSlot" | "StaContextSlot" | "StaScriptContextSlot" | "StaModuleVariable"
                | "AddSmi" | "SubSmi" | "MulSmi" | "DivSmi" | "ModSmi" | "ExpSmi"
                | "BitwiseOrSmi" | "BitwiseXorSmi" | "BitwiseAndSmi" | "ShiftLeftSmi" | "ShiftRightSmi"
                | "ShiftRightLogicalSmi" | "SwitchOnSmiNoFeedback" | "CallWithSpread"
                | "ConstructWithSpread" | "GetTemplateObject"
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

    // ───────────────────────── 生成器重写 ─────────────────────────
    //
    // V8 把 function* 编成一台状态机：序言建生成器对象并"起始挂起"，每个 yield 处
    // 把寄存器存进生成器对象再挂起，恢复后按 GeneratorGetResumeMode 分派
    // next / return / throw 三条路；`yield*` 更是一整圈 next/return/throw 委托协议
    // （GetIterator → 分派环 → 出口取 .value）。而 JS 的 function* 由引擎重做这一切，
    // 所以这里把这些机器指令标成 __gskip（不发射），只留一条 `yield` / `yield*`。
    //
    // 判据只用形态、不猜语义：分派 switch 的跳转表里同时有 case 1 与 case 2
    // （next/return/throw 三态）才算委托协议；被委托的挂起点没有 CreateIterResultObject
    // 包装（V8 把内层迭代器的结果对象直接转发给外层调用者）。

    /// SuspendGenerator/SwitchOnSmiNoFeedback 之类说"寄存器在哪个下标"。
    fn plan_generator(&mut self) {
        let n = self.instrs.len();
        if n == 0 {
            return;
        }
        // 先取一遍名字：下面要就地改 self.instrs（闭包借用会与改动冲突）
        let names: Vec<String> = self
            .instrs
            .iter()
            .map(|i| i.name.split('.').next().unwrap_or("").to_string())
            .collect();
        let base_at = |k: usize| -> String { names[k].clone() };
        // 内建调用判定（GeneratorGetResumeMode / CreateIterResultObject）也先算好
        let intrinsics: Vec<Option<String>> = (0..n)
            .map(|k| {
                if names[k] != "InvokeIntrinsic" {
                    return None;
                }
                self.instrs[k]
                    .operands
                    .first()
                    .map(|o| self.render_operand(o))
            })
            .collect();
        let is_intrinsic = |k: usize, want: &str| -> bool {
            intrinsics[k].as_deref().is_some_and(|t| t.contains(want))
        };
        // 6.2 的生成器是另一套形态：没有 SwitchOnGeneratorState —— 序言是
        // `Ldar rG; JumpIfUndefined <prologue>; …RestoreGeneratorState…; SwitchOnSmiNoFeedback
        //  {0:续体0, 1:续体1…}`，各挂起点的续体在**函数开头**分派，挂起点本体则是
        // `SuspendGenerator [k]; Return`。这里走一条独立的折叠路径（见 six_x_generator）。
        if std::env::var("JSCD_DBG_GEN").is_ok() {
            eprintln!("[gen] enter n={n} i0={:?} async={:?}", names.first(), self.is_async_fn);
        }
        // 8.1 的生成器序言前面还有一条 `StackCheck`（8.4 起没有）→ 不能死认第 0 条。
        // 在前几条里找 `SwitchOnGeneratorState`，把它的下标当序言起点（前缀按机器码跳过）。
        let Some(gen_start) = (0..n.min(8)).find(|&k| base_at(k) == "SwitchOnGeneratorState")
        else {
            if names.iter().take(24).any(|x| x == "RestoreGeneratorState") {
                self.six_x_generator(&names);
            }
            return;
        };
        let Some(first_sus) = (gen_start..n)
            .find(|&k| matches!(base_at(k).as_str(), "SuspendGenerator" | "__gawait"))
        else {
            return;
        };
        if std::env::var("JSCD_DBG_GEN").is_ok() {
            eprintln!(
                "[gen] n={n} first_sus={first_sus} names={:?}",
                &names[..n.min(24)]
            );
        }
        let async_fn = self.is_async_fn;
        let mut skip = vec![false; n];
        // async 的序言里有"被 await 的表达式"（`Mov a0, r4` 之类）→ 不能整体跳过；
        // plan_async 已把其中的状态机指令标成 `__gskip`，其余留给正常发射。
        for (k, s) in skip.iter_mut().enumerate().take(first_sus) {
            if k < gen_start || !async_fn || base_at(k) == "SwitchOnGeneratorState" {
                *s = true;
            }
        }
        // 序言里的参数下落：`Ldar aK; StaCurrentContextSlot [S]` → 槽 S 就是参数 K
        for k in 0..first_sus.saturating_sub(1) {
            if base_at(k) != "Ldar" {
                continue;
            }
            // 参数寄存器在内部是负下标，且基址随版本变（帧布局）→ 直接用操作数渲染
            let Some(op) = self.instrs[k].operands.first().cloned() else {
                continue;
            };
            if !matches!(op, Operand::Reg(_)) {
                continue;
            }
            let param = self.render_operand(&op);
            if !param.starts_with('a') || param == "a" {
                continue; // 不是参数寄存器
            }
            match base_at(k + 1).as_str() {
                "StaCurrentContextSlot" | "StaContextSlot" | "StaCurrentScriptContextSlot" => {
                    if let Some(Operand::Idx(slot)) = self.instrs[k + 1].operands.first() {
                        self.gen_param_slots.insert(*slot as usize, param);
                    }
                }
                _ => {}
            }
        }


        // ── ① yield* 的委托协议 ──
        self.detect_generator_delegates(&mut skip);

        // ── ② 普通挂起点（含序言的起始挂起）──
        for s in 0..n {
            if skip[s] || !matches!(base_at(s).as_str(), "SuspendGenerator" | "__gawait") {
                continue;
            }
            // async 没有"起始挂起"：第一个挂起点就是第一个 await → 不能当序言丢掉
            let prologue = !async_fn && s == first_sus;
            // 值层：紧邻的 CreateIterResultObject 包装（值 = 寄存器区间首），done 常量层一并丢
            let mut value: Option<String> = None;
            if !prologue && s >= 1 && is_intrinsic(s - 1, "CreateIterResultObject") {
                if let Some(op) = self.instrs[s - 1].operands.get(1) {
                    let t = self.render_operand(op);
                    value = Some(t.split('-').next().unwrap_or(&t).to_string());
                }
                skip[s - 1] = true;
                // done 常量层（LdaFalse/LdaTrue + Star）在值层之前：中间可能夹一条
                // 值计算（`Mov a1, r1` 这种），所以往前扫 4 条找这个 pattern
                for back in 2..=4usize.min(s) {
                    if s > back
                        && self.star_reg_text(s - back).is_some()
                        && matches!(base_at(s - back - 1).as_str(), "LdaFalse" | "LdaTrue")
                    {
                        skip[s - back] = true;
                        skip[s - back - 1] = true;
                        break;
                    }
                }
            }
            // 恢复值存储：ResumeGenerator 之后紧跟 Star → 表达式形（`r = yield v;`）
            let mut k = s + 1;
            if k < n && base_at(k) == "ResumeGenerator" {
                skip[k] = true;
                k += 1;
            }
            let expr_form = !prologue && self.star_reg_text(k).is_some();
            // 恢复分派：GeneratorGetResumeMode + 分派 switch + 各分支（到 case 0 续体为止）
            // 续体 = case 0 的目标（kNext）；case 1 = kReturn 要 return 恢复值，
            // 默认（kThrow）走 fallthrough 的 Ldar/Throw —— 都在 case 0 之前。
            let mut cont = k;
            if let Some(d) = (k..n.min(k + 12)).find(|&j| is_intrinsic(j, "GeneratorGetResumeMode")) {
                if let Some(sw) = (d..n.min(d + 4)).find(|&j| base_at(j) == "SwitchOnSmiNoFeedback") {
                    let cases = self.switch_cases(sw);
                    if let Some(t) = cases
                        .iter()
                        .find(|(v, _)| *v == 0)
                        .map(|(_, t)| *t)
                        .or_else(|| cases.iter().map(|(_, t)| *t).min())
                    {
                        if let Some(&ci) = self.idx_of.get(&t) {
                            cont = ci;
                        }
                    }
                } else if let Some(j) = (d..n.min(d + 8)).find(|&j| {
                    matches!(base_at(j).as_str(), "TestReferenceEqual" | "TestEqualStrict" | "LdaZero")
                }) {
                    // 13.x 的恢复分派写成比较链：`LdaZero; TestReferenceEqual rMode; JumpIfTrue <kNext>`
                    // （没有 SwitchOnSmiNoFeedback）→ 续体就是那条跳转的目标。
                    if let Some(jt) = (j..n.min(j + 4))
                        .find(|&x| matches!(base_at(x).as_str(), "JumpIfTrue" | "JumpIfFalse"))
                    {
                        if let Some(t) = self.cond_jump_target(&self.instrs[jt].clone()) {
                            if let Some(&ci) = self.idx_of.get(&t) {
                                cont = ci;
                            }
                        }
                    }
                }
            }
            if std::env::var("JSCD_DBG_GEN").is_ok() {
                eprintln!(
                    "[gen] suspend s={s} prologue={prologue} k={k} expr={expr_form} cont={cont} value={value:?}"
                );
            }
            let skip_from = if expr_form { k + 1 } else { k };
            for s in skip.iter_mut().take(cont.min(n)).skip(skip_from) {
                *s = true;
            }
            if async_fn && !prologue {
                // 保持 plan_async 定的 `__gawait` 与其"被 await 的值"，只把续体分派折叠掉
                if !self.instrs[s].name.starts_with("__gawait") {
                    self.instrs[s].name = "__gawait".into();
                }
            } else {
                self.instrs[s].name = if prologue {
                    "__gskip".into()
                } else if expr_form {
                    "__gyield".into()
                } else {
                    "__gyield_stmt".into()
                };
                self.gen_yields.insert(s, value);
            }
        }

        for (k, sk) in skip.iter().enumerate() {
            if *sk && !matches!(self.instrs[k].name.as_str(), "__gawait" | "__gyieldstar") {
                self.instrs[k].name = "__gskip".into();
            }
        }
    }

    /// `InvokeIntrinsic [_X]` 判定。
    fn intrinsic_is(&self, k: usize, want: &str) -> bool {
        let ins = match self.instrs.get(k) {
            Some(i) => i,
            None => return false,
        };
        if ins.name != "InvokeIntrinsic" {
            return false;
        }
        ins.operands
            .first()
            .map(|o| self.render_operand(o))
            .is_some_and(|t| t.contains(want))
    }

    /// yield* 委托协议的识别（8.x+ 锚 `GetIterator`，6.x 锚"取 @@iterator 后调用"）。
    /// 命中时把起点改名 `__gyieldstar` 并登记 (委托对象, 结果寄存器)，其余机器码标 skip。
    fn detect_generator_delegates(&mut self, skip: &mut [bool]) {
        let n = self.instrs.len();
        let names: Vec<String> = self
            .instrs
            .iter()
            .map(|i| i.name.split('.').next().unwrap_or("").to_string())
            .collect();
        let base_at = |k: usize| -> String { names[k].clone() };
    // ── ① yield* 的委托协议 ──
    for g in 0..n {
        if skip[g] {
            continue;
        }
        // 委托起点有两种形态：
        //   8.x+ ：`GetIterator rObj`（迭代器方法一步取出，随后 CallProperty0 调用）
        //   6.x  ：没有 GetIterator —— `LdaNamedProperty rObj, [@@iterator]; Star rM;
        //          CallProperty0 rM, rObj`。锚在 CallProperty0 上，委托对象就是它的**接收者**。
        let anchor_iter: Option<String> = match base_at(g).as_str() {
            "GetIterator" => self.instrs[g].operands.first().map(|o| self.render_operand(o)),
            "CallProperty0" => {
                // 名字必须以 `Symbol.iterator`（众所周知的符号根）出现；
                // 6.2/6.8 的表里符号名就是 JS 名，`constant()` 会给出这个标识符文本
                let iter_name: Option<String> = match self
                    .instrs
                    .get(g - 2)
                    .and_then(|ins| ins.operands.get(1))
                    .cloned()
                {
                    Some(Operand::Idx(i)) => Some(self.constant(i as usize).render()),
                    _ => None,
                };
                let prev_ok = g >= 2
                    && base_at(g - 1) == "Star"
                    && base_at(g - 2) == "LdaNamedProperty"
                    && iter_name
                        .as_deref()
                        .map(|n| n.contains("Symbol.iterator") || n.contains("@@iterator"))
                        .unwrap_or(false);
                if prev_ok {
                    self.instrs[g].operands.get(1).map(|o| self.render_operand(o))
                } else {
                    None
                }
            }
            _ => None,
        };
        let Some(anchor_iter) = anchor_iter else {
            continue;
        };
        let Some(sw) = (g..n.min(g + 48)).find(|&k| {
            base_at(k) == "SwitchOnSmiNoFeedback" && {
                let cs = self.switch_cases(k);
                cs.iter().any(|(v, _)| *v == 1) && cs.iter().any(|(v, _)| *v == 2)
            }
        }) else {
            if std::env::var("JSCD_DBG_GEN").is_ok() {
                let cands: Vec<(usize, Vec<(i64, usize)>)> = (g..n.min(g + 48))
                    .filter(|&k| base_at(k) == "SwitchOnSmiNoFeedback")
                    .map(|k| (k, self.switch_cases(k)))
                    .collect();
                eprintln!("[gen] delegate g={g}: 无三态分派，候选={cands:?}");
            }
            continue;
        };
        // 委托环的回边
        let Some(jl) = (sw..n.min(sw + 200)).find(|&k| base_at(k) == "JumpLoop") else {
            continue;
        };
        // 出口协议：`.value` → 委托结果寄存器
        let vload = (jl..n.min(jl + 16)).find(|&k| {
            matches!(base_at(k).as_str(), "LdaNamedProperty" | "GetNamedProperty")
                && self.prop_name_is(k, 1, "value")
        });
        let Some(vload) = vload else {
            if std::env::var("JSCD_DBG_GEN").is_ok() {
                let names2: Vec<String> =
                    (jl..n.min(jl + 16)).map(|k| names[k].clone()).collect();
                eprintln!("[gen] delegate g={g} sw={sw} jl={jl}: 出口无 .value；{names2:?}");
            }
            continue;
        };
        let Some(store) = (vload..n.min(vload + 4)).find_map(|k| self.star_reg_text(k)) else {
            continue;
        };
        // 模式测试后的续体（state == 1 那条直接 return 委托值）
        let Some(cont) = (vload..n.min(vload + 10))
            .find(|&k| matches!(base_at(k).as_str(), "JumpIfFalse" | "JumpIfTrue"))
            .and_then(|k| self.cond_jump_target(&self.instrs[k]))
            .and_then(|t| self.idx_of.get(&t).copied())
        else {
            continue;
        };
        self.instrs[g].name = "__gyieldstar".into();
        self.gen_delegates.insert(g, (anchor_iter.clone(), store.clone()));
        for s in skip.iter_mut().take(cont.min(n)).skip(g + 1) {
            *s = true;
        }
        if base_at(g) == "CallProperty0" {
            // 6.x：委托对象上那条取 @@iterator 的语句也一并丢掉（否则会被重复求值）
            if g >= 2 {
                skip[g - 1] = true;
                skip[g - 2] = true;
            }
        }
        if std::env::var("JSCD_DBG_GEN").is_ok() {
            eprintln!("[gen] delegate g={g} sw={sw} jl={jl} vload={vload} store={store} cont={cont}");
        }
    }


    }
    /// 续体 shim 里"恢复值落在哪个寄存器"：`GeneratorGetInputOrDebugPos` 之后紧跟的 Star。
    fn resume_value_reg(&self, from: usize, to: usize) -> Option<String> {
        let n = self.instrs.len();
        let hi = to.min(n);
        for j in from..hi {
            if !self.intrinsic_is(j, "GeneratorGetInputOrDebugPos") {
                continue;
            }
            for k in j + 1..(j + 4).min(hi) {
                if let Some(r) = self.star_reg_text(k) {
                    if r.starts_with('r') {
                        return Some(r);
                    }
                }
            }
        }
        None
    }

    /// 该指令是不是"某个内建/运行时的调用"，是的话给出名字。
    /// async 的机器码在两代里形态不同：12.x+ 是 `InvokeIntrinsic [_AsyncFunctionAwaitUncaught]`，
    /// 6.8 是 `CallJSRuntime [179]`（名字来自 runtime 表）。两条路都要认。
    fn call_name(&self, k: usize) -> Option<String> {
        let ins = self.instrs.get(k)?;
        let b = ins.name.split('.').next().unwrap_or(&ins.name);
        if !matches!(b, "InvokeIntrinsic" | "CallJSRuntime" | "CallRuntime" | "CallRuntimeForPair") {
            return None;
        }
        ins.operands
            .first()
            .map(|o| {
                self.render_operand(o)
                    .trim_matches(['[', ']'])
                    .trim_start_matches('_')
                    .to_string()
            })
    }

    /// 调用指令寄存器列表里的第 idx 个寄存器（async 的被 await 表达式在第二个）。
    fn reglist_nth(&self, k: usize, idx: usize) -> Option<String> {
        let ins = self.instrs.get(k)?;
        let ops: Vec<String> = ins
            .operands
            .iter()
            .map(|o| self.render_operand(o))
            .collect();
        let list = ops.iter().find(|s| s.contains('-') || s.starts_with('r'))?;
        let (head, tail) = match list.split_once('-') {
            Some((a, b)) => (a.to_string(), Some(b.to_string())),
            None => (list.clone(), None),
        };
        let (pfx, a) = split_reg(&head);
        let b = tail.as_deref().map(|t| split_reg(t).1).unwrap_or(a);
        let (a, b) = (a?, b?);
        let n = b.checked_sub(a)? + 1;
        if n <= 16 && idx < n as usize {
            Some(format!("{pfx}{}", a + idx as u32))
        } else {
            None
        }
    }

    /// async 函数的折叠：识别形态、折叠机器码、把挂起点改判成 `await`。
    ///
    /// V8 把 async function 编成和生成器同构的状态机：
    ///   `_AsyncFunctionEnter` 建状态对象 → 每个 await 点
    ///   `_AsyncFunctionAwait(state, 值)` + `SuspendGenerator` → `ResumeGenerator` + 模式分派
    ///   → `_AsyncFunctionResolve/Reject` 结算 Promise。
    /// 重建时直接发 `async function` + `await`（不另造驱动器）：挂起点发 `rK = await <值>`，
    /// 结算调用发 `return <值>` / `throw <错误>`。
    fn plan_async(&mut self) {
        let n = self.instrs.len();
        if n == 0 {
            return;
        }
        // 先记下所有跳转（源下标, 目标下标）：6.x 的 catch 体右端要靠"catch_start 之前
        // 的跳转落进 catch 体"定位 catch 之后共享的续接块（try 体正常完成时跳进去那段）。
        // 折叠会把跳转改名成 `__gskip`，那时名字就查不出跳转类型了，所以在这里先记。
        for k in 0..n {
            let ins = self.instrs[k].clone();
            if let Some(t) = self
                .uncond_jump_target(&ins)
                .or_else(|| self.cond_jump_target(&ins))
            {
                if let Some(&ti) = self.idx_of.get(&t) {
                    self.early_jumps.push((k, ti));
                }
            }
        }
        if std::env::var("JSCD_DBG_ASYNC").is_ok() {
            let calls: Vec<(usize, Option<String>)> = (0..n).map(|k| (k, self.call_name(k))).collect();
            eprintln!("[async?] calls={:?}", calls.iter().filter(|(_, c)| c.is_some()).collect::<Vec<_>>());
        }
        let mut enter: Option<usize> = None;
        let mut await_points: Vec<(usize, String)> = Vec::new(); // (await 调用下标, 被 await 的寄存器)
        let mut is_async_gen = false;
        for k in 0..n {
            let Some(name) = self.call_name(k) else { continue };
            match name.as_str() {
                "AsyncFunctionEnter" | "CreateAsyncFunctionObject" => enter = Some(k),
                "AsyncFunctionAwaitUncaught" | "AsyncFunctionAwaitCaught"
                | "AsyncFunctionAwait" | "AsyncGeneratorAwaitUncaught"
                | "AsyncGeneratorAwaitCaught" => {
                    if let Some(r) = self.reglist_nth(k, 1) {
                        await_points.push((k, r));
                    }
                }
                "AsyncGeneratorYieldWithAwait" | "AsyncGeneratorYield" | "AsyncGeneratorResolve"
                | "AsyncGeneratorReject" | "AsyncGeneratorReturn" | "AsyncGeneratorAwaitReturn" => is_async_gen = true,
                // 6.x 的 async 机器码走 CallRuntime（名字能解出来）：
                // async 函数体内一定会有 ResolvePromise/RejectPromise 的结算调用，
                // 而普通生成器不会有 —— 这是 6.x 唯一不依赖 native-context 槽号的判据。
                "ResolvePromise" | "RejectPromise" | "AsyncFunctionResolve" | "AsyncFunctionReject" => {
                    enter = enter.or(Some(k))
                }
                _ => {}
            }
        }
        // 6.2 的 async 机器码全是 native-context 槽号形式的 `CallJSRuntime`（名字解不出，
        // 渲染成数字）→ 名字判据失效。形态判据：`CreateJSGeneratorObject`（async 拿它当状态对象）
        // 之后紧跟着一条"数字名调用"（= AsyncFunctionEnter）。普通生成器那里是 StackCheck。
        if enter.is_none() && await_points.is_empty() && !is_async_gen {
            let cjso = (0..n).find(|&k| {
                matches!(self.call_name(k).as_deref(), Some("CreateJSGeneratorObject"))
            });
            let numeric_after = cjso.is_some_and(|p| {
                (p..n.min(p + 12)).any(|k| {
                    let b = self.instrs[k]
                        .name
                        .split('.')
                        .next()
                        .unwrap_or(&self.instrs[k].name)
                        .to_string();
                    matches!(b.as_str(), "CallJSRuntime" | "CallRuntime")
                        && self
                            .call_name(k)
                            .map(|x| !x.is_empty() && x.chars().all(|c| c.is_ascii_digit()))
                            .unwrap_or(false)
                })
            });
            if !numeric_after {
                // 没有 `CreateJSGeneratorObject` 的 6.2 async（函数里一个 await 都没有）：
                // 判据 = 尾部 `SwitchOnSmiNoFeedback` 的 case 0 块里有**数字名调用**
                // （= ResolvePromise 的 native-context 槽号形式；普通生成器的 case 块是续体，不会有）。
                let numeric_settle = (0..n)
                    .rev()
                    .find(|&k| {
                        matches!(
                            self.instrs[k].name.split('.').next().unwrap_or(""),
                            "SwitchOnSmiNoFeedback"
                        )
                    })
                    .map(|sw| {
                        self.switch_cases(sw).iter().any(|(v, t)| {
                            *v == 0
                                && self
                                    .idx_of
                                    .get(t)
                                    .map(|&ci| {
                                        (ci..n.min(ci + 12)).any(|j| {
                                            let b = self.instrs[j]
                                                .name
                                                .split('.')
                                                .next()
                                                .unwrap_or(&self.instrs[j].name)
                                                .to_string();
                                            b == "CallJSRuntime"
                                                && self
                                                    .call_name(j)
                                                    .map(|x| {
                                                        !x.is_empty()
                                                            && x.chars().all(|c| c.is_ascii_digit())
                                                    })
                                                    .unwrap_or(false)
                                        })
                                    })
                                    .unwrap_or(false)
                        })
                    })
                    .unwrap_or(false);
                if !numeric_settle {
                    // **6.2 的 async 函数体里一个 await 都没有**：没有 `CreateJSGeneratorObject`、
                    // 也没有尾部 Switch 分派 → 上面三条判据全落空，但序言里有 async 的
                    // 形态证据：`StackCheck` 之后紧跟一条**数字名** `CallJSRuntime`
                    // （= AsyncFunctionEnter 的 native-context 槽号形式）。
                    // 这种情况不需要折叠机器码（没有 await 可折），但**必须**把函数标记成
                    // async：JS 的 `async` 关键字自带 promise/异常→reject 机制，
                    // 否则整个函数被当普通函数输出、调用返回 undefined 而不是 promise。
                    if let Some(k) = (0..n.min(6)).find(|&k| {
                        matches!(
                            self.instrs[k].name.split('.').next().unwrap_or(""),
                            "CallJSRuntime" | "CallRuntime"
                        ) && self
                            .call_name(k)
                            .map(|x| !x.is_empty() && x.chars().all(|c| c.is_ascii_digit()))
                            .unwrap_or(false)
                    }) {
                        let _ = k;
                        self.is_async_fn = true;
                    }
                    return;
                }
            }
            // 只有"形态证据"（6.2：async 机器码全走 native-context 槽号，名字解不出）。
            // 6.2 的包装器与 6.8 同源（完成码 + 尾部 SwitchOnSmiNoFeedback 分派），
            // 折叠所需的两样都能按形态取：
            //   Enter = 函数里第一条数字名 `CallJSRuntime`（noAwait 是 `LdaUndefined; Star;
            //           CallJSRuntime [165]`；withAwait 在 `_CreateJSGeneratorObject` 之后）；
            //   await 值 = 挂起点之前最近一条调用的寄存器列表第 2 个（与 6.8 同）。
            // 分派由下面的"最后一个 SwitchOnSmiNoFeedback"兜底认（case 里是数字名结算）。
            if enter.is_none() {
                enter = (0..n).find(|&k| {
                    let b = self.instrs[k]
                        .name
                        .split('.')
                        .next()
                        .unwrap_or(&self.instrs[k].name)
                        .to_string();
                    b == "CallJSRuntime"
                        && self
                            .call_name(k)
                            .map(|x| !x.is_empty() && x.chars().all(|c| c.is_ascii_digit()))
                            .unwrap_or(false)
                });
            }
        }
        self.is_async_fn = true;
        self.is_async_gen = is_async_gen;
        self.async_enter = enter;
        // 序言右端在**改名之前**算好（Enter 之后那条 Star；6.2 的 async 用它划"序言 vs 函数体"）
        if let Some(e) = enter {
            self.async_prologue_end = Some(
                (e + 1..n.min(e + 3))
                    .find(|k| self.star_reg_text(*k).is_some())
                    .unwrap_or(e + 1),
            );
        }
        // 状态寄存器：`SwitchOnGeneratorState rState` 的操作数（8.x+），6.2 是
        // `RestoreGeneratorState rState`；都没有时用 Enter 之后那条 Star 的目标
        let state_reg: Option<String> = self
            .instrs
            .iter()
            .find(|i| i.name.starts_with("SwitchOnGeneratorState"))
            .and_then(|i| i.operands.first().cloned())
            .map(|o| self.render_operand(&o));
        let state_reg = state_reg
            .filter(|r| r.starts_with('r'))
            .or_else(|| {
                self.instrs
                    .iter()
                    .find(|i| i.name.starts_with("RestoreGeneratorState"))
                    .and_then(|i| i.operands.first().cloned())
                    .map(|o| self.render_operand(&o))
                    .filter(|r| r.starts_with('r'))
            })
            .or_else(|| {
                enter.and_then(|e| {
                    (e + 1..n.min(e + 3)).find_map(|k| self.star_reg_text(k))
                })
            });
        // 挂起点：把"下一个 SuspendGenerator"改判成 `await <值>`
        let mark_await = |me: &mut Self, sus: usize, value_reg: String| {
            me.gen_yields.insert(sus, Some(value_reg.clone()));
            me.instrs[sus].name = "__gawait".into();
            // 6.x 的恢复值来自续体 shim 的 `GetInputOrDebugPos → Star rK`（shim 会被折叠掉）
            // → 记下来，发射时直接写 `rK = await <值>;`
            if let Some(r) = me.resume_value_reg(sus + 1, me.instrs.len().min(sus + 24)) {
                me.gen_yield_store.insert(sus, r);
            }
            if std::env::var("JSCD_DBG_GEN").is_ok() {
                eprintln!("[async] await sus={sus} value={value_reg} gen={is_async_gen}");
            }
        };
        if std::env::var("JSCD_DBG_ASYNC").is_ok() {
            let aw: Vec<(usize, usize, String, Option<String>)> = self
                .instrs
                .iter()
                .enumerate()
                .filter(|(_, i)| i.name.contains("gawait") || i.name.starts_with("SuspendGenerator"))
                .map(|(k, i)| (k, i.offset, i.name.clone(), self.gen_yields.get(&k).cloned().flatten()))
                .collect();
            eprintln!("[async-dump] 挂起点={aw:?} stores={:?}", self.gen_yield_store);
        }
        for (k, value_reg) in await_points.clone() {
            let Some(sus) = (k..n.min(k + 12)).find(|j| {
                self.instrs[*j]
                    .name
                    .split('.')
                    .next()
                    .unwrap_or("")
                    .starts_with("SuspendGenerator")
            }) else {
                continue;
            };
            mark_await(self, sus, value_reg);
        }
        // 6.x：await 调用是 `CallJSRuntime [槽号]`（名字解不出）—— 只能按形态取：
        // 挂起点之前最近的一条调用，其寄存器列表第 2 个就是被 await 的值。
        for sus in 0..n {
            if !matches!(
                self.instrs[sus].name.split('.').next().unwrap_or(""),
                "SuspendGenerator" | "__gawait"
            ) {
                continue;
            }
            if self.instrs[sus].name == "__gawait" {
                continue;
            }
            let mut found = None;
            for k in (sus.saturating_sub(4)..sus).rev() {
                let b = self.instrs[k].name.split('.').next().unwrap_or("").to_string();
                if !matches!(b.as_str(), "CallJSRuntime" | "CallRuntime" | "InvokeIntrinsic") {
                    continue;
                }
                if let Some(r) = self.reglist_nth(k, 1) {
                    if r.starts_with('r') {
                        found = Some(r);
                    }
                }
                break;
            }
            if let Some(r) = found {
                mark_await(self, sus, r);
            }
        }

        // 折叠机器指令
        for k in 0..n {
            let b = self.instrs[k]
                .name
                .split('.')
                .next()
                .unwrap_or(&self.instrs[k].name)
                .to_string();
            let cname = self.call_name(k);
            let skip = match (b.as_str(), cname.as_deref()) {
                // 注意：`SwitchOnGeneratorState` **不改名** —— plan_generator 靠它认形态，
                // 由它在 async 分支里显式跳过。
                (_, Some("AsyncFunctionEnter")) | (_, Some("CreateAsyncFunctionObject")) => true,
                (_, Some("AsyncFunctionAwaitUncaught")) | (_, Some("AsyncFunctionAwaitCaught"))
                | (_, Some("AsyncFunctionAwait")) | (_, Some("AsyncGeneratorAwaitUncaught"))
                | (_, Some("AsyncGeneratorAwaitCaught")) | (_, Some("AsyncGeneratorYieldWithAwait"))
                | (_, Some("AsyncGeneratorResolve")) | (_, Some("AsyncGeneratorReject"))
                | (_, Some("GeneratorClose")) | (_, Some("CheckIsBootstrapping"))
                | (_, Some("CreateAsyncFromSyncIterator")) => true,
                ("LdaTheHole", _) | ("SetPendingMessage", _) => true,
                // 6.x 的 async 用 JSGeneratorObject 当状态对象，外加一串 native-context
                // 槽号形式的 CallJSRuntime（名字解不出、渲染成数字）—— 都是机器码
                (_, Some("CreateJSGeneratorObject")) if self.is_async_fn => true,
                ("CallJSRuntime", Some(x)) if self.is_async_fn && x.chars().all(|c| c.is_ascii_digit()) => true,
                ("Mov", _) => {
                    // `<closure>`/`<this>`/`<context>` 三个源是机器码；state → 调用参数的搬运也是
                    let src = self.instrs[k]
                        .operands
                        .first()
                        .map(|o| self.render_operand(o))
                        .unwrap_or_default();
                    src == "<closure>"
                        || src == "<this>"
                        || src == "<context>"
                        || state_reg.as_deref() == Some(src.as_str())
                }
                _ => false,
            };
            if skip {
                self.instrs[k].name = "__gskip".into();
            }
        }
        // Enter 之后那条 Star（状态落地）也是机器码
        if let Some(e) = enter {
            if let Some(k) = (e + 1..n.min(e + 3)).find(|k| self.star_reg_text(*k).is_some()) {
                self.instrs[k].name = "__gskip".into();
            }
        }
        // 6.2 的 `_CreateJSGeneratorObject` 之后那条 Star 同理（没有 Enter 判据时也要跳）
        if self.is_async_fn {
            for k in 0..n {
                if matches!(self.call_name(k).as_deref(), Some("CreateJSGeneratorObject")) {
                    if let Some(j) = (k + 1..n.min(k + 3)).find(|j| self.star_reg_text(*j).is_some()) {
                        self.instrs[j].name = "__gskip".into();
                    }
                }
            }
        }
        if std::env::var("JSCD_DBG_ASYNC").is_ok() {
            let aw: Vec<(usize, usize, String, Option<String>)> = self
                .instrs
                .iter()
                .enumerate()
                .filter(|(_, i)| i.name.contains("gawait"))
                .map(|(k, i)| (k, i.offset, i.name.clone(), self.gen_yields.get(&k).cloned().flatten()))
                .collect();
            eprintln!("[async-end] gawait={aw:?} stores={:?}", self.gen_yield_store);
        }
        // 6.x 的 async 外面还包一层"完成码 + 分派"包装器：
        //   主体 → `LdaZero; Star rC; Jump <dispatch>`      （正常出口）
        //   catch 处理器 → `RejectPromise(outer, err); …; Star rC(=1); Jump <dispatch>`
        //   <dispatch> → `SwitchOnSmiNoFeedback (rC) {0: ResolvePromise, 1: return, 2: ReThrow}`
        // 其中 catch 处理器与主体出口的 Jump 都是机器码：跳过它们、让控制流线性落入分派，
        // 分派里的 ResolvePromise/RejectPromise 由结算处理器变成 `return` / `throw`。
        {
            let base_names: Vec<String> = self
                .instrs
                .iter()
                .map(|x| x.name.split('.').next().unwrap_or(&x.name).to_string())
                .collect();
            let base_of = |k: usize| -> String {
                base_names.get(k).cloned().unwrap_or_default()
            };
            // 分派 = 尾部那个 case 含 ResolvePromise 的 switch
            let mut dispatch: Option<usize> = None;
            for k in 0..n {
                if base_of(k) != "SwitchOnSmiNoFeedback" {
                    continue;
                }
                let cases = self.switch_cases(k);
                let has_resolve = cases.iter().any(|(_, t)| {
                    self.idx_of
                        .get(t)
                        .map(|&ci| {
                            (ci..n.min(ci + 8)).any(|j| {
                                matches!(self.call_name(j).as_deref(),
                                    Some("ResolvePromise") | Some("AsyncFunctionResolve"))
                            })
                        })
                        .unwrap_or(false)
                });
                if has_resolve {
                    dispatch = Some(k);
                }
            }
            if std::env::var("JSCD_DBG_ASYNC").is_ok() {
                eprintln!(
                    "[async6] handlers={:?} disp={:?}",
                    self.handlers
                        .iter()
                        .map(|h| (h.start, h.end, h.target))
                        .collect::<Vec<_>>(),
                    dispatch.map(|d| self.instrs[d].offset)
                );
            }
            // 6.2 的结算调用是数字名（`CallJSRuntime [166]`）→ 上面的 ResolvePromise 判据失效，
            // 兜底取"函数里最后一个 SwitchOnSmiNoFeedback"（完成码分派总在尾部）。
            let dispatch = dispatch.or_else(|| {
                (0..n)
                    .rev()
                    .find(|&k| base_of(k) == "SwitchOnSmiNoFeedback")
                    .filter(|&k| k > n / 2)
            });
            if let Some(disp) = dispatch {
                self.async_dispatch = Some((disp, n));
                // 外层 handler 的 target 就是分派本身 —— 登记为"已消费"，
                // 否则 ① try/catch 规则会把分派整块当成 catch 体（正常路径变不可达）。
                // 外层（完成码）handler：target 落在"catch 处理器之后、分派之前"，
                // 且入口形态是完成码设置（`Star; LdaSmi/LdaZero; Star`）而不是用户 catch
                // （用户 catch 一定有 CreateCatchContext）。
                let disp_off = self.instrs.get(disp).map(|i| i.offset).unwrap_or(0);
                let _ = disp_off;
                let outer_starts: Vec<u32> = self
                    .handlers
                    .iter()
                    .filter(|h| {
                        let Some(&ti) = self.idx_of.get(&(h.target as usize)) else {
                            return false;
                        };
                        if ti >= disp {
                            return false;
                        }
                        let win: Vec<String> = (ti..self.instrs.len().min(ti + 4))
                            .map(|k| {
                                self.instrs[k]
                                    .name
                                    .split('.')
                                    .next()
                                    .unwrap_or(&self.instrs[k].name)
                                    .to_string()
                            })
                            .collect();
                        !win.iter().any(|x| x == "CreateCatchContext")
                            && win.iter().any(|x| x == "LdaSmi" || x == "LdaZero")
                            && win.iter().any(|x| x.starts_with("Star"))
                    })
                    .map(|h| h.start)
                    .collect();
                for st in outer_starts {
                    self.used_handler_starts.push(st as usize);
                }
                // catch 处理器：分派之前最后一个 CreateCatchContext 起，到分派为止
                if let Some(catch_idx) = (0..disp).rev().find(|&k| base_of(k) == "CreateCatchContext") {
                    // 这个 catch 体整段是机器码（reject 路径）→ 连同它的 handler 一起消费掉，
                    // 否则 ① 规则会把它当 catch 体、把分派裹进去（正常路径仍然不可达）。
                    // handler 的 target 是 catch 入口那条 `Star rEx`（就在 CreateCatchContext 前）
                    // → 允许 1~2 条指令的偏移
                    let inner_starts: Vec<u32> = self
                        .handlers
                        .iter()
                        .filter(|h| {
                            self.idx_of
                                .get(&(h.target as usize))
                                .map(|&ti| ti + 2 >= catch_idx && ti <= catch_idx)
                                .unwrap_or(false)
                        })
                        .map(|h| h.start)
                        .collect();
                    for st in inner_starts {
                        self.used_handler_starts.push(st as usize);
                    }
                    for k in catch_idx..disp {
                        self.instrs[k].name = "__gskip".into();
                    }
                    if std::env::var("JSCD_DBG_GEN").is_ok() {
                        eprintln!("[async6] 跳过 catch 处理器 {catch_idx}..{disp}");
                    }
                }
                // 主体出口那条 Jump（目标正好是分派）→ 跳过，控制流自然落入分派
                let disp_off = self.instrs.get(disp).map(|i| i.offset).unwrap_or(0);
                for k in 0..disp {
                    if !base_of(k).starts_with("Jump") {
                        continue;
                    }
                    let t = self
                        .uncond_jump_target(&self.instrs[k].clone())
                        .or_else(|| self.cond_jump_target(&self.instrs[k].clone()));
                    if t == Some(disp_off) {
                        self.instrs[k].name = "__gskip".into();
                    }
                }
                // 完成码出口就地折叠：每条出口（try 体正常完成 / 用户 catch 体完成 / …）
                // 都先写（完成码 rC, 值 rV）再 `Jump <分派>`，值由分派里的 ResolvePromise
                // 结算。线性发射只能渲染一份分派 —— try/catch 体里的 return 折不到，
                // 后面的死尾声块还会把 catch 的结果覆盖成 `return undefined`。
                // 所以把"完成码 = 0"的出口直接就地折成 `return <rV>;`，分派块整体丢弃
                // （隐式 reject 块此前已标 __gskip）。
                // 注意：此时不少指令的名字已被改成 `__gskip` —— 指令类型一律看**改名前的
                // 快照** base_names，操作数不受改名影响。
                let orig = |k: usize| -> String { base_names.get(k).cloned().unwrap_or_default() };
                let load_reg = |k: usize| -> Option<String> {
                    let b = orig(k);
                    let rest = b.strip_prefix("Ldar")?;
                    if rest.is_empty() {
                        self.instrs
                            .get(k)
                            .and_then(|i| i.operands.first())
                            .map(|o| self.render_operand(o))
                    } else if rest.chars().all(|c| c.is_ascii_digit()) {
                        Some(format!("r{rest}"))
                    } else {
                        None
                    }
                };
                let store_reg = |k: usize| -> Option<String> {
                    let b = orig(k);
                    let rest = b.strip_prefix("Star")?;
                    if rest.is_empty() {
                        self.instrs
                            .get(k)
                            .and_then(|i| i.operands.first())
                            .map(|o| self.render_operand(o))
                    } else if rest.chars().all(|c| c.is_ascii_digit()) {
                        Some(format!("r{rest}"))
                    } else {
                        None
                    }
                };
                let cases = self.switch_cases(disp);
                let mut val_reg: Option<String> = None;
                if let Some(&c1) = cases.iter().find(|(v, _)| *v == 1).map(|(_, t)| t) {
                    if let Some(&ci) = self.idx_of.get(&c1) {
                        for j in ci..n.min(ci + 3) {
                            if let Some(r) = load_reg(j) {
                                if r.starts_with('r') {
                                    val_reg = Some(r);
                                }
                                break;
                            }
                            if orig(j) == "Return" {
                                break;
                            }
                        }
                    }
                }
                // 完成码寄存器 = 分派 switch 之前最后一条 `Ldar rX`（分派把 rC 载进 acc 再 switch）
                let mut comp_reg: Option<String> = None;
                for j in (disp.saturating_sub(4)..disp).rev() {
                    if let Some(r) = load_reg(j) {
                        if r.starts_with('r') {
                            comp_reg = Some(r);
                        }
                        break;
                    }
                }
                if std::env::var("JSCD_DBG_ASYNC").is_ok() {
                    eprintln!(
                        "[async-r?] disp={disp} off={} cases={:?} val={val_reg:?} comp={comp_reg:?}",
                        self.instrs.get(disp).map(|i| i.offset).unwrap_or(0),
                        self.switch_cases(disp)
                    );
                }
                if let (Some(rv), Some(rc)) = (val_reg, comp_reg) {
                    // 分派入口：preamble 以 `LdaTheHole; SetPendingMessage` 开头，出口的 Jump
                    // 指向的是**那里**（不是 switch 本身）——两种都算出口。
                    let mut disp_start = disp;
                    for j in (0..disp).rev() {
                        if orig(j) == "LdaTheHole" && orig(j + 1).starts_with("SetPendingMessage") {
                            disp_start = j;
                            break;
                        }
                    }
                    let disp_start_off = self.instrs[disp_start].offset;
                    // 先把要折的出口挑出来（不可变借用阶段），再统一改名（可变借用）
                    let mut cand: Vec<usize> = Vec::new();
                    if std::env::var("JSCD_DBG_ASYNC").is_ok() {
                        let js: Vec<String> = (0..disp)
                            .filter(|&k| orig(k).starts_with("Jump"))
                            .map(|k| {
                                let t = self
                                    .uncond_jump_target(&self.instrs[k].clone())
                                    .or_else(|| self.cond_jump_target(&self.instrs[k].clone()));
                                format!("{}@{}->{:?}", orig(k), self.instrs[k].offset, t)
                            })
                            .collect();
                        eprintln!(
                            "[async-j] disp_off={disp_off} disp_start_off={disp_start_off} jumps={js:?}"
                        );
                    }
                    for k in 0..disp {
                        if !orig(k).starts_with("Jump") {
                            continue;
                        }
                        // 已被别的规则判成机器码的跳转不再折（分派块内部的完成码设置等）
                        if self.instrs[k].name == "__gskip" {
                            continue;
                        }
                        let t = self
                            .uncond_jump_target(&self.instrs[k].clone())
                            .or_else(|| self.cond_jump_target(&self.instrs[k].clone()));
                        let Some(t) = t else { continue };
                        // 出口指向分派（switch 本身，或它前面那段 preamble）
                        let near_disp = self
                            .idx_of
                            .get(&t)
                            .map(|&ti| ti <= disp && ti + 10 >= disp)
                            .unwrap_or(false);
                        if t != disp_off && !near_disp {
                            continue;
                        }
                        // 往前找完成码设置：`LdaZero/LdaSmi 0; Star rC`
                        for back in 1..=4usize.min(k) {
                            if store_reg(k - back).as_deref() == Some(rc.as_str()) {
                                let src = orig(k - back - 1);
                                let srcv = self.instrs[k - back - 1]
                                    .operands
                                    .first()
                                    .map(|o| self.render_operand(o).trim_matches(['[', ']']).to_string())
                                    .unwrap_or_default();
                                if src == "LdaZero" || (src == "LdaSmi" && srcv == "0") {
                                    cand.push(k);
                                }
                                break;
                            }
                        }
                    }
                    for k in &cand {
                        self.async_returns.insert(*k, rv.clone());
                        self.instrs[*k].name = "__greturn".into();
                    }
                    if !cand.is_empty() {
                        for k in disp..n {
                            self.instrs[k].name = "__gskip".into();
                        }
                    }
                    if std::env::var("JSCD_DBG_ASYNC").is_ok() {
                        eprintln!("[async-r] rC={rc} rV={rv} 完成码出口折了 {} 个", cand.len());
                    }
                }
            }
        }

        // 现代形态（12.x+：没有完成码分派 switch）：隐式 reject 处理器。
        // V8 给 async 函数体包一层"任何未捕获异常 → reject promise"的处理器，它的
        // target 块最后调 `AsyncFunctionReject/RejectPromise`。用户代码的 `throw`
        // 走的是 `Throw` 字节码（不会调这两个内建），所以"块里有这两个调用"就是机器码。
        // 重建后的 JS 里语义等价于异常自然外抛（async 函数抛异常 = promise reject）
        // → 消费这些 handler（① 不再包 try/catch），整块标 `__gskip` 丢掉。
        // 6.8 老形态（disp=Some）的分派 switch 由上面的折叠处理，不走这条路。
        if self.async_dispatch.is_none() {
            let mut consume: Vec<u32> = Vec::new();
            for k in 0..self.handlers.len() {
                let h = self.handlers[k];
                let Some(&ti) = self.idx_of.get(&(h.target as usize)) else {
                    continue;
                };
                let mut is_reject = false;
                let mut block_end = ti;
                for j in ti..n.min(ti + 24) {
                    let b = self.instrs[j]
                        .name
                        .split('.')
                        .next()
                        .unwrap_or(&self.instrs[j].name)
                        .to_string();
                    if matches!(
                        self.call_name(j).as_deref(),
                        Some("AsyncFunctionReject") | Some("RejectPromise")
                    ) {
                        is_reject = true;
                    }
                    block_end = j;
                    if matches!(b.as_str(), "Return" | "Throw" | "ReThrow") {
                        break;
                    }
                }
                if is_reject {
                    consume.push(h.start);
                    for j in ti..=block_end {
                        self.instrs[j].name = "__gskip".into();
                    }
                    if std::env::var("JSCD_DBG_ASYNC").is_ok() {
                        eprintln!("[async-rej] 消费隐式 reject handler ({},{},{}) 块={ti}..={block_end}", h.start, h.end, h.target);
                    }
                }
            }
            for st in consume {
                self.used_handler_starts.push(st as usize);
            }
        }
    }

    /// 6.2 生成器的折叠（形态见上面 `plan_generator` 的注释）。
    ///
    /// 要点：把序言/恢复分派/挂起后的 `Return` 全标成 `__gskip`，只留挂起点本身；
    /// 之后统一把"跳向已跳过指令"的跳转重定向到其后第一条活指令
    /// （6.2 的循环回边指向的是分派 shim，续体在 shim 之后）。
    fn six_x_generator(&mut self, names: &[String]) {
        let n = self.instrs.len();
        let base_at = |k: usize| -> String { names[k].clone() };
        let Some(first_sus) = (0..n).find(|&k| matches!(base_at(k).as_str(), "SuspendGenerator" | "__gawait")) else {
            return;
        };
        if std::env::var("JSCD_DBG_SKIP").is_ok() {
            eprintln!(
                "[skip?] is_async={} first_sus={first_sus} enter={:?}",
                self.is_async_fn, self.async_enter
            );
        }
        let mut skip = vec![false; n];
        let mut why: Vec<&str> = vec![""; n];
        // 序言 + 起始挂起 + 其后的 Return。
        // 生成器：`SuspendGenerator` 之前整段是序言（首挂起是"未启动"挂起，体在续体里）。
        // async：首挂起是**第一个 await**（在体内！）—— 序言只到"状态对象/Promise 建好"为止
        // （= Enter 那条数字名 `CallJSRuntime` 之后的 Star），再往后是函数体本体，不能跳
        // （否则 `await` 的值表达式整段消失，node8 的 withAwait 曾 await 未赋值的 r10 → NaN）。
        let prologue_end = if self.is_async_fn {
            self.async_prologue_end.unwrap_or(first_sus).min(first_sus)
        } else {
            first_sus
        };
        for k in 0..=prologue_end.min(n - 1) {
            skip[k] = true;
            why[k] = "prologue";
        }
        if first_sus + 1 < n && base_at(first_sus + 1) == "Return" {
            skip[first_sus + 1] = true;
            why[first_sus + 1] = "ret-after-sus";
        }
        // 序言的参数下落（`Ldar aK; StaCurrentContextSlot [S]`）
        for k in 0..first_sus.saturating_sub(1) {
            if base_at(k) != "Ldar" {
                continue;
            }
            let Some(op) = self.instrs[k].operands.first().cloned() else {
                continue;
            };
            if !matches!(op, Operand::Reg(_)) {
                continue;
            }
            let param = self.render_operand(&op);
            if !param.starts_with('a') || param == "a" {
                continue;
            }
            if matches!(
                base_at(k + 1).as_str(),
                "StaCurrentContextSlot" | "StaContextSlot" | "StaCurrentScriptContextSlot"
            ) {
                if let Some(Operand::Idx(slot)) = self.instrs[k + 1].operands.first() {
                    self.gen_param_slots.insert(*slot as usize, param);
                }
            }
        }
        // 顶部 `SwitchOnSmiNoFeedback` 的每个 case 都是一段"恢复分派 shim"：
        //   `RestoreGeneratorRegisters …; GeneratorGetInputOrDebugPos; …; SwitchOnSmiNoFeedback
        //    {0: <真正续体>}`。shim 整段跳过（真正的续体在它后面）。
        if let Some(top_sw) = (0..n.min(first_sus)).find(|&k| base_at(k) == "SwitchOnSmiNoFeedback") {
            let cases = self.switch_cases(top_sw);
            for (_, tgt) in cases {
                let Some(&ci) = self.idx_of.get(&tgt) else {
                    continue;
                };
                let mut body = ci;
                if let Some(d) =
                    (ci..n.min(ci + 12)).find(|&j| self.intrinsic_is(j, "GeneratorGetResumeMode"))
                {
                    if let Some(sw) = (d..n.min(d + 4)).find(|&j| base_at(j) == "SwitchOnSmiNoFeedback")
                    {
                        let cs = self.switch_cases(sw);
                        if let Some(t) = cs
                            .iter()
                            .find(|(v, _)| *v == 0)
                            .map(|(_, t)| *t)
                            .or_else(|| cs.iter().map(|(_, t)| *t).min())
                        {
                            if let Some(&bi) = self.idx_of.get(&t) {
                                body = bi;
                            }
                        }
                    } else if let Some(j) = (d..n.min(d + 8)).find(|&j| {
                        matches!(
                            base_at(j).as_str(),
                            "TestReferenceEqual" | "TestEqualStrictNoFeedback" | "LdaZero"
                        )
                    }) {
                        // 6.2/13.x 的比较链形态：`LdaZero; TestEqual…; JumpIfTrue <续体>`
                        if let Some(jt) = (j..n.min(j + 4))
                            .find(|&x| matches!(base_at(x).as_str(), "JumpIfTrue" | "JumpIfFalse"))
                        {
                            if let Some(t) = self.cond_jump_target(&self.instrs[jt].clone()) {
                                if let Some(&bi) = self.idx_of.get(&t) {
                                    body = bi;
                                }
                            }
                        }
                    }
                }
                for j in ci..body.min(n) {
                    skip[j] = true;
                    why[j] = "topcase-shim";
                }
                if std::env::var("JSCD_DBG_GEN").is_ok() {
                    let offs: Vec<(usize, usize)> = (ci.saturating_sub(3)..n.min(ci + 4))
                        .map(|k| (k, self.instrs[k].offset))
                        .collect();
                    eprintln!(
                        "[gen6] top case shim {ci}..{body} tgt={tgt} idx@tgt={ci} offs={offs:?} names={:?}",
                        (ci.saturating_sub(3)..n.min(ci + 4))
                            .map(|k| self.instrs[k].name.clone())
                            .collect::<Vec<_>>()
                    );
                }
            }
        }

        // 每个挂起点之前还有一段"我是不是该在这恢复"的测试 shim：
        //   `Ldar rS; SwitchOnSmiNoFeedback {k: <别的续体>}; LdaSmi [-2];
        //    TestEqualStrictNoFeedback rS; JumpIfTrue <真正继续处>; …Abort…`
        // 整段跳过（真正继续处 = JumpIfTrue 的目标）。
        for j in 0..n {
            if skip[j] || base_at(j) != "TestEqualStrictNoFeedback" {
                continue;
            }
            if base_at(j + 1) != "JumpIfTrue" {
                continue;
            }
            let Some(t) = self.cond_jump_target(&self.instrs[j + 1].clone()) else {
                continue;
            };
            let Some(&ti) = self.idx_of.get(&t) else {
                continue;
            };
            // 往前收拢 shim 起点
            let mut start = j;
            while start > 0
                && matches!(
                    base_at(start - 1).as_str(),
                    "Ldar" | "SwitchOnSmiNoFeedback" | "LdaSmi"
                )
            {
                start -= 1;
            }
            if ti <= start {
                continue;
            }
            for k in start..ti.min(n) {
                skip[k] = true;
                why[k] = "state-shim";
            }
            if std::env::var("JSCD_DBG_GEN").is_ok() {
                eprintln!("[gen6] state shim {start}..{ti}");
            }
        }

        // 每个 yield 挂起点：值层 + 恢复分派 shim
        for s in first_sus..n {
            if !matches!(base_at(s).as_str(), "SuspendGenerator" | "__gawait") {
                continue;
            }
            // async（非 async generator）的第一个挂起就是第一个 await，函数体在它之前 ——
            // 挂起之后的恢复垫片没人负责（top-case 逻辑只走到"状态测试块"就停了），
            // 这里一并跳过（否则 node8 的 loopAwait 会把 RestoreGeneratorRegisters /
            // GetInputOrDebugPos / GetResumeMode 当代码发射，恢复值被覆盖成 undefined → NaN）。
            if s == first_sus && !(self.is_async_fn && !self.is_async_gen) {
                continue;
            }
            // async 的挂起点已由 plan_async 判成 `__gawait` 并带上了"被 await 的值"：
            // 下面按 yield 的那套（值、改名、恢复值寄存器）不能再做（会把 await 抹掉、值也没了），
            // 但**挂起之后的恢复垫片照样要跳**（`Return` + RestoreGeneratorRegisters…到续体），
            // 否则 6.2 的 multi 会漏出 `return;` 和一段机器码。
            let is_gawait = self.instrs[s].name.starts_with("__gawait");
            let mut value: Option<String> = None;
            if !is_gawait && s >= 1 && self.intrinsic_is(s - 1, "CreateIterResultObject") {
                if let Some(op) = self.instrs[s - 1].operands.get(1) {
                    let t = self.render_operand(op);
                    value = Some(t.split('-').next().unwrap_or(&t).to_string());
                }
                skip[s - 1] = true;
                why[s - 1] = "iterresult";
                for back in 2..=4usize.min(s) {
                    if s > back
                        && self.star_reg_text(s - back).is_some()
                        && matches!(base_at(s - back - 1).as_str(), "LdaFalse" | "LdaTrue")
                    {
                        skip[s - back] = true;
                        skip[s - back - 1] = true;
                        why[s - back] = "done-layer";
                        why[s - back - 1] = "done-layer";
                        break;
                    }
                }
            }
            // 挂起之后：`Return` + 恢复分派 shim（到 case 0 续体前）
            let mut cont = s + 1;
            if let Some(d) = (s + 1..n.min(s + 12)).find(|&j| self.intrinsic_is(j, "GeneratorGetResumeMode"))
            {
                if let Some(sw) = (d..n.min(d + 4)).find(|&j| base_at(j) == "SwitchOnSmiNoFeedback") {
                    let cases = self.switch_cases(sw);
                    if let Some(t) = cases
                        .iter()
                        .find(|(v, _)| *v == 0)
                        .map(|(_, t)| *t)
                        .or_else(|| cases.iter().map(|(_, t)| *t).min())
                    {
                        if let Some(&ci) = self.idx_of.get(&t) {
                            cont = ci;
                        }
                    }
                } else if let Some(j) = (d..n.min(d + 8)).find(|&j| {
                    matches!(
                        base_at(j).as_str(),
                        "TestReferenceEqual" | "TestEqualStrictNoFeedback" | "LdaZero"
                    )
                }) {
                    // 6.2/13.x 的比较链形态：`LdaZero; TestEqual…; JumpIfTrue <续体>`
                    if let Some(jt) = (j..n.min(j + 4))
                        .find(|&x| matches!(base_at(x).as_str(), "JumpIfTrue" | "JumpIfFalse"))
                    {
                        if let Some(t) = self.cond_jump_target(&self.instrs[jt].clone()) {
                            if let Some(&ci) = self.idx_of.get(&t) {
                                cont = ci;
                            }
                        }
                    }
                }
            }
            for j in s + 1..cont.min(n) {
                skip[j] = true;
                why[j] = "resume-shim";
            }
            if is_gawait {
                continue;
            }
            // `r = yield v` 的恢复值：续体 shim 里 GetInputOrDebugPos → Star rK
            let resume_reg = self.resume_value_reg(s + 1, cont);
            for j in s + 1..cont.min(n) {
                skip[j] = true;
                why[j] = "resume-shim";
            }
            self.instrs[s].name = match (&value, &resume_reg) {
                // 值 + 恢复寄存器都有 → 表达式形态（6.2 的 Star 在 shim 里、会被跳过，
                // 所以恢复值的落地要由挂起点自己发射）
                (Some(_), Some(_)) => "__gyield".into(),
                (Some(_), None) => "__gyield_stmt".into(),
                _ => "__gskip".into(),
            };
            if let Some(r) = resume_reg {
                self.gen_yield_store.insert(s, r);
            }
            self.gen_yields.insert(s, value);
            if std::env::var("JSCD_DBG_GEN").is_ok() {
                eprintln!("[gen6] yield s={s} value={:?} cont={cont}", self.gen_yields.get(&s));
            }
        }
        // yield* 的委托协议（6.2 同样要认：锚在"取 @@iterator 后调用"那条 CallProperty0 上）
        self.detect_generator_delegates(&mut skip);

        for k in 0..n {
            if std::env::var("JSCD_DBG_SKIP").is_ok() && skip[k] {
                eprintln!(
                    "[skip] {k} @{} {} <- {}",
                    self.instrs[k].offset, self.instrs[k].name, why[k]
                );
            }
            if skip[k]
                && !matches!(self.instrs[k].name.as_str(), "__gawait" | "__gyieldstar")
            {
                self.instrs[k].name = "__gskip".into();
            }
        }
        // 跳转重定向：目标落在已跳过指令上 → 其后第一条活指令
        let live_next = |from: usize| -> Option<usize> { (from..n).find(|&k| !skip[k]) };
        for k in 0..n {
            if skip[k] {
                continue;
            }
            // 只动**跳转指令**，而且要用"解析后的绝对目标"判断/改写：
            // 操作数本身是**相对**字节量（Jump 正向前、JumpLoop 反向减），直接当偏移用
            // 会把池索引/相对量当绝对地址（早先这么干过，循环回边指到了 @86）。
            let base = ins_base(&self.instrs[k]);
            if !base.starts_with("Jump") {
                continue;
            }
            let mut ins = self.instrs[k].clone();
            let tgt = self
                .cond_jump_target(&ins)
                .or_else(|| self.uncond_jump_target(&ins));
            let mut changed = false;
            if let Some(t) = tgt {
                if let Some(j) = self.idx_of.get(&t).copied() {
                    if skip.get(j).copied().unwrap_or(false) {
                        if let Some(nj) = live_next(j) {
                            let abs = self.instrs[nj].offset as i64;
                            let prefix = if ins.scale > 1 { 1 } else { 0 };
                            let here = ins.offset as i64 + prefix as i64;
                            let rel = if base.starts_with("JumpLoop") { here - abs } else { abs - here };
                            if rel >= 0 {
                                if let Some(Operand::Idx(v)) = ins.operands.first_mut() {
                                    *v = rel as u32;
                                    changed = true;
                                }
                            }
                        }
                    }
                }
            }
            if changed {
                if std::env::var("JSCD_DBG_GEN").is_ok() {
                    eprintln!("[gen6] retarget @{} {:?}", self.instrs[k].offset, ins.operands);
                }
                self.instrs[k] = ins;
            }
        }
    }

    /// SwitchOnSmiNoFeedback 的跳转表 → (case 值, 目标字节偏移)。
    /// 第三个操作数是 case 基值（V8 把 `switch (acc - base)` 的表压平）：
    /// 生成器恢复分派那张表基值是 1（kReturn），0（kNext）走 fallthrough。
    fn switch_cases(&self, k: usize) -> Vec<(i64, usize)> {
        let ins = &self.instrs[k];
        let mut out = Vec::new();
        let Some(Operand::Idx(table)) = ins.operands.first() else {
            return out;
        };
        let Some(size) = ins.operands.get(1).and_then(|o| match o {
            Operand::Idx(v) => Some(*v as usize),
            Operand::Imm(v) => Some(*v as usize),
            _ => None,
        }) else {
            return out;
        };
        let case_base = ins
            .operands
            .get(2)
            .and_then(|o| match o {
                Operand::Imm(v) => Some(*v),
                Operand::Idx(v) => Some(*v as i64),
                _ => None,
            })
            .unwrap_or(0);
        for t in 0..size {
            if let Some(v) = self
                .pool
                .and_then(|p| self.d.cache.array_elem(p, *table as usize + t))
                .and_then(|e| e.as_smi())
            {
                let prefix = if ins.scale > 1 { 1 } else { 0 };
                out.push((case_base + t as i64, ins.offset + prefix + v as usize));
            }
        }
        out
    }

    /// 命名属性的键是否为某个字符串（键是常量池下标，`LdaNamedProperty r1, [11]`）。
    fn prop_name_is(&mut self, k: usize, n: usize, want: &str) -> bool {
        let Some(Operand::Idx(i)) = self.instrs[k].operands.get(n) else {
            return false;
        };
        let i = *i as usize;
        // 只读堆字符串要看 ro-map；constant 已经处理了这条路径
        matches!(self.constant(i), Expr::Str(s) if s == want)
    }

    /// Star/StarN 的目标寄存器文本（`r2` / `a0`）。
    fn star_reg_text(&self, k: usize) -> Option<String> {
        if k >= self.instrs.len() {
            return None;
        }
        let b = self.instrs[k].name.split('.').next().unwrap_or("");
        let rest = b.strip_prefix("Star")?;
        if rest.is_empty() {
            return self.instrs[k].operands.first().map(|o| self.render_operand(o));
        }
        if rest.chars().all(|c| c.is_ascii_digit()) {
            return Some(format!("r{rest}"));
        }
        None
    }


    /// 寄存器在产物里的显示名：**与读侧/反汇编文本同一套命名** ——
    /// 老族的参数槽在文本里是 `aN`，读侧走 operand 文本所以是 `a0`，而写侧曾直接
    /// 拼 `r{下标}` → 同一个槽读写名字不一致：`finally { x = x + 100 }`（x 是参数）
    /// 被写成 `r0 = a0 + 100`（更新落在死临时上，优化层顺手把整句删了）。
    fn reg_display(&self, r: u32) -> String {
        let dec = Decoder::new(self.d.table, self.d.layout, self.param_count() as i32);
        let n = dec.reg_name(r as i32);
        // `<this>` / `<context>` 之类不是合法左值 → 退回 rN
        if n.len() > 1 && (n.starts_with('r') || n.starts_with('a')) && n[1..].chars().all(|c| c.is_ascii_digit()) {
            n
        } else {
            format!("r{r}")
        }
    }

    /// 把表达式写进"任意名字"的目标（生成器委托的 `r = yield* x` 用）。
    fn store_named(&mut self, target: &str, e: Expr) {
        let rhs = Self::render_stmt(&e);
        self.line(&format!("{target} = {rhs};"));
        if let Some(r) = target.strip_prefix('r').and_then(|s| s.parse::<u32>().ok()) {
            if self.regs.len() <= r as usize {
                self.regs.resize(r as usize + 1, None);
            }
            self.regs[r as usize] = Some(Expr::Reg(r));
        }
    }

    /// 新建一个 phi 变量（就地声明，避免"声明列表漏项"导致赋值到未声明变量）。
    fn new_phi(&mut self) -> String {
        let phi = format!("phi{}", self.tmp_counter);
        self.tmp_counter += 1;
        // 只在函数头统一声明：phi 可能在嵌套块里创建、在外层消费，
        // 就地 `let` 会因块作用域越界（generator 的 phi2 就是这么 undefined 的）。
        if !self.phi_vars.contains(&phi) {
            self.phi_vars.push(phi.clone());
        }
        phi
    }

    /// 模板描述 Struct 的某个槽所指的对象（槽 1 = raw_strings、槽 2 = cooked_strings）。
    fn template_part(&self, o: ObjId, slot: usize) -> Option<ObjId> {
        match self.d.cache.slot_at(o, slot) {
            Some(crate::serializer::SlotValue::Ref(crate::serializer::Ref::Object(x))) => {
                Some(*x)
            }
            _ => None,
        }
    }

    /// 数组元素的表达式渲染（模板串/字面量共用）。
    fn array_element_expr(&mut self, o: ObjId, i: usize) -> Expr {
        if self.d.is_double_array(o) {
            return match self.d.double_elem(o, i) {
                Some(v) => Expr::Num(v),
                None => Expr::Undefined,
            };
        }
        match self.d.cache.array_elem(o, i) {
            Some(Elem::Smi(v)) => Expr::Num(v as f64),
            Some(Elem::Ref(Ref::Object(x))) => self.object_constant(x),
            Some(Elem::Ref(Ref::Root(r))) => self.root_value(r),
            Some(Elem::Ref(Ref::RoRef(c, off))) => {
                match self.d.ro_map.as_deref().and_then(|m| m.get(c, off)) {
                    Some(n) => Expr::Str(n.to_string()),
                    None => Expr::Str(format!("<ro{c}_{off}>")),
                }
            }
            _ => Expr::Hole,
        }
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

    /// for-in 重建。
    ///
    /// V8 的形状（7.8–13.x 一致）：
    ///   ToObject rObj ; ForInEnumerate rObj ; ForInPrepare rA-rC
    ///   LdaZero ; Star rIdx
    ///   L: ForInContinue rIdx, rC ; JumpIfFalse EXIT
    ///      ForInNext rObj, rIdx, rA-rB ; JumpIfUndefined STEP
    ///      Star <loop var> …（循环体）…
    ///   STEP: ForInStep rIdx ; Star rIdx ; JumpLoop L
    ///   EXIT:
    /// 6.x 的 `ToObject` 检查（对象解构、for-in 之前）：
    ///   `Ldar X; JumpIfUndefined @cold; Ldar X; JumpIfNotNull @after; <cold: …; Throw>`
    /// 两条跳转合起来是"X 为 null/undefined 就抛"。单跳转规则会把冷块发射两遍
    /// （第二遍成线性代码 → 后续正常流程整段不可达，node8 的解构直接用不了）。
    fn try_emit_null_guard(&mut self, i: usize, end: usize) -> Option<usize> {
        let b = |this: &Self, k: usize| -> String {
            this.instrs
                .get(k)
                .map(|x| x.name.split('.').next().unwrap_or(&x.name).to_string())
                .unwrap_or_default()
        };
        let j1 = b(self, i);
        if j1 != "JumpIfUndefined" && j1 != "JumpIfNull" {
            return None;
        }
        let cold_off = self.cond_jump_target(&self.instrs[i].clone())?;
        let cold = *self.idx_of.get(&cold_off)?;
        // 两种版式：
        //   6.x     ：`Ldar X; JumpIf<Null|Undefined> <cold>; Ldar X; JumpIfNot<Null|Undefined> <after>`
        //   7.5/7.6 ：`Ldar X; JumpIfNull <cold>; JumpIfNotUndefined <after>` ——
        //              acc 没被第一条跳转破坏，不再重复 `Ldar`。
        // 只认前者时后者会漏 → 冷块（`ThrowPatternAssignmentNonCoercible` 一类）被当线性代码
        // 发射、函数**必然抛错**（12.5/12.9 的对象解构 `{x, ...rest} = obj` 就是这样挂的）。
        let j2_idx = if b(self, i + 1) == "Ldar" { i + 2 } else { i + 1 };
        let j2 = b(self, j2_idx);
        if j2 != "JumpIfNotNull" && j2 != "JumpIfNotUndefined" {
            return None;
        }
        let after_off = self.cond_jump_target(&self.instrs[j2_idx].clone())?;
        let after = *self.idx_of.get(&after_off)?;
        let dbg = std::env::var("JSCD_DBG_GUARD").is_ok();
        if after <= cold || cold <= j2_idx || after > end {
            if dbg {
                eprintln!("[guard] i={i} j1={j1} j2={j2} cold={cold} after={after} end={end} → 区间不符");
            }
            return None;
        }
        // 冷块要么以 Return/Throw 收尾，要么**整个就是一条会抛的 `CallRuntime`**
        // （7.5/7.6：`CallRuntime [ThrowPatternAssignmentNonCoercible]` 之后直接就是 after，
        // 运行时不返回 → 没有显式的 Throw 指令）。
        let cold_ok = self.block_terminates(after - 1) || {
            let only_call = cold + 1 == after
                && matches!(
                    self.instrs[cold].name.split('.').next().unwrap_or(""),
                    "CallRuntime" | "CallJSRuntime"
                );
            only_call
                && self
                    .call_name(cold)
                    .map(|n| n.starts_with("Throw") || n.contains("NonCoercible"))
                    .unwrap_or(false)
        };
        if !cold_ok || !self.self_contained(cold, after) {
            if dbg {
                eprintln!(
                    "[guard] i={i} j1={j1} cold={cold} after={after} → cold_ok={cold_ok} self_contained={}",
                    self.self_contained(cold, after)
                );
            }
            return None;
        }
        let acc = self.acc.clone().unwrap_or(Expr::Hole);
        self.line(&format!("if ({} == null) {{", acc.render()));
        self.indent += 1;
        let r = self.emit_range(cold, after);
        self.indent -= 1;
        self.line("}");
        r.ok()?;
        self.skip_spans.push((cold, after.saturating_sub(1)));
        self.acc = Some(acc);
        Some(after)
    }

    /// 对应 JS 的 `for (<lval> in <obj>) { body }` —— 直接发射，循环控制指令整段丢掉。
    fn try_emit_for_in(&mut self, i: usize, end: usize) -> Option<usize> {
        let base_of = |k: usize| -> String {
            self.instrs
                .get(k)
                .map(|x| x.name.split('.').next().unwrap_or(&x.name).to_string())
                .unwrap_or_default()
        };
        // 两代形态：
        //   6.8+ ：`ForInEnumerate rX; ForInPrepare …`（枚举器在 i，对象寄存器在 i）
        //   6.2  ：`ToObject rX; ForInPrepare rX, rA-rC`（**没有** ForInEnumerate，
        //          对象寄存器在 ForInPrepare 的第一个操作数上）
        let is_enumerate = base_of(i) == "ForInEnumerate" && base_of(i + 1) == "ForInPrepare";
        let is_to_object = base_of(i) == "ToObject" && base_of(i + 1) == "ForInPrepare";
        if !is_enumerate && !is_to_object {
            return None;
        }
        let src = if is_enumerate { i } else { i + 1 };
        let obj_reg = match self.instrs[src].operands.first() {
            Some(Operand::Reg(r)) if *r >= 0 => *r as u32,
            _ => return None,
        };
        // 循环判定有两代形态：
        //   ≤12.x：`ForInContinue idx, len` + `JumpIfFalse EXIT`
        //   13.x ：合并成 `JumpIfForInDone idx, len`（自带跳转）
        let mut j = i + 2;
        let mut test_idx = None;
        while j < end && j <= i + 8 {
            match base_of(j).as_str() {
                "ForInContinue" if base_of(j + 1) == "JumpIfFalse" => {
                    test_idx = Some(j + 1);
                    break;
                }
                "JumpIfForInDone" => {
                    test_idx = Some(j);
                    break;
                }
                _ => j += 1,
            }
        }
        let test_idx = test_idx?;
        if std::env::var("JSCD_DBG_FORIN").is_ok() {
            let t = &self.instrs[test_idx];
            eprintln!(
                "[forin] i={i} j={j} test={test_idx} name={} off={} scale={} ops={:?} -> {:?}",
                t.name,
                t.offset,
                t.scale,
                t.operands,
                self.cond_jump_target(t)
            );
        }
        let exit_off = self.cond_jump_target(&self.instrs[test_idx].clone())?;
        let exit_idx = *self.idx_of.get(&exit_off)?;
        // 回边：跳到循环判定处的无条件跳转
        let head_off = self.instrs[j].offset;
        if std::env::var("JSCD_DBG_FORIN").is_ok() {
            for k in j..exit_idx.min(self.instrs.len()) {
                let b = self.instrs[k].name.split('.').next().unwrap_or("").to_string();
                eprintln!(
                    "[forin]  k={k} off={} {b} uncond={:?} cond={:?}",
                    self.instrs[k].offset,
                    self.uncond_jump_target(&self.instrs[k].clone()),
                    self.cond_jump_target(&self.instrs[k].clone())
                );
            }
        }
        // 体区起点：判定指令之后（13.x 的判定在 ForInNext 之前，用 j+2 会把它跳过去）
        let body_lo = test_idx + 1;
        let back_idx = (body_lo..exit_idx).find(|k| {
            self.uncond_jump_target(&self.instrs[*k].clone())
                .map(|t| t == head_off)
                .unwrap_or(false)
        })?;
        // 循环体：[ForInNext 的 undefined 守卫之后, ForInStep)
        // ForInStep 紧贴回边之前（`ForInStep; Star idx; JumpLoop`）→ 从后往前找最稳
        let step_idx = (body_lo..back_idx)
            .rev()
            .find(|k| base_of(*k) == "ForInStep")?;
        let next_idx = (body_lo..step_idx).find(|k| base_of(*k) == "ForInNext")?;
        let mut body_start = next_idx + 1;
        // `JumpIfUndefined STEP`：键被删掉时跳过本轮
        let mut guard: Option<String> = None;
        if base_of(body_start) == "JumpIfUndefined" {
            if let Some(Operand::Idx(v)) = self.instrs[body_start].operands.first() {
                let t = self.instrs[body_start].offset + 1 + *v as usize;
                if self.idx_of.get(&t) == Some(&step_idx) {
                    guard = Some("true".to_string()); // 变量名稍后按真实寄存器填
                }
            }
            body_start += 1;
        }
        // 循环变量：ForInNext 的值先 Star 进某个寄存器
        let var_reg = self.loop_var_reg(body_start)?;
        // 该 Star 本身不必输出（`for (r5 in …)` 已经赋值）；后续对同值的 Star 走 acc 模型
        let body_real = body_start + 1;
        self.acc = Some(Expr::Reg(var_reg));
        if std::env::var("JSCD_DBG_FORIN").is_ok() {
            for k in j..(back_idx + 1).min(self.instrs.len()) {
                let b = self.instrs[k].name.split('.').next().unwrap_or("").to_string();
                eprintln!("[forin] idx={k} off={} {b}", self.instrs[k].offset);
            }
            eprintln!("[forin] body_real={body_real} step_idx={step_idx} back_idx={back_idx} exit={exit_idx}");
        }
        let var = format!("r{var_reg}");
        let guard = guard.map(|_| format!("{var} !== undefined"));
        self.line(&format!("for ({var} in r{obj_reg}) {{"));
        self.indent += 1;
        if let Some(g) = guard {
            self.line(&format!("if ({g}) {{"));
            self.indent += 1;
            if let Err(e) = self.emit_range(body_real, step_idx) {
                self.line(&format!("/* 结构化失败: {e} */"));
            }
            // 体内最后一条带副作用的表达式（如 `keys.push(k)`）落在 acc 里、没人消费 →
            // 在这里落地成语句，否则会被外层的合并点物化到循环外面去
            self.materialize_acc_stmt();
            self.indent -= 1;
            self.line("}");
        } else {
            if let Err(e) = self.emit_range(body_real, step_idx) {
                self.line(&format!("/* 结构化失败: {e} */"));
            }
            self.materialize_acc_stmt();
        }
        self.indent -= 1;
        self.line("}");
        Some(exit_idx.max(i + 1))
    }

    /// `ForInNext` 之后承载键的那个寄存器（Star 序列里的第一个）。
    fn loop_var_reg(&self, k: usize) -> Option<u32> {
        for j in k..(k + 3).min(self.instrs.len()) {
            let ins = &self.instrs[j];
            let b = ins.name.split('.').next().unwrap_or(&ins.name);
            if b == "Star" {
                if let Some(Operand::Reg(r)) = ins.operands.first() {
                    if *r >= 0 {
                        return Some(*r as u32);
                    }
                }
            }
            if b.starts_with("Star") && b[4..].chars().all(|c| c.is_ascii_digit()) {
                return b[4..].parse().ok();
            }
        }
        None
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
        // 体以 return/throw 收尾时没有"跳向 join"的 Jump —— 此时 join 就是范围末尾
        // （`case 0: return …` 这种 V8 把每个 case 体做成独立返回块）。
        let join = join.unwrap_or(end);
        for &t in &targets {
            if t >= join {
                return None;
            }
        }
        if join == 0 {
            return None;
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

    /// try/catch/finally 重建。
    ///
    /// handler 表在 V8 里是成对的（本机实测）：
    ///   外侧条目 start=3  end=73 target=73   → try→finally（target == end）
    ///   内侧条目 start=6  end=41 target=41   → try→catch  （target == end，catch 体从 target 起）
    /// try 体尾部是 `Star v; LdaSmi 1; Star0; Jump <finally>`（完成码 1 = return），
    /// catch 体同样；finally 之后是 `switch (完成码)` 分发（0 → 重抛）。这些在 JS 里
    /// 正好对应原生 `try {} catch (e) {} finally {}`，于是完成码与分发整段丢掉。
    /// 无 catch 的 `try { … } finally { … }`（只有一个 try→finally handler）。
    ///
    /// V8 形态（16.20 / 26.10 实测）：
    /// ```text
    ///   <try 体> StarV; LdaSmi [code]; StarC; Jump FIN     ; code 1 = return、-1 = 正常完成
    ///   HANDLER: StarV; LdaZero; StarC                     ; 异常入口：异常放 V、完成码 0
    ///   FIN: LdaTheHole; SetPendingMessage; Star…; <finally 体>; Ldar …; SetPendingMessage
    ///   DISP: Ldar C; SwitchOnSmiNoFeedback {0: Ldar V; ReThrow, 1: Ldar V; Return}
    /// ```
    /// 重建成 `try { <try 体>; return rV; } finally { <finally 体> }` —— throw 路径本来就是
    /// finally 的天然语义，完成码分发整段丢掉。
    fn try_emit_try_finally_no_catch(&mut self, i: usize, end: usize, ho: Handler) -> Option<usize> {
        let fail = |why: &str| {
            if std::env::var("JSCD_DBG_TF").is_ok() {
                eprintln!("[tf-solo] ✗ {why}");
            }
        };
        let try_start = *self.idx_of.get(&(ho.start as usize))?;
        let try_end = *self.idx_of.get(&(ho.end as usize))?;
        let handler_start = *self.idx_of.get(&(ho.target as usize))?;
        if try_end <= try_start || handler_start < try_end || handler_start + 3 > end {
            fail(&format!("range try={try_start}..{try_end} handler={handler_start} end={end}"));
            return None;
        }
        let base = |this: &Self, k: usize| -> String {
            this.instrs
                .get(k)
                .map(|x| x.name.split('.').next().unwrap_or(&x.name).to_string())
                .unwrap_or_default()
        };
        // try 体尾部的完成码设置有两种布局（都实测过）：
        //   return 路径：`StarV; LdaSmi 1; StarC`   ← 返回值放 V、完成码 1
        //   正常路径：  `LdaSmi -1; StarV; StarC`   ← 完成码 -1（V 里也是 -1，无意义）
        let smi_of = |this: &Self, k: usize| -> Option<i64> {
            let ins = this.instrs.get(k)?;
            let b = ins.name.split('.').next().unwrap_or(&ins.name);
            if b != "LdaSmi" && b != "LdaZero" {
                return None;
            }
            if b == "LdaZero" {
                return Some(0);
            }
            ins.operands.first().and_then(|o| match o {
                Operand::Imm(v) => Some(*v),
                Operand::Idx(v) => Some(*v as i64),
                _ => None,
            })
        };
        // 完成码设置可能落在 handler.end **之后**（V8 9.4 实测：end 只到 try 体末尾）——
        // 从 handler 入口往前扫、取最近的一处匹配（设置永远紧挨着 handler）。
        let mut setup = None;
        let mut k = handler_start.saturating_sub(1);
        while k >= try_start {
            if let Some(code) = smi_of(self, k) {
                // return 路径：上一条是 Star（返回值），再下一条（k+1）是 StarC
                if k > try_start
                    && base(self, k - 1).starts_with("Star")
                    && base(self, k + 1).starts_with("Star")
                    && code >= 0
                {
                    // `Star0` 是短形式：寄存器编码在 opcode 名里、没有操作数 → 用 star_reg_text
                    let creg = self.star_reg_text(k + 1).unwrap_or_default();
                    setup = Some((k, code, true, k - 1, creg));
                    break;
                }
                // 正常路径：`LdaSmi -1; StarV; StarC`
                if code < 0
                    && base(self, k + 1).starts_with("Star")
                    && base(self, k + 2).starts_with("Star")
                {
                    let creg = self.star_reg_text(k + 2).unwrap_or_default();
                    setup = Some((k, code, false, k, creg));
                    break;
                }
            }
            if k == 0 {
                break;
            }
            k -= 1;
        }
        let Some((_, code, returns, body_end, code_reg)) = setup else {
            fail("找不到完成码设置");
            return None;
        };
        let try_val = if returns {
            self.completion_return_reg(body_end + 2)?
        } else {
            0
        };
        // handler 入口必须是异常路径：`StarV; LdaZero; StarC`（值寄存器同一个，code=0）
        if !base(self, handler_start).starts_with("Star")
            || base(self, handler_start + 1) != "LdaZero"
            || !base(self, handler_start + 2).starts_with("Star")
        {
            fail(&format!(
                "handler 入口形态不对: {} {} {}",
                base(self, handler_start),
                base(self, handler_start + 1),
                base(self, handler_start + 2)
            ));
            return None;
        }
        let finally_start = handler_start + 3;
        // 完成码分发两种形态：
        //   switch（≤20.x 实测）：`Ldar P; SetPendingMessage; Ldar C; SwitchOnSmiNoFeedback {…}`
        //     恢复挂起消息的配对在分发**之前**。
        //   比较链（26.x 实测）：`LdaZero; TestReferenceEqual C; JumpIfFalse <续写>`，
        //     负分支里是 `Ldar P; SetPendingMessage; Ldar E; ReThrow` —— 恢复对在分支
        //     **内部**，不再能当“finally 体收尾”用。
        let mut disp = 0usize;
        let mut disp_end = 0usize;
        let mut fin_end = 0usize;
        let mut found = false;
        for k in finally_start..end.min(self.instrs.len()) {
            if base(self, k) == "SwitchOnSmiNoFeedback" {
                let start = k.saturating_sub(1);
                if !code_reg.is_empty() {
                    let ld = self.instrs[start]
                        .operands
                        .first()
                        .map(|o| self.render_operand(o))
                        .unwrap_or_default();
                    if base(self, start) != "Ldar" || ld != code_reg {
                        break;
                    }
                }
                let mut fe = start;
                if fe >= 2 && base(self, fe - 1) == "SetPendingMessage" && base(self, fe - 2) == "Ldar" {
                    fe -= 2;
                }
                disp = start;
                disp_end = self.completion_dispatch_end(start, end).unwrap_or(end);
                fin_end = fe;
                found = true;
                break;
            }
            let cmp_name = base(self, k);
            if cmp_name == "TestReferenceEqual" || cmp_name == "TestEqualStrictNoFeedback" {
                let op = self.instrs[k]
                    .operands
                    .first()
                    .map(|o| self.render_operand(o))
                    .unwrap_or_default();
                if !code_reg.is_empty() && op != code_reg {
                    continue;
                }
                // 同样"比较完成码"的形态也出现在收尾协议自己的 handler 里
                // （`LdaZero; TestReferenceEqual rC; JumpIfTrue …` 那条是 handler 内部的
                // 守卫，分支里没有恢复挂起消息）→ 那种要**跳过继续找**，不能当成分派、
                // 更不能直接放弃（for-of 的 IteratorClose 收尾就是这么把异常吞掉的）。
                //
                // 分派有两种版式（都实测过）：
                //   9.4–22.x：`Ldar <saved>; SetPendingMessage; LdaZero; TestReferenceEqual rC;
                //             JumpIfFalse <cont>; Ldar <exc>; ReThrow`
                //   V8 14   ：`LdaZero; TestReferenceEqual rC; JumpIfFalse <cont>;
                //             Ldar <saved>; SetPendingMessage; Ldar <exc>; ReThrow`
                // 判据因此取"前置有 SetPendingMessage **或** 分支里有 SetPendingMessage"。
                let has_spm_before =
                    (k.saturating_sub(3)..k).any(|j| base(self, j) == "SetPendingMessage");
                let nxt = base(self, k + 1);
                if nxt != "JumpIfFalse" && nxt != "JumpIfTrue" {
                    continue;
                }
                let Some(t) = self.cond_jump_target(&self.instrs[k + 1].clone()) else {
                    continue;
                };
                let Some(ti) = self.idx_of.get(&t).copied() else {
                    continue;
                };
                let has_spm_in_branch = ti > k + 1
                    && (k + 1..ti).any(|j| base(self, j) == "SetPendingMessage");
                if !has_spm_before && !has_spm_in_branch {
                    continue;
                }
                fin_end = k.saturating_sub(1);
                disp = fin_end;
                disp_end = ti;
                found = true;
                break;
            }
        }
        if !found {
            fail(&format!(
                "找不到完成码分发 (fin_end={fin_end} code_reg={code_reg})"
            ));
            return None;
        }
        if disp_end <= fin_end {
            fail("分发区间不合法");
            return None;
        }
        if fin_end <= finally_start {
            fail("finally 体为空");
            return None;
        }
        // 用户级 try/finally 的完成码分发是**纯控制流**（Lda*/Test*/Jump*/Ldar/ReThrow/Return）。
        // for-of 的 IteratorClose 协议在新版 V8 上也是"单 handler"，但它的分发里有
        // `return()` 调用与属性读 —— 那些不该被这条路径接管（23.11+ 的
        // destructure/for_of_in 曾因此挂掉）。23.11+ 起恢复挂起消息的 `Ldar P;
        // SetPendingMessage` 会落在 case/ReThrow 分支**内部**（switch 与比较链都是），
        // 所以 SetPendingMessage 必须放行 —— 协议分发仍会因 CallProperty 一类被挡下。
        for k in disp..disp_end.min(self.instrs.len()) {
            let b = base(self, k);
            let control_only = b.starts_with("Lda")
                || b.starts_with("Test")
                || b.starts_with("Jump")
                || b.starts_with("Star")
                || b.starts_with("Switch")
                || b == "Ldar"
                || b == "ReThrow"
                || b == "Return"
                || b == "Throw"
                || b == "SetPendingMessage";
            if !control_only {
                fail(&format!("分发区间里出现非控制流指令 {b} @{k}"));
                return None;
            }
        }
        if std::env::var("JSCD_DBG_TF").is_ok() {
            eprintln!(
                "[tf-solo] try=[{try_start},{try_end}) val=r{try_val} code={code} fin=[{finally_start},{disp}) disp_end={disp_end}"
            );
        }
        self.used_handler_starts.push(ho.start as usize);
        self.line("try {");
        self.indent += 1;
        if let Err(e) = self.emit_range(try_start, body_end + 1) {
            self.line(&format!("/* 结构化失败: {e} */"));
        }
        if returns {
            self.line(&format!("return r{try_val};"));
        }
        self.indent -= 1;
        self.line("} finally {");
        self.indent += 1;
        if let Err(e) = self.emit_range(finally_start, fin_end) {
            self.line(&format!("/* 结构化失败: {e} */"));
        }
        // finally 体尾部若还挂着"待求值"的 acc（最后一句是个调用），先落成语句 ——
        // 否则 flush 会把它发到 try/finally **之后**（位置错、顺序也错）。
        let next_base = self
            .instrs
            .get(disp_end)
            .map(|x| x.name.split('.').next().unwrap_or(&x.name).to_string())
            .unwrap_or_default();
        self.flush_acc_before(&next_base);
        self.indent -= 1;
        self.line("}");
        let _ = disp;
        Some(disp_end.max(i + 1))
    }

    fn try_emit_try_finally(&mut self, i: usize, end: usize) -> Option<usize> {
        // 外侧 = try→finally 条目（target == end），且当前 walk 恰好在它起点
        let ho = self
            .handlers
            .iter()
            .find(|h| {
                self.tf_handler_entry(h)
                    && (h.end as usize) < usize::MAX
                    && self.idx_of.get(&(h.start as usize)) == Some(&i)
            })
            .copied()?;
        // 内侧 = try→catch 条目（起点落在外侧范围内）。**没有**内侧条目时是
        // 无 catch 的 `try { … } finally { … }` —— 它有自己的一套完成码形态，
        // 交给 `try_emit_try_finally_no_catch`（否则会把 handler 块当成 catch 体，
        // finally 主体被塞进凭空的 `catch (e)`、完成码 switch 泄漏、`return` 丢失）。
        let hi_opt = self.handlers.iter().find(|h| {
            self.tf_handler_entry(h)
                && h.start >= ho.start
                && h.end <= ho.end
                && h.start != ho.start
        }).copied();
        let Some(hi) = hi_opt else {
            let r = self.try_emit_try_finally_no_catch(i, end, ho);
            if std::env::var("JSCD_DBG_TF").is_ok() {
                eprintln!("[tf] 无 catch 形态 i={i} ho=({},{},{}) -> {r:?}", ho.start, ho.end, ho.target);
            }
            return r;
        };
        let try_start = *self.idx_of.get(&(hi.start as usize))?;
        let try_end = *self.idx_of.get(&(hi.end as usize))?;
        let catch_start = *self.idx_of.get(&(hi.target as usize))?;
        let finally_hint = *self.idx_of.get(&(ho.target as usize))?;
        if try_end <= try_start || catch_start < try_end || finally_hint < catch_start {
            return None;
        }
        // 完成码设置（`Star v; LdaSmi 1; Star0`）把 try / catch 体切开
        let dbg = std::env::var("JSCD_DBG_TF").is_ok();
        if dbg {
            eprintln!("[tf] ho=({},{},{}) hi=({},{},{}) try=[{try_start},{try_end}) catch={catch_start} fin={finally_hint}",
                ho.start, ho.end, ho.target, hi.start, hi.end, hi.target);
        }
        // 这三处失败都在**发射之前** → 回退试"无 catch 的 try/finally"形态。
        // 6.x 的 handler 表会把这组配对成"外层 handler + 内层 handler"，但内层不一定是
        // catch（for-of 的收尾协议就是内层那个 try/catch）；直接 return None 会把外层交给
        // ① 规则当普通 catch 处理 → 收尾协议的重抛守卫失效、异常被吞（iter_close 的 6.x 半）。
        let Some(try_setup) = self.completion_setup_start(try_start, try_end) else {
            if dbg { eprintln!("[tf] 失败：找不到完成码设置（回退无 catch 形态）"); }
            return self.try_emit_try_finally_no_catch(i, end, ho);
        };
        let Some(try_val) = self.completion_return_reg(try_setup + 2) else {
            if dbg { eprintln!("[tf] 失败：try 返回值寄存器解不出（回退无 catch 形态）"); }
            return self.try_emit_try_finally_no_catch(i, end, ho);
        };
        let Some(catch_setup) = self.completion_setup_start(catch_start, finally_hint) else {
            if dbg { eprintln!("[tf] 失败：catch 区找不到完成码设置（回退无 catch 形态）"); }
            return self.try_emit_try_finally_no_catch(i, end, ho);
        };
        let Some(catch_val) = self.completion_return_reg(catch_setup + 2) else {
            if dbg { eprintln!("[tf] 失败：catch 返回值寄存器解不出（回退无 catch 形态）"); }
            return self.try_emit_try_finally_no_catch(i, end, ho);
        };
        // catch 体尾部那条 Jump 指向 finally 本体（找不到同样回退：也还在发射之前）
        let Some(finally_start) = self.jump_target_before(catch_setup) else {
            if dbg { eprintln!("[tf] 失败：catch 体尾部找不到 Jump（回退无 catch 形态）"); }
            return self.try_emit_try_finally_no_catch(i, end, ho);
        };
        // finally 之后是完成码分发（9.x–12.x 用 SwitchOnSmiNoFeedback，13.x 降级成比较链）
        // → 找不到就容忍：分发留在线性路径里也无害（重抛带守卫、返回的是同一个值），
        // 但**绝不能**因此放弃整个 try/catch/finally 重建（node24 就是卡在这里）。
        let disp = self
            .find_completion_dispatch(finally_start, end)
            .unwrap_or(end);
        let disp_end = self.completion_dispatch_end(disp, end).unwrap_or(end);
        // ── 发射 ──（先登记 handler，避免发射内部时旧规则再包一层）
        self.used_handler_starts.push(hi.start as usize);
        self.used_handler_starts.push(ho.start as usize);
        self.line("try {");
        self.indent += 1;
        // 区间要**包含**那条 `Star v`（返回值就是它存的）——否则 Add 的结果落在区间外，
        // 合成的 `return rV` 读到的是旧值。
        if let Err(e) = self.emit_range(try_start, try_setup + 1) {
            self.line(&format!("/* 结构化失败: {e} */"));
        }
        self.line(&format!("return r{try_val};"));
        self.indent -= 1;
        let resolved = self
            .catch_scope_of(catch_start)
            .and_then(|s| self.d.scope_by_id(s))
            .and_then(|sc| sc.context_locals.first().cloned())
            .filter(|n| !n.is_empty())
            .map(|n| sanitize_var(&n));
        let catch_var = resolved.unwrap_or_else(|| "e".to_string());
        // catch 体里读异常走的是 catch context 的 `THROWN_OBJECT_INDEX = MIN_CONTEXT_SLOTS`
        // 槽 → 绑到同一个名字，否则参数叫 e、引用却是 `__ctx.ctx4`（≤7.x 的
        // MIN_CONTEXT_SLOTS 是 4，不能写死 2），运行时就变成 undefined.message。
        self.slot_aliases
            .insert(self.d.table.min_context_slots, catch_var.clone());
        self.line(&format!("}} catch ({catch_var}) {{"));
        self.indent += 1;
        if let Err(e) = self.emit_range(catch_start, catch_setup + 1) {
            self.line(&format!("/* 结构化失败: {e} */"));
        }
        self.line(&format!("return r{catch_val};"));
        self.indent -= 1;
        self.line("} finally {");
        self.indent += 1;
        if let Err(e) = self.emit_range(finally_start, disp) {
            self.line(&format!("/* 结构化失败: {e} */"));
        }
        // 注意：这里**不能**像 no_catch 路径那样在收尾前 flush acc —— 对 for-of 的
        // IteratorClose 形态，finally 体尾部挂的正是 `return()` 的结果（phi 承载），
        // 提前 flush 会把值流切断（24.12 的 destructure/for_of_in 就是这么挂的）。
        self.indent -= 1;
        self.line("}");
        Some(disp_end.max(i + 1))
    }

    /// handler 是否是 try/finally 的入口（含 6.x 的偏移形态）。
    ///
    /// 9.x+ 的 handler target 恒等于区间末尾；6.2/6.8 的入口落在 end 之后几条指令
    /// （中间夹着另一条完成码设置，如 `LdaSmi -1; Star r1; Star r0; Jump`），
    /// 于是 `target == end` 的判定整体不成立 → try/catch/finally 重建退化成两层
    /// try/catch（try_catch/destructure 的行为就错在这里）。放宽成
    /// "target ≥ end 且入口形态像 handler"。
    fn tf_handler_entry(&self, h: &Handler) -> bool {
        if h.target == h.end {
            return true;
        }
        if h.target < h.end {
            return false;
        }
        let Some(&t) = self.idx_of.get(&(h.target as usize)) else {
            return false;
        };
        let b = |k: usize| -> String {
            self.instrs[k]
                .name
                .split('.')
                .next()
                .unwrap_or(&self.instrs[k].name)
                .to_string()
        };
        let win: Vec<String> = (t..(t + 5).min(self.instrs.len())).map(b).collect();
        // catch 入口：`Star rX; CreateCatchContext rX, [scope]`
        if win.iter().any(|x| x == "CreateCatchContext") {
            return true;
        }
        // finally 入口：完成码设置（`Star v; LdaSmi/LdaZero k; Star0`）
        win.iter().any(|x| x == "LdaSmi" || x == "LdaZero")
            && win.iter().any(|x| x.starts_with("Star"))
    }

    /// [a, b) 尾部若是 `Star v; LdaSmi 1; Star0`（完成码=1 → return）→ 返回 v。
    fn completion_return_reg(&self, b: usize) -> Option<u32> {
        if b == 0 || b > self.instrs.len() {
            return None;
        }
        let base = |k: usize| {
            self.instrs[k]
                .name
                .split('.')
                .next()
                .unwrap_or(&self.instrs[k].name)
                .to_string()
        };
        // b 指向 `LdaSmi 1`：前一条是 Star（承载返回值）
        // 6.x 的完成码 0 用 `LdaZero`（值恒为 0，同样只需取前面那条 Star）
        if base(b - 1) == "LdaSmi" || base(b - 1) == "LdaZero" {
            let st = &self.instrs[b - 2];
            if st.name == "Star" {
                return match st.operands.first() {
                    Some(Operand::Reg(r)) if *r >= 0 => Some(*r as u32),
                    _ => None,
                };
            }
            if st.name.starts_with("Star") && st.name[4..].chars().all(|c| c.is_ascii_digit()) {
                return st.name[4..].parse().ok();
            }
        }
        None
    }

    /// 区间末尾前一条 Jump 的目标下标（catch 体尾部跳向 finally）。
    fn jump_target_before(&self, b: usize) -> Option<usize> {
        if b == 0 || b > self.instrs.len() {
            return None;
        }
        // 完成码设置块以一条 Jump 收尾（`Star v; LdaSmi 1; Star0; Jump <finally>`）
        for k in b..(b + 4).min(self.instrs.len()) {
            let name = &self.instrs[k].name;
            if !name.starts_with("Jump") {
                continue;
            }
            if let Some(t) = self.uncond_jump_target(&self.instrs[k].clone()) {
                if let Some(&idx) = self.idx_of.get(&t) {
                    return Some(idx);
                }
            }
        }
        None
    }

    /// 在 [start, limit) 内找"完成码设置"起点（`LdaSmi 1` 的前一条 Star）。
    fn completion_setup_start(&self, start: usize, limit: usize) -> Option<usize> {
        let mut found = None;
        for k in start..limit.min(self.instrs.len()) {
            let b = self.instrs[k].name.split('.').next().unwrap_or(&self.instrs[k].name);
            if (b == "LdaSmi" || b == "LdaZero") && k > start {
                let prev = &self.instrs[k - 1];
                if prev.name == "Star" || (prev.name.starts_with("Star") && prev.name[4..].chars().all(|c| c.is_ascii_digit())) {
                    found = Some(k - 1);
                }
            }
        }
        found
    }

    /// 分发起点：finally 之后第一个 `SwitchOnSmiNoFeedback` 的取值指令。
    fn find_completion_dispatch(&self, start: usize, limit: usize) -> Option<usize> {
        for k in start..limit.min(self.instrs.len()) {
            let b = self.instrs[k].name.split('.').next().unwrap_or(&self.instrs[k].name);
            if b == "SwitchOnSmiNoFeedback" {
                return Some(k.saturating_sub(1));
            }
        }
        None
    }

    /// 分发区间结束：各 case 体（return/rethrow 短块）之后。
    fn completion_dispatch_end(&self, start: usize, limit: usize) -> Option<usize> {
        let sw = start + 1;
        let ins = self.instrs.get(sw)?.clone();
        let ops: Vec<String> = ins
            .operands
            .iter()
            .map(|o| Decoder::new(self.d.table, self.d.layout, 0).render_operand(o))
            .collect();
        let table_start = ops
            .first()
            .and_then(|s| s.trim_matches(['[', ']']).parse::<usize>().ok())?;
        let size = ops
            .get(1)
            .and_then(|s| s.trim_matches(['[', ']']).parse::<usize>().ok())?;
        let prefix = if ins.scale > 1 { 1 } else { 0 };
        let mut last = sw;
        for k in 0..size {
            let Some(v) = self
                .pool
                .and_then(|p| self.d.cache.array_elem(p, table_start + k))
                .and_then(|e| e.as_smi())
            else {
                continue;
            };
            if let Some(&idx) = self.idx_of.get(&(ins.offset + prefix + v as usize)) {
                last = last.max(idx);
            }
        }
        let mut k = last.min(limit);
        while k < limit.min(self.instrs.len()) {
            let b = self.instrs[k].name.split('.').next().unwrap_or(&self.instrs[k].name);
            if matches!(b, "Return" | "Throw" | "ReThrow") {
                return Some(k + 1);
            }
            k += 1;
        }
        Some(limit)
    }

    /// catch 入口附近 `CreateCatchContext [池索引]` 的 ScopeInfo 对象。
    fn catch_scope_of(&self, catch_start: usize) -> Option<ObjId> {
        for k in catch_start..(catch_start + 6).min(self.instrs.len()) {
            let ins = &self.instrs[k];
            let b = ins.name.split('.').next().unwrap_or(&ins.name);
            if b != "CreateCatchContext" {
                continue;
            }
            let Some(Operand::Idx(i)) = ins.operands.first() else {
                return None;
            };
            return match self.pool.and_then(|p| self.d.cache.array_elem(p, *i as usize)) {
                Some(Elem::Ref(Ref::Object(o)))
                    if self.d.cache.obj(o).ty.is(self.d.table, "ScopeInfo") =>
                {
                    Some(o)
                }
                _ => None,
            };
        }
        None
    }

    /// 发射一段指令范围（结构化控制流重建的核心）。
    ///
    /// 规则（覆盖 V8 常见模式）：
    /// - 前向条件跳转到 break 目标 → `if (!cond) break;`
    /// - 后向条件跳转到 continue 目标 → `if (cond) continue;`
    /// - 前向条件跳转（其他）→ `if (cond) { … }`（紧跟的无条件 Jump 构成 `else`）
    /// - JumpLoop → `continue;`；前向无条件跳转到 break 目标 → `break;`
    /// - 循环头（本指令是后向跳转的目标）→ 包成 `while (true) { … }`
    fn emit_range(&mut self, i: usize, end: usize) -> Result<(), String> {
        // 结构化规则之间可能互相重入（try/switch/guard 的区间重叠）→ 加护栏，
        // 宁可输出一条注释也不要栈溢出把整个进程带走。
        if self.emit_depth > 80 {
            self.line("/* 结构化递归过深：此处降级为线性输出 */");
            return Ok(());
        }
        self.emit_depth += 1;
        let r = self.emit_range_inner(i, end);
        self.emit_depth -= 1;
        r
    }

    fn emit_range_inner(&mut self, mut i: usize, end: usize) -> Result<(), String> {
        while i < end {
            let ins = self.instrs[i].clone();
            let base = ins.name.split('.').next().unwrap_or(&ins.name).to_string();

            self.prev_base = std::mem::replace(&mut self.cur_base, base.clone());
            if std::env::var("JSCD_DBG_ACC").is_ok() {
                eprintln!(
                    "[acc] i={i} @{} {} {:?} acc_before={:?}",
                    ins.offset, ins.name, ins.operands, self.acc
                );
            }

            // ⓪ 已被 guard 子句就地发射的冷块 → 跳过（它的内容已在对应分支里生成过）。
            // 消费后要移除：同一区间可能被别的分支范围再次经过（可选链的 `LdaUndefined`
            // 块就踩过这个坑），留着会静默吞掉指令、让合并值取到分支里的旧值。
            if let Some(pos) = self.skip_spans.iter().position(|(s, _)| *s == i) {
                let (_, e) = self.skip_spans.remove(pos);
                i = e.max(i + 1);
                continue;
            }

            // ⓪-0 生成器重写折掉的机器指令：必须在这里跳过（不能落到 emit_expr_statement，
            // 否则它的 flush_acc_before 会把刚设好的 `yield` 表达式当死值成句丢掉）
            if self.instrs[i].name == "__gskip" {
                i += 1;
                continue;
            }
            // ⓪-0b 参数默认值被搬进签名后，原 prologue 的 `Star rV` 改成"按形参名回写"
            // （体里可能仍按该寄存器读这个形参；不写回的话读到的是上一个函数遗留的 rN）。
            if self.instrs[i].name == "__pregs" {
                if let Some(&(pidx, r)) = self.pregs_fixups.get(&i) {
                    let name = format!("a{pidx}");
                    self.line(&format!("{} = {name};", self.reg_display(r)));
                    if self.regs.len() <= r as usize {
                        self.regs.resize(r as usize + 1, None);
                    }
                    self.regs[r as usize] = Some(Expr::Ident(name));
                }
                self.acc = None;
                self.acc_consumed();
                i += 1;
                continue;
            }

            // ①-0 try/catch/finally（V8 的完成码形态）
            if let Some(next) = self.try_emit_try_finally(i, end) {
                if std::env::var("JSCD_DBG_RULES").is_ok() {
                    eprintln!("[rules] i={i} off={} ①-0 try/finally -> {next}", self.instrs[i].offset);
                }
                i = next.max(i + 1);
                continue;
            }

            if std::env::var("JSCD_DBG_RULES").is_ok() {
                for h in self.handlers.iter() {
                    let eff = self
                        .idx_of
                        .get(&(h.start as usize))
                        .map(|&s| {
                            let mut k = s;
                            while k < end && self.instrs[k].name == "__gskip" {
                                k += 1;
                            }
                            k
                        });
                    if eff == Some(i) {
                        eprintln!(
                            "[rules?] i={i} off={} handler=({},{},{}) used={}",
                            self.instrs[i].offset,
                            h.start,
                            h.end,
                            h.target,
                            self.used_handler_starts.contains(&(h.start as usize))
                        );
                    }
                }
            }
            // ① try/catch：handler 覆盖的区间包一层
            // handler 起点可能落在被折叠掉的机器指令上（async/generator 的状态搬运、
            // `Mov <context>` 之类）→ 起点向后移到第一条真正会被发射的指令。
            // 多个 handler 落到同一条指令时取范围最小的（最内层）：内层先包，外层
            // 若已被消费（隐式 reject）就自然不匹配。
            let eff_start = |h: &Handler| -> Option<usize> {
                let s = *self.idx_of.get(&(h.start as usize))?;
                let mut k = s;
                while k < end && self.instrs[k].name == "__gskip" {
                    k += 1;
                }
                Some(k)
            };
            if let Some(h) = self
                .handlers
                .iter()
                .filter(|h| {
                    eff_start(h) == Some(i)
                        && (h.end as usize) < usize::MAX
                        && !self.used_handler_starts.contains(&(h.start as usize))
                })
                .min_by_key(|h| h.end.saturating_sub(h.start))
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
                // catch 体的右端：
                //   6.x 把 catch/finally **内联**在代码中间（处理块紧跟 try 体之后），
                //   而且别的 try 的处理块也可能落在本 catch 体里面 → 只能取到区间末尾，
                //   内层 try 交给 catch 体内的遍历按 handler 起点自行识别；
                //   7.8+ 把所有处理块挪到函数尾部（顺序排列）→ "下一个 handler 的
                //   target 起点"正好就是本 catch 体的末端。
                let six_x = self.d.table.v8.starts_with('6');
                let mut catch_end = if six_x {
                    end
                } else {
                    self.handlers
                        .iter()
                        .filter(|x| x.target > h.target)
                        .filter_map(|x| self.idx_of.get(&(x.target as usize)).copied())
                        .filter(|s| *s > catch_start && *s <= end)
                        .min()
                        .unwrap_or(end)
                };
                // 上面那条"下一个 handler 的 target"在两种情形下不够用：
                //   ① 函数里**只有这一个** handler（`try { A } catch { B } C`，V8 不发完成码）
                //      → 退化成函数尾，把 C 整段吞进 catch；
                //   ② 函数里有**多个**顺序 try/catch（处理块都排在函数尾部）→ 下一个
                //      handler 的 target 是**第二个 catch 处理器**的位置，而第一条 try/catch
                //      的续接（第二条 try 的本体）在它**前面** → 第二条 try 被吞进第一条的
                //      catch（`target` 于是把第二个 catch 体漏到 try 外面，`e.message` 读到
                //      `__ctx.ctx2` 上的 undefined）。
                // 两种情形都有同一个结构性锚点：try 体末尾那条 `Jump`（跳过 catch 处理器）
                // 的目标就是续接点 —— 始终参与取 min。
                if !six_x && body_end > i {
                    for k in body_end..(body_end + 4).min(end) {
                        let b = self.instrs[k].name.split('.').next().unwrap_or(&self.instrs[k].name);
                        if b == "Jump" || b == "JumpConstant" {
                            if let Some(t) = self.uncond_jump_target(&self.instrs[k].clone()) {
                                if let Some(&ti) = self.idx_of.get(&t) {
                                    if ti > catch_start && ti < catch_end {
                                        catch_end = ti;
                                    }
                                }
                            }
                            break;
                        }
                    }
                }
                // 6.x 的 try/catch 之后若还有"两路共享的续接块"（try 体正常完成时跳进去、
                // catch 体掉出来也进去），catch 体右端必须截在那之前 —— 否则续接块被吞进
                // catch，try 体一路直接落空（`async function f(){ let r; try{r=await p}catch(e){r="c"} return "r="+r }`
                // 的成功路径会返回 undefined）。用"catch_start 之前的跳转落进 catch 体"定位它。
                if six_x {
                    if let Some(t) = self
                        .early_jumps
                        .iter()
                        .filter(|(src, tgt)| *src < catch_start && *tgt > catch_start && *tgt < catch_end)
                        .map(|(_, tgt)| *tgt)
                        .min()
                    {
                        catch_end = t;
                    }
                }
                // V8 的合成"吞掉异常并复位 pending message"处理器
                // （`LdaTheHole; SetPendingMessage; Ldar rCtx; Jump <join>`）后面**紧挨着**
                // completion dispatch 的 else 分支代码（它不属于 catch）→ catch 体必须在
                // 这条 Jump 处截断。不截断就会把别人的分支吞进来：node10 的 for_of_in 里
                // 那段 `%_Call(return, it)` 于是被发射两遍，未加守卫的那次直接 TypeError。
                if six_x && catch_start < self.instrs.len() {
                    // 处理块的收尾是"重抛守卫"：`JumpIfFalse <join>; Ldar rX; ReThrow`，
                    // 之后（<join>）就是正常流程 —— 处理块到此为止。取 end 会把整个函数
                    // 剩余部分都吞进 catch（for_of_in 的 target 于是直接返回 undefined）。
                    let mut k = catch_start;
                    while k + 2 < catch_end.min(self.instrs.len()) {
                        if self.instrs[k].name.starts_with("JumpIf") {
                            if let Some(t) = self.cond_jump_target(&self.instrs[k].clone()) {
                                if let Some(&t_idx) = self.idx_of.get(&t) {
                                    let guard_body_term = t_idx > k + 1
                                        && self.instrs[t_idx - 1].name.starts_with("ReThrow");
                                    if guard_body_term {
                                        catch_end = t_idx;
                                        break;
                                    }
                                }
                            }
                        }
                        k += 1;
                    }
                }
                // 先登记这个 handler 已被消费：这样从 i 开始发射后，下一次迭代虽然还落在
                // 同一个下标上，①也不会再匹配同一个 handler（否则 try 自我重入、嵌套爆炸）。
                self.used_handler_starts.push(h.start as usize);
                if std::env::var("JSCD_DBG_RULES").is_ok() {
                    eprintln!(
                        "[rules] i={i} off={} ① try/catch start={} end={} target={}",
                        self.instrs[i].offset, h.start, h.end, h.target
                    );
                }
                self.line("try {");
                self.indent += 1;
                // 从 i（而不是 i+1）开始：handler 起点可能正好落在**循环头**上
                // （V8 把整个 for-of 包在迭代器 close 的 try 里），跳过它②循环规则就看不到
                // 这个头 → 没有 while 包裹、回边退化成注释 → 循环只跑一次。
                self.emit_range(i, body_end)?;
                // try 体末尾留在 acc 里的副作用表达式（如 IteratorClose 的
                // `%_Call(return, iterator)`）必须在**这里**落地：否则它被当成待发射值
                // 带进 catch 体（输出成 `catch { ...; Call(...) }`，调用时机整个错位）。
                self.materialize_acc_stmt();
                self.indent -= 1;
                if std::env::var("JSCD_DBG_RULES").is_ok() {
                    let name_at = |k: usize| {
                        self.instrs
                            .get(k)
                            .map(|i| format!("{}@{}", i.name, i.offset))
                            .unwrap_or_default()
                    };
                    eprintln!(
                        "[rules] ① catch 体 [{catch_start},{catch_end}) 之后是 {} / 续接点候选 {}",
                        name_at(catch_end),
                        name_at(catch_end)
                    );
                }
                self.line("} catch (e) {");
                self.indent += 1;
                // catch 体里读异常走 catch context 的 `THROWN_OBJECT_INDEX = MIN_CONTEXT_SLOTS`
                // 槽 → 绑到 `e`，否则输出 `__ctx.ctx2`（async 快路径的 Reject 分支因此
                // 抛出的是上下文槽而不是捕获的错误）。
                // 只对 `*CurrentContextSlot` 生效；显式上下文读取（寄存器里是**外层**上下文，
                // 见 context_name 的说明）要跳过最内层 catch 作用域，否则外层同名槽会被读成 `e`。
                let saved_alias = self.catch_alias.take();
                let saved_skip = self.explicit_scope_skip;
                self.catch_alias = Some("e".to_string());
                self.explicit_scope_skip = 1;
                let r = self.emit_range(catch_start, catch_end);
                // catch 体尾部挂着"待求值"的副作用表达式（最后一句是个调用）必须在**这里**
                // 落地：否则它被当成待发射值带出 catch，在**下一条语句的位置**执行 ——
                // 那可能是 try 之外的另一条路径（`try { A } catch { out.push("E1") } try { B }`
                // 的 `E1` 于是变成无条件执行、还排在第二个 try 里）。
                self.materialize_acc_stmt();
                self.catch_alias = saved_alias;
                self.explicit_scope_skip = saved_skip;
                r?;
                self.indent -= 1;
                self.line("}");
                i = catch_end.max(i + 1);
                continue;
            }

            // ①-b 6.x 的 null/undefined 守卫（两条跳转合成一个）
            if let Some(next) = self.try_emit_null_guard(i, end) {
                i = next.max(i + 1);
                continue;
            }

            // ② 循环头 → while(true) 包装
            if let Some((back_idx, exit_target)) = self.find_loop(i, end) {
                // 体内还有回边（嵌套循环）→ 打标签，供内层的 `break outer` / `continue outer`
                let nested = (i + 1..back_idx).any(|k| {
                    let cur = self.instrs[k].offset;
                    self.uncond_jump_target(&self.instrs[k])
                        .or_else(|| self.cond_jump_target(&self.instrs[k]))
                        .map(|t| t <= cur && t >= self.instrs[i].offset)
                        .unwrap_or(false)
                });
                let label = if nested {
                    self.label_counter += 1;
                    format!("L{}", self.label_counter)
                } else {
                    String::new()
                };
                if label.is_empty() {
                    self.line("while (true) {");
                } else {
                    self.line(&format!("{label}: while (true) {{"));
                }
                self.indent += 1;
                self.loops.push(LoopCtx {
                    continue_target: ins.offset,
                    break_target: exit_target,
                    label: label.clone(),
                });
                // 循环头指令要放在循环体内：`continue` 回到顶部时需要重新求值（条件计算就在头部）
                self.emit_expr_statement(&ins);
                self.emit_range(i + 1, back_idx)?;
                // 回边本身用 continue 表示（不要把它当普通指令发射）
                let back_ins = &self.instrs[back_idx];
                let is_plain_loop = !back_ins.name.starts_with("JumpLoop")
                    && self.uncond_jump_target(back_ins).is_some();
                if !is_plain_loop {
                    self.flush_acc_at_edge(ins.offset);
                    self.line("continue;");
                }
                self.loops.pop();
                self.indent -= 1;
                self.line("}");
                i = (back_idx + 1).max(i + 1);
                continue;
            }

            // ③-0a for-in 枚举协议
            if let Some(next) = self.try_emit_for_in(i, end) {
                i = next.max(i + 1);
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
                        self.acc = Some(Expr::Ident(phi));
                    }
                }
                let cond_e = self.cond_of(&base);
                let cond = cond_e.render();
                // 匹配的是哪一层循环？非最内层 → 必须带标签（`break outer` 语义）
                let brk = self
                    .loops
                    .iter()
                    .rposition(|l| l.break_target == target)
                    .map(|idx| (idx, self.loops[idx].label.clone()));
                if let Some((idx, label)) = brk {
                    let stmt = if idx + 1 == self.loops.len() || label.is_empty() {
                        "break;".to_string()
                    } else {
                        format!("break {label};")
                    };
                    self.line(&format!("if ({cond}) {stmt}"));
                    i += 1;
                    continue;
                }
                let cont = self
                    .loops
                    .iter()
                    .rposition(|l| l.continue_target == target)
                    .map(|idx| (idx, self.loops[idx].label.clone()));
                if let Some((idx, label)) = cont {
                    let stmt = if idx + 1 == self.loops.len() || label.is_empty() {
                        "continue;".to_string()
                    } else {
                        format!("continue {label};")
                    };
                    self.line(&format!("if ({cond}) {stmt}"));
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
                        if std::env::var("JSCD_DBG_IF").is_ok() {
                            eprintln!("[if] ③b fallthrough-cold i={i} target={target} t_idx={t_idx} not_taken={not_taken}");
                        }
                        self.line(&format!("if ({not_taken}) {{"));
                        self.indent += 1;
                        let r = self.emit_range(i + 1, t_idx);
                        self.indent -= 1;
                        self.line("}");
                        r?;
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
                            if std::env::var("JSCD_DBG_IF").is_ok() {
                                eprintln!("[if] ③a-guard i={i} target={target} t_idx={t_idx} cond={cond}");
                            }
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
                            r?;
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
                                        if std::env::var("JSCD_DBG_IF").is_ok() {
                                            eprintln!("[if] empty-then i={i} target={target} e_idx={e_idx} cond={cond}");
                                        }
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
                        let acc_in_stored = self.acc_stored;
                        let phi = self.new_phi();
                        let if_start = self.out.len();
                        let then_cond = Expr::Un {
                            op: "!",
                            e: Box::new(cond_e.clone()),
                            postfix: false,
                        }
                        .render();
                        if std::env::var("JSCD_DBG_IF").is_ok() {
                            eprintln!("[if] if-else i={i} target={target} t_idx={t_idx} then_cond={then_cond}");
                        }
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
                        // then / else 两段文本的分界（没有 else 时就是 self.out.len()）
                        let mut else_text_start = self.out.len();
                        // 紧邻 target 之前的无条件 Jump → else 分支
                        if t_idx > 0 {
                            if let Some(else_target) = self.uncond_jump_target(&self.instrs[t_idx - 1])
                            {
                                if else_target > target {
                                    if let Some(&e_idx) = self.idx_of.get(&else_target) {
                                        acc_then = self.acc.clone();
                                        // else 路径是**跳转**过来的：acc 还是分支前的值
                                        // （then 体里的求值不会发生在这条路径上）。不恢复的话，
                                        // then 里 pending 的调用会被当成"待求值"在 else 里再发一遍
                                        // —— 可选链 `?.()` 的短路分支就这样多出一次调用
                                        // （`obj?.m?.()`：m 为 nullish 时产物里还有一次 `obj.m()`）。
                                        self.acc = acc_in.clone();
                                        self.acc_stored = acc_in_stored;
                                        self.acc_stored_reg = acc_in_reg;
                                        self.line("} else {");
                                        self.indent += 1;
                                        let else_start = self.out.len();
                                        else_text_start = else_start;
                                        // 不按 `end` 裁剪：短路共享目标可能正好在当前范围之外
                                        self.emit_range(t_idx, e_idx)?;
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
                        if std::env::var("JSCD_DBG_IF").is_ok() {
                            eprintln!(
                                "[if] i={i} target={target} then={then_v:?} else={cur:?} entry={entry:?} changed={changed} has_else={has_else}"
                            );
                        }
                        // new_phi 已把名字登记进 phi_vars（并就地声明），这里只负责回填起始值。
                        // 两条分支都**无条件**给 phi 赋过值、且赋值不看 phi 旧值时，这次初始化
                        // 是多余的 —— 而且它的右值可能带副作用（`phi0 = a0 instanceof Object;`
                        // 会多调一次 `Symbol.hasInstance`）。判据要能挡掉"嵌套 if 里的条件赋值"
                        // 与"`phi = phi + 1` 这种读旧值"的形态，所以逐行看：第一条提到 phi 的行
                        // 必须是**分支顶层**的 `phi = <不含 phi 的表达式>`。
                        let phi_probe = format!("{phi} = ");
                        let expected_indent = "  ".repeat(self.indent + 1);
                        let branch_uncond = |text: &str| -> bool {
                            for l in text.lines() {
                                if !l.contains(&phi[..]) {
                                    continue;
                                }
                                let lead = l.len() - l.trim_start().len();
                                let t = l.trim();
                                if lead != expected_indent.len() || !t.starts_with(&phi_probe) {
                                    return false;
                                }
                                return !t[phi_probe.len()..].contains(&phi[..]);
                            }
                            false
                        };
                        let skip_init = has_else
                            && branch_uncond(&self.out[body_start..else_text_start])
                            && branch_uncond(&self.out[else_text_start..]);
                        let _ = (then_assigned, else_assigned);
                        if changed {
                            let init = match acc_in_reg {
                                Some(r) => format!("r{r}"),
                                None => acc_in
                                    .as_ref()
                                    .map(|e| e.render())
                                    .unwrap_or_else(|| "undefined".into()),
                            };
                            if !skip_init {
                                let indent = "  ".repeat(self.indent);
                                self.out
                                    .insert_str(if_start, &format!("{indent}{phi} = {init};\n"));
                            }
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
                    if let Some((idx, label)) = self
                        .loops
                        .iter()
                        .rposition(|l| l.continue_target == target)
                        .map(|idx| (idx, self.loops[idx].label.clone()))
                    {
                        self.flush_acc_at_edge(target);
                        if idx + 1 == self.loops.len() || label.is_empty() {
                            self.line("continue;");
                        } else {
                            self.line(&format!("continue {label};"));
                        }
                    } else if let Some((idx, label)) = self
                        .loops
                        .iter()
                        .rposition(|l| l.break_target == target)
                        .map(|idx| (idx, self.loops[idx].label.clone()))
                    {
                        self.flush_acc_at_edge(target);
                        if idx + 1 == self.loops.len() || label.is_empty() {
                            self.line("break;");
                        } else {
                            self.line(&format!("break {label};"));
                        }
                    } else {
                        self.line(&format!("/* 回边 @{target}（未识别的循环结构） */"));
                    }
                    i += 1;
                    continue;
                }
                if let Some((idx, label)) = self
                    .loops
                    .iter()
                    .rposition(|l| l.break_target == target)
                    .map(|idx| (idx, self.loops[idx].label.clone()))
                {
                    self.flush_acc_before(&base);
                    if idx + 1 == self.loops.len() || label.is_empty() {
                        self.line("break;");
                    } else {
                        self.line(&format!("break {label};"));
                    }
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

    /// 把待发射的 acc 立刻物化成语句（调用方已判定"表达式属于当前块"）。
    ///
    /// 与 `flush_acc_before` 的区别：这里不看下一条指令保不保留 acc ——
    /// 结构化规则（for-in / switch 等）在块末尾需要把"留在累加器里的副作用表达式"
    /// 落地在**块内**，否则它会被外层合并点物化到块外面去
    /// （`keys.push(k)` 曾因此只在循环结束后执行一次）。
    fn materialize_acc_stmt(&mut self) {
        if self.acc_stored {
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

    /// 跳转出口（回边 continue / break）处把待发射的 acc 成句。
    ///
    /// acc 在 V8 里是真正的机器寄存器、跨跳转保留；但"表达式文本"是我们自己的记账，
    /// 跳转目标重新写 acc 时旧表达式就是死值 —— 死的是值，副作用不能丢：
    /// `out.push(v)` 后面紧跟 `Mov r4, r11` + `JumpLoop`（两条都"保留 acc"），
    /// 于是整条调用被 continue 吞掉，`target(3)` 返回空串。
    fn flush_acc_at_edge(&mut self, target_off: usize) {
        let Some(&k) = self.idx_of.get(&target_off) else {
            return;
        };
        let next = self.instrs[k]
            .name
            .split('.')
            .next()
            .unwrap_or("")
            .to_string();
        if next.is_empty() || self.reads_acc(&next) || self.preserves_acc(&next) {
            return;
        }
        if self.acc_stored {
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
                // 一定按**有符号**读：`LdaSmi [-1]` 用无符号解析会失败并落到 0 ——
                // 静默错值（`obj?.raw ?? -1` 的兜底值曾变成 0：语法没问题、行为全错）。
                let v = match ins.operands.first() {
                    Some(Operand::Imm(v)) => *v,
                    Some(Operand::Idx(v)) => *v as i64,
                    _ => ops
                        .first()
                        .and_then(|s| s.trim_matches(['[', ']']).parse::<i64>().ok())
                        .unwrap_or(0),
                };
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
                let value = self.acc.clone();
                // 参数槽在字节码里是负下标、文本里是 `aN`：`reg_of("a0")` 会取到下标 0，
                // 于是 `Star a0`（`finally { x = x + 100 }` 里的参数写回）被写成 `r0 = …`
                // ——更新落到死临时上，AST 优化层还会把整句当死代码删掉。按**名字**写。
                if let Some(name) = param_slot_name(&arg(0)) {
                    self.store_named(&name, value.unwrap_or(Expr::Undefined));
                    self.acc = Some(Expr::Ident(name));
                    self.acc_stored = true;
                    self.acc_stored_reg = None;
                } else {
                    let r = reg_of(&arg(0));
                    self.store_reg(r, value.clone());
                    if let Some(m) = value {
                        if matches!(m, Expr::Member { .. }) {
                            self.reg_prop.insert(r, m);
                        }
                    }
                    // 刚落下来的如果是个上下文 → 记下"这个寄存器持有哪个作用域的上下文"
                    if let Some(scope) = self.pending_ctx.take() {
                        if std::env::var("JSCD_DBG_CTX").is_ok() {
                            eprintln!("[regscope-star] r{r} <- scope {scope}");
                        }
                        self.reg_scope.insert(r, scope);
                    }
                    // Star 之后 acc 与 r 同值 → 直接用 r 表示，
                    // 否则表达式文本会在 r 被改写后失真（`obj?.nope?.deep` 曾算成 `r7.deep`）
                    self.acc = Some(Expr::Reg(r));
                    self.acc_stored = true;
                    self.acc_stored_reg = Some(r);
                }
            }
            // 短 Star：StarN 直接编码寄存器号
            _ if base.starts_with("Star") && base[4..].chars().all(|c| c.is_ascii_digit()) => {
                let r: u32 = base[4..].parse().unwrap_or(0);
                self.store_reg(r, self.acc.clone());
                if let Some(m) = self.acc.clone() {
                    if matches!(m, Expr::Member { .. }) {
                        self.reg_prop.insert(r, m);
                    }
                }
                if let Some(scope) = self.pending_ctx.take() {
                    self.reg_scope.insert(r, scope);
                }
                self.acc = Some(Expr::Reg(r));
                self.acc_stored = true;
                self.acc_stored_reg = Some(r);
            }
            "Mov" => {
                let value = self.operand_expr(&arg(0));
                if let Some(name) = param_slot_name(&arg(1)) {
                    self.store_named(&name, value.clone());
                } else {
                    let dst = reg_of(&arg(1));
                    self.store_reg(dst, Some(value));
                    // `Mov rA, rB`：rB 拿到的上下文作用域跟着 rA。
                    // **必须严格判定源操作数是不是真寄存器**：`Mov <context>, r0` 里
                    // `reg_of("<context>")` 会落到 0（parse 失败 → unwrap_or(0)）→ 把
                    // **r0 的作用域**错误地传给 r0，读侧于是把脚本级 `orig` 解析成
                    // 函数级 `real`（闭包探针直接 TypeError）。
                    if let Some(rest) = arg(0).strip_prefix('r') {
                        if let Ok(src) = rest.parse::<u32>() {
                            if let Some(scope) = self.reg_scope.get(&src).copied() {
                                self.reg_scope.insert(dst, scope);
                            }
                        }
                    }
                    if let Some(scope) = self.pending_ctx.take() {
                        self.reg_scope.insert(dst, scope);
                    }
                }
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
            // 宽松模式下的动态查找（`with`/间接 eval/未声明名）：名字在第一个操作数指向的常量池上。
            // 语义按"全局名读取"渲染 —— 反编译产物里作用域链已摊平，这就是源码里的那个名字。
            "LdaLookupGlobalSlot" | "LdaLookupSlot" | "LdaLookupScriptContextSlot"
            | "LdaLookupSlotInsideTypeof" | "LdaLookupScriptContextSlotInsideTypeof" => {
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
                if std::env::var("JSCD_DBG_SLOT").is_ok() {
                    eprintln!("[lda] base={base} slot={slot} ops={:?}", ins.operands);
                }
                // 脚本上下文多一个 extension 槽 → 变量基准 +1（见 context_name_base）
                let base = if base.contains("ScriptContextSlot") {
                    self.d.script_ctx_base()
                } else {
                    self.d.table.min_context_slots
                };
                let nm = self.context_name_base(slot, true, base);
                if std::env::var("JSCD_DBG_SLOT").is_ok() {
                    eprintln!("[lda] slot={slot} -> {nm}");
                }
                self.acc = Some(Expr::Ident(nm));
            }
            "LdaContextSlot" | "LdaScriptContextSlot" | "LdaImmutableContextSlot" => {
                let i = ops.len().saturating_sub(2);
                let slot = idx_num(&arg(i)).unwrap_or(0);
                self.last_ctx_slot = Some(slot);
                let base = if base.contains("ScriptContextSlot") {
                    self.d.script_ctx_base()
                } else {
                    self.d.table.min_context_slots
                };
                // 精确解析（同号槽不串名）：
                //   ① 寄存器持有哪个上下文已知 → 按该上下文 + depth；
                //   ② 操作数是 `<context>`（当前上下文）→ 从最内层作用域走 depth 跳。
                // 都解不出来才退回原来的启发式。
                let depth = self.context_depth(&ops);
                // 操作数是 `<context>`（当前上下文）→ 从当前作用域走 depth 跳；
                // 是真寄存器 → **只能**按 reg_scope 解析（寄存器里可能是外层上下文，
                // 拿当前作用域去解会把脚本级 `orig` 读成函数级 `real`）。
                let ctx_is_current = ops.first().map(|s| s == "<context>").unwrap_or(false);
                let precise = if ctx_is_current {
                    self.context_name_by_depth(slot, depth, base)
                } else {
                    self.context_reg_slot_depth(&ops)
                        .and_then(|(r, _, d)| self.context_name_via_reg(r, slot, d, base))
                }
                // 形参拷贝槽优先用 `aK`：async/生成器的序言把形参搬进上下文槽，
                // 摊平产物里形参是 `aK` 而**不是**源码名（这里算出 `q` 会让
                // `await q` 变成未声明名字 → undefined；node8/10 的 async retry 就栽在这）。
                .filter(|n| {
                    self.gen_param_slots
                        .get(&slot)
                        .map(|p| p == n)
                        .unwrap_or(true)
                });
                if std::env::var("JSCD_DBG_SLOT").is_ok() {
                    eprintln!(
                        "[lda-explicit] base={base} ops={ops:?} depth={depth} precise={precise:?} fallback={:?}",
                        self.context_name_base(slot, false, base)
                    );
                }
                self.acc = Some(Expr::Ident(
                    precise.unwrap_or_else(|| self.context_name_base(slot, false, base)),
                ));
            }
            "StaCurrentContextSlot" | "StaCurrentScriptContextSlot" | "StaContextSlot" | "StaScriptContextSlot" => {
                if std::env::var("JSCD_DBG_SLOT").is_ok() {
                    eprintln!("[sta] base={base} ops={:?}", ins.operands);
                }
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
                // 名字解不出来时 context_name 落到 `__ctx.ctxN`（成员表达式）——
                // 这里不能再 sanitize（会变成另一个标识符 `__ctx_ctxN`，写进去读不出来：
                // node24 的类构造器就是这么丢的）。
                let via_cur = !base.starts_with("StaContextSlot") && !base.starts_with("StaScriptContextSlot");
                let cbase = if base.contains("ScriptContextSlot") {
                    self.d.script_ctx_base()
                } else {
                    self.d.table.min_context_slots
                };
                // 与读侧同一套精确解析（否则**写**会落到同号槽的另一个名字上：
                // 闭包里 `const real = it.return` 曾写成顶层 `orig = …`，
                // 嵌套函数读 `real` 就是 undefined）。
                let raw = if via_cur {
                    self.context_name_by_depth(slot, 0, cbase)
                        .unwrap_or_else(|| self.context_name_base(slot, true, cbase))
                } else {
                    let ctx_is_current = ops.first().map(|s| s == "<context>").unwrap_or(false);
                    let precise = if ctx_is_current {
                        self.context_name_by_depth(slot, self.context_depth(&ops), cbase)
                    } else {
                        self.context_reg_slot_depth(&ops)
                            .and_then(|(r, _, d)| self.context_name_via_reg(r, slot, d, cbase))
                    };
                    precise.unwrap_or_else(|| self.context_name_base(slot, false, cbase))
                };
                if std::env::var("JSCD_DBG_CTX").is_ok() {
                    eprintln!("[stctx] base={base} slot={slot} via_current={via_cur} -> {raw}");
                }
                let name = if raw.starts_with("__ctx.") {
                    raw
                } else {
                    sanitize_var(&raw)
                };
                self.line(&format!("{name} = {};", value.render()));
                // 存回去之后 acc 就是该槽的值本身：不能留 `++n` 这类表达式文本 ——
                // 后续 Return 会照着再渲染一遍，副作用做两次（`return ++n` 又自增一次，
                // closure fixture 因此算成 6 而不是 3）
                self.acc = Some(Expr::Ident(name));
                self.acc_consumed();
            }
            "PushContext" => {
                // V8 语义（interpreter-generator.cc）：`new_context = acc` 成为当前上下文，
                // **旧上下文**存进 rX（留着给 `PopContext rX` 恢复）。
                //   新上下文的作用域在 Create* 时已压进 ctx_scopes（不再重复压，否则错位）；
                //   rX 拿到的是"上一个"上下文 → 它的作用域 = ctx_scopes 的次外层。
                //   `LdaContextSlot rX, [slot], [depth]` 正是按这个旧上下文解名字
                //   （V8 14 的 `for (let ctor …) { const rab … }`：读 `ctor` 用的是
                //   PushContext 留在 rX 里的外层上下文 —— 拿最内层解会读成 `rab`）。
                let r = reg_of(&arg(0));
                let len = self.ctx_scopes.len();
                let prev = if len >= 2 { self.ctx_scopes[len - 2] } else { None };
                if std::env::var("JSCD_DBG_CTX").is_ok() {
                    eprintln!("[pushctx] r{r} len={len} ctx_scopes={:?} prev={prev:?}", self.ctx_scopes);
                }
                match prev {
                    Some(scope) => {
                        self.reg_scope.insert(r, scope);
                    }
                    None => {
                        self.reg_scope.remove(&r);
                    }
                }
                // 新上下文没落进任何寄存器（就在当前上下文里），pending 到此为止
                self.pending_ctx = None;
            }

            // ── 属性访问 ──
            // 7.7–9.x 的 `LdaNamedPropertyNoFeedback` 是同一件事的旧名（只是带反馈槽），
            // 不认它 → 整个 `console.log(...)` 变成 `__runtime.LdaNamedPropertyNoFeedback.call(...)`
            // 的桩调用（node12/13 的脚本因此一行都不输出）。
            "LdaNamedProperty" | "LdaNamedPropertyNoFeedback" | "GetNamedProperty"
            | "LdaNamedPropertyFromSuper" | "GetNamedPropertyFromSuper" => {
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
            "StaNamedProperty" | "SetNamedProperty" | "StaNamedOwnProperty" | "DefineNamedOwnProperty"
            // 6.2 的存储指令还带 Sloppy/Strict 后缀（6.8 起才并入无后缀名），操作数形状一致
            | "StaNamedPropertySloppy" | "StaNamedPropertyStrict" | "StaNamedPropertyNoFeedback" => {
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
            "StaKeyedProperty" | "SetKeyedProperty" | "StaKeyedPropertySloppy" | "StaKeyedPropertyStrict" => {
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
            "StaGlobal" | "StaGlobalSloppy" | "StaGlobalStrict" => {
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
            // 宽松模式动态赋值：`StaLookupSlot [名], <flags>`（名字是**第一个**操作数）
            "StaLookupSlot" => {
                let name = idx_num(&arg(0))
                    .map(|i| match self.constant(i) {
                        Expr::Str(s) => s,
                        other => other.render(),
                    })
                    .unwrap_or_else(|| "__lookup".into());
                let name = sanitize_var(&name);
                let value = self.acc.clone().unwrap_or(Expr::Undefined);
                self.line(&format!("{name} = {};", value.render()));
                self.acc_consumed();
            }
            // `delete <动态名>`（严格模式下 V8 会先抛，这里按源码写法渲染）
            "DeleteLookupSlot" => {
                let name = idx_num(&arg(0))
                    .map(|i| match self.constant(i) {
                        Expr::Str(s) => s,
                        other => other.render(),
                    })
                    .unwrap_or_else(|| "__lookup".into());
                let name = sanitize_var(&name);
                self.line(&format!("delete {name};"));
                self.acc = Some(Expr::Bool(true));
                self.acc_consumed();
            }
            "StaDataPropertyInLiteral" | "DefineKeyedOwnPropertyInLiteral" => {
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
                // 累加器已被存进数组：漏掉这句会让下一条指令的 flush 把同一个表达式
                // **再发一遍**（`r3[i] = f(); f();` —— 调用被求值两次）。
                self.acc_consumed();
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
            // V8 14.x 的字符串拼接特化：`Add_StringConstant_Internalize <reg 常量>, [slot], #variant`
            // —— acc 是另一个操作数，语义就是 `reg + acc`（bytecodes.h 的注释：kReg /* lhs */、
            // acc 读写；`"c" + n` 走这条）。不认它会把 `obj["c" + n]` 渲染成
            // `obj[__runtime.Add_StringConstant_Internalize]` → 取到 undefined。
            "Add_StringConstant_Internalize" => {
                let l = self.reg_expr(&arg(0));
                let r = self.acc.clone().unwrap_or(Expr::Hole);
                self.acc = Some(Expr::Bin {
                    op: "+",
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
            // 6.x 的完成码分派用 `TestEqualStrictNoFeedback`（同一件事的旧名）——
            // 不认它 → 分派整段落成 `__runtime.TestEqualStrictNoFeedback(...)` 桩调用，
            // for-of 收尾的重抛守卫永远不成立（异常被吞、正常结束还可能多调一次 return）。
            "TestEqual" | "TestEqualStrict" | "TestEqualStrictNoFeedback" | "TestLessThan"
            | "TestLessThanOrEqual" | "TestGreaterThan" | "TestGreaterThanOrEqual"
            | "TestInstanceOf" | "TestIn" | "TestReferenceEqual" => {
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
                // flag 是 `TestTypeOfFlag` 枚举值，**枚举顺序随版本差一位**：
                // BigInt 支持（V8 6.7 / node 10.4）往枚举里插了 `kIsBigInt`，
                // 于是 undefined/function/object 从 4/5/6 挪到 5/6/7。
                // 实测（`typeof x === "…"` 探针逐版本 dump）：
                //   ≥6.7：0 number / 1 string / 2 symbol / 3 boolean / 4 bigint /
                //         5 undefined / 6 function / 7 object
                //   <6.7：0 number / 1 string / 2 symbol / 3 boolean /
                //         4 undefined / 5 function / 6 object
                // 认错一位会把"是不是函数"判成"是不是 undefined"——8.17 的 for-of 收尾
                // 因此对着活函数抛 `TypeError`（正常应该调用它）。
                let modern = self.d.v8_at_least(6, 7);
                let want = match (ops.first().map(|s| s.as_str()), modern) {
                    (Some("#0"), _) => "number",
                    (Some("#1"), _) => "string",
                    (Some("#2"), _) => "symbol",
                    (Some("#3"), _) => "boolean",
                    (Some("#4"), true) => "bigint",
                    (Some("#4"), false) => "undefined",
                    (Some("#5"), true) => "undefined",
                    (Some("#5"), false) => "function",
                    (Some("#6"), true) => "function",
                    (Some("#6"), false) => "object",
                    (Some("#7"), _) => "object",
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
                // 变参形态（tables: Reg/RegList/RegCount/Idx）：**接收者与实参一起**放在
                // 寄存器组里（第一格是接收者，其余是实参）——`CallProperty r4, r5-r8` 是
                // `r5.m(r6, r7, r8)`。定参形态（…0/1/2）才是"接收者单独一格"。
                let (receiver, fixed_args): (Expr, Vec<Expr>) = if base == "CallProperty" {
                    let mut list = self.reglist_of(ins, &ops, 1);
                    let recv = if list.is_empty() { Expr::Hole } else { list.remove(0) };
                    (recv, list)
                } else {
                    let n = match base.as_str() {
                        "CallProperty0" => 0,
                        "CallProperty1" => 1,
                        _ => 2,
                    };
                    let recv = self.operand_expr(&arg(1));
                    let args = (0..n).map(|k| self.operand_expr(&arg(2 + k))).collect();
                    (recv, args)
                };
                // `r2 = o.m; …; CallProperty1 r2, o, arg` → 写成 `o.m(arg)`：
                // 更可读，而且保持"非函数时抛 X is not a function"的原始语义
                // （`.call` 形式在 callee 为 null/undefined 时报的是另一种错）。
                let callee_is_loaded_method = {
                    let (_, reg) = split_reg(&arg(0));
                    match (reg.and_then(|r| self.reg_prop.get(&r)), &callee) {
                        (Some(Expr::Member { obj, .. }), Expr::Reg(_)) => {
                            obj.render() == receiver.render()
                        }
                        _ => false,
                    }
                };
                let args: Vec<Expr> = fixed_args;
                // 语义：`<callee-stored-in-reg>(<receiver>, args)` —— 接收者必须传进去，
                // 否则 `arr.join("-")` 会退化成 `fn("-")`（this=undefined → TypeError）。
                let callee = if callee_is_loaded_method {
                    let (_, reg) = split_reg(&arg(0));
                    reg.and_then(|r| self.reg_prop.get(&r).cloned()).unwrap()
                } else {
                    match callee {
                        Expr::Member { obj, key } => {
                            // 形如 `obj.method` 已自带接收者
                            let _ = receiver;
                            Expr::Member { obj, key }
                        }
                        other => Expr::Member {
                            obj: Box::new(other),
                            key: Key::Ident("call".to_string()),
                        },
                    }
                };
                let mut all = args;
                if !callee_is_loaded_method
                    && matches!(callee, Expr::Member { ref key, .. } if matches!(key, Key::Ident(k) if k == "call"))
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
                // 变参形态（Reg/RegList/RegCount/Idx）：寄存器组**全是实参**（接收者是
                // undefined，不在表里）——`CallUndefinedReceiver r4, r5-r7` 是 `r4(r5,r6,r7)`。
                let args: Vec<Expr> = if base == "CallUndefinedReceiver" {
                    self.reglist_of(ins, &ops, 1)
                } else {
                    let n = match base.as_str() {
                        "CallUndefinedReceiver0" => 0,
                        "CallUndefinedReceiver1" => 1,
                        _ => 2,
                    };
                    (0..n).map(|k| self.operand_expr(&arg(1 + k))).collect()
                };
                self.acc = Some(Expr::Call {
                    callee: Box::new(callee),
                    args,
                    is_new: false,
                    spread_arg: None,
                });
            }
            // CallAnyReceiver <callee>, <reglist: 接收者 + 实参>, <count>, [slot]
            // 所有版本同形（tables: Reg/RegList/RegCount/Idx）——**被调在第一个寄存器操作数**，
            // 不是累加器；寄存器组第一格是接收者。6.x 的 `super.m()` 就是这个形态
            // （`LdaKeyedProperty <closure>` 取 home object → CallRuntime[LoadFromSuper] →
            //  CallAnyReceiver r1, <this>-<this>），按 acc 取会把 `<this>-<this>` 当实参渲染，
            // 产物直接语法错误（`undefined /* hole */(<this>-<this>, …)`）。
            "CallAnyReceiver" | "Call" | "CallNoFeedback" => {
                let callee = self.operand_expr(&arg(0));
                let regs = self.reglist_of(ins, &ops, 1);
                let (recv, rest) = match regs.split_first() {
                    Some((r, rest)) => (r.clone(), rest.to_vec()),
                    None => (Expr::Hole, Vec::new()),
                };
                // 与 CallProperty 一致：接收者必须传进去（否则 this=undefined）
                let mut args = vec![recv];
                args.extend(rest);
                self.acc = Some(Expr::Call {
                    callee: Box::new(Expr::Member {
                        obj: Box::new(callee),
                        key: Key::Ident("call".into()),
                    }),
                    args,
                    is_new: false,
                    spread_arg: None,
                });
            }
            "CallWithSpread" | "CallWithArrayLike" => {
                // V8: `CallWithSpread <callable>, <args+spread reglist>, [slot]`
                // 被调在**第一个寄存器操作数**，展开的实参是寄存器组最后一个
                let callee = self.reg_expr(&arg(0));
                let regs = self.reglist_of(ins, &ops, 1);
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
                let args = self.reglist_of(ins, &ops, 1);
                self.acc = Some(Expr::Call {
                    callee: Box::new(callee),
                    args,
                    is_new: true,
                    spread_arg: None,
                });
            }
            "ConstructWithSpread" => {
                let callee = self.operand_expr(&arg(0));
                let regs = self.reglist_of(ins, &ops, 1);
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
                let args = self.reglist_of(ins, &ops, 1);
                // 7.8/7.9 的字面量走 CallRuntime（`CreateArrayLiteralWithoutAllocationSite
                // <字面量>, <flags>`）—— 第一个实参已经是解好的字面量，别包成桩调用：
                // `__runtime.CreateArrayLiteralWithoutAllocationSite([1.5,-0], 37)`
                // 在裸 node 下是 undefined。
                if matches!(
                    name.as_str(),
                    "CreateArrayLiteral"
                        | "CreateArrayLiteralWithoutAllocationSite"
                        | "CreateObjectLiteral"
                        | "CreateObjectLiteralWithoutAllocationSite"
                ) {
                    self.acc = Some(args.first().cloned().unwrap_or(Expr::Hole));
                    self.acc_consumed();
                    return;
                }
                // 6.x async 的完成码分派里，数字名调用就是异步机器的结算：
                //   case 0 = `Mov outer, rX; Mov value, rY; CallJSRuntime [id], rX-rY` → `return 值`
                // （名字表解不出 id，只能靠"落在分派区间内"认）
                if let Some((ds, de)) = self.async_dispatch {
                    let here = self.idx_of.get(&ins.offset).copied();
                    if here.map(|h| h >= ds && h < de).unwrap_or(false)
                        && name.chars().all(|c| c.is_ascii_digit())
                    {
                        let v = args.get(1).or_else(|| args.last()).cloned().unwrap_or(Expr::Undefined);
                        self.line(&format!("return {};", v.render()));
                        self.acc = None;
                        self.acc_consumed();
                        return;
                    }
                }
                // **catch 体里的数字名结算**（机器 catch 处理器）：`RejectPromise(state, err)` —
                // 名字表解不出 id，但位置在 catch 体内 + 两个实参就认得出来。
                // 渲染成 `throw err;`：JS 的 `async` 关键字会把抛出变成 reject
                // （不这么做的话 settle 落在未实现的桩上 → 体里的异常被**吞掉**、
                // promise 反而 resolve；8.17 的 `async function f(){ throw … }` 就是这样）。
                if self.catch_alias.is_some() && name.chars().all(|c| c.is_ascii_digit()) && args.len() >= 2 {
                    let err = args.get(1).cloned().unwrap_or(Expr::Undefined);
                    self.line(&format!("throw {};", err.render()));
                    self.acc = None;
                    self.acc_consumed();
                    return;
                }
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
                let args = self.reglist_of(ins, &ops, 1);
                // async 的结算调用：`_AsyncFunctionResolve(state, v)` → `return v;`
                // （老族是 CallJSRuntime/CallRuntime，名字同样来自 runtime 表 → 一起处理）
                {
                    let args = self.reglist_of(ins, &ops, 1);
                    match name.as_str() {
                        // 实参：(状态, 值, …)：**第 2 个**才是结算值（7.8 还多一个
                        // "是否已捕获"的布尔尾巴，取 last 会把 `return false` 发出去）
                        "AsyncFunctionResolve" | "ResolvePromise" => {
                            let v = args
                                .get(1)
                                .or_else(|| args.last())
                                .cloned()
                                .unwrap_or(Expr::Undefined);
                            self.line(&format!("return {};", v.render()));
                            self.acc = None;
                            self.acc_consumed();
                            return;
                        }
                        "AsyncFunctionReject" | "RejectPromise" => {
                            let v = args
                                .get(1)
                                .or_else(|| args.last())
                                .cloned()
                                .unwrap_or(Expr::Undefined);
                            self.line(&format!("throw {};", v.render()));
                            self.acc = None;
                            self.acc_consumed();
                            return;
                        }
                        _ => {}
                    }
                }
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
            "CreateObjectLiteral" | "CreateArrayLiteral" => {
                // 对象字面量的常量池条目就是 BoilerplateDescription —— 只有这里能确定，
                // 所以打开 lit_ctx（顶层脚本的 DeclareGlobals 数组形状近似，靠它区分）
                let save_lit = self.d.lit_ctx.get();
                self.d.lit_ctx.set(true);
                let mut lit = idx_num(&arg(0)).map(|i| self.constant(i)).unwrap_or(Expr::Hole);
                // 6.2 的空字面量常量池条目就是 `EmptyFixedArray` 这个根
                // （bytecode-generator 的 EmptyFixedArrayConstantPoolEntry）——
                // 按字面量种类补成 `{}` / `[]`；不补的话展开目标/基底是 undefined。
                if let Some(i) = idx_num(&arg(0)) {
                    let is_empty_root = match self.d.cache.array_elem(
                        self.pool.unwrap_or(0),
                        i,
                    ) {
                        Some(crate::serializer::Elem::Ref(Ref::Root(r))) => {
                            let n = self.d.table.root_name(r).unwrap_or("");
                            n == "EmptyFixedArray"
                                || n == "EmptyBoilerplateDescription"
                                || n == "EmptyObjectBoilerplateDescription"
                        }
                        _ => false,
                    };
                    if is_empty_root {
                        lit = if base == "CreateObjectLiteral" {
                            Expr::ObjectLit(Vec::new())
                        } else {
                            Expr::ArrayLit(Vec::new())
                        };
                    }
                }
                self.d.lit_ctx.set(save_lit);
                // 6.2 的 CreateObjectLiteral 末位是 RegOut（`… #41, r1`），结果只进寄存器、
                // 不写 acc；后续靠 `Mov r1, r0` 搬走。不写寄存器的话那个 Mov 会把
                // 一个未初始化的 r1 赋出去（字面量整个丢失）。
                let reg_out = self
                    .d
                    .table
                    .bytecodes
                    .iter()
                    .find(|b| b.name == base)
                    .is_some_and(|b| b.operands.last().is_some_and(|o| o == "RegOut"));
                if reg_out {
                    // V8 的 ImplicitRegisterUse 是空（acc 既非读也非写）→ **不能**动累加器：
                    // `LdaConstant "x"; CreateObjectLiteral …, r5; Star r4` 里那条 Star 存的
                    // 是 "x"（键），写成字面量就把键整个顶掉（`"x" in {x:1}` 直接算错）。
                    let dst = reg_of(&arg(ops.len().saturating_sub(1)));
                    self.store_reg(dst, Some(lit));
                } else {
                    self.acc = Some(lit);
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
                let closure_sfi = idx.and_then(|i| {
                    self.pool
                        .and_then(|p| self.d.cache.array_elem(p, i))
                        .and_then(|e| e.as_ref())
                        .and_then(|r| match r {
                            Ref::Object(o)
                                if self.d.cache.obj(o).ty.is(self.d.table, "SharedFunctionInfo") =>
                            {
                                Some(o)
                            }
                            _ => None,
                        })
                });
                // 空名（箭头/嵌套函数）→ 该 SFI 的**全文件唯一**名（`function_emit_name`）；
                // 直接按原始名渲染会让重名的两个箭头指向同一个声明（`return nest` 曾
                // 指的是内层而不是这个闭包）。
                self.acc = Some(Expr::Ident(match closure_sfi {
                    Some(o) => self.d.function_emit_name(o),
                    None => anon_name(self.sfi),
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
                // 上下文本体在 acc 里（随后 `Star rN` 落进寄存器）—— 作用域记到 pending，
                // 由 Star/Mov 登记为 `reg_scope[rN]`（显式读取按它解析）。
                // 同时**照旧压栈**：不少形态（V8 14 的块）不发 PushContext，只靠这里的作用域
                // 才能给 `*CurrentContextSlot` 解出名字（撤掉压栈会让它们全变 `__ctx.ctxN`）。
                self.pending_ctx = scope_id;
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
                // 模板站点对象：常量池里是 TemplateObjectDescription = [cooked…, raw…]（各占一半）。
                // 以前给个空数组 → `String.raw\`a\nb\`` 读到空 raw（length 0），
                // 带标签模板也拿不到字符串。按 V8 的真实形状给出：数组 + raw 数组。
                let obj = idx_num(&arg(0)).and_then(|i| {
                    match self.pool.and_then(|p| self.d.cache.array_elem(p, i)) {
                        Some(Elem::Ref(Ref::Object(o))) => Some(o),
                        _ => None,
                    }
                });
                let mut cooked = Vec::new();
                let mut raw = Vec::new();
                if let Some(o) = obj {
                    // TemplateObjectDescription 是 Struct：槽 1 = raw_strings、槽 2 = cooked_strings
                    // （每个都是 FixedArray）—— 之前按 FixedArray 直接读，长度/元素全错。
                    let raw_arr = self.template_part(o, 1);
                    let cooked_arr = self.template_part(o, 2);
                    if let (Some(r), Some(c)) = (raw_arr, cooked_arr) {
                        let n = self.d.cache.array_len(c);
                        for k in 0..n.min(64) {
                            cooked.push(self.array_element_expr(c, k));
                        }
                        let n2 = self.d.cache.array_len(r);
                        for k in 0..n2.min(64) {
                            raw.push(self.array_element_expr(r, k));
                        }
                    } else {
                        // 退路：老版本/未知布局按一半一半读
                        let len = self.d.cache.array_len(o);
                        let half = len / 2;
                        for k in 0..half.min(64) {
                            cooked.push(self.array_element_expr(o, k));
                            raw.push(self.array_element_expr(o, half + k));
                        }
                    }
                }
                self.acc = Some(Expr::Call {
                    callee: Box::new(Expr::Member {
                        obj: Box::new(Expr::Ident("Object".into())),
                        key: Key::Ident("assign".into()),
                    }),
                    args: vec![
                        Expr::ArrayLit(cooked),
                        Expr::ObjectLit(vec![("raw".to_string(), Expr::ArrayLit(raw))]),
                    ],
                    is_new: false,
                    spread_arg: None,
                });
            }
            "GetIterator" | "GetAsyncIterator" => {
                // 注意用**计算键**：`obj[Symbol.iterator]()`；
                // `obj.Symbol.iterator` 是"名为 Symbol.iterator 的点属性"→ 取到 undefined
                let key = Key::Computed(Box::new(Expr::Ident(if base == "GetIterator" {
                    "Symbol.iterator".to_string()
                } else {
                    "Symbol.asyncIterator".to_string()
                })));
                let member = Expr::Member {
                    obj: Box::new(self.reg_expr(&arg(0))),
                    key,
                };
                // 语义随版本变：≤7.x 的 `GetIteratorWithFeedback` 只做
                // `GetProperty(receiver, @@iterator)`（torque: LoadIC）——**不调用**，
                // 调用是紧随其后的 `CallProperty0 <方法>, <receiver>`；
                // 8.x 起的 builtin 内部连调用一起做，直接给迭代器。
                // 判据就写在同一张表里：操作数 3 个（load+call 两个 feedback 槽）= 会调用。
                self.acc = Some(if self.d.table.bytecodes.iter().any(|b| {
                    b.name == base && b.operands.len() >= 3
                }) {
                    Expr::Call {
                        callee: Box::new(member),
                        args: Vec::new(),
                        is_new: false,
                        spread_arg: None,
                    }
                } else {
                    member
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
                // V8 语义：`ReThrow` 抛**累加器**（interpreter-generator.cc：
                // `exception = GetAccumulator()`）。字节码在它前面正好是 `Ldar <异常寄存器>`
                // （for-of 的 IteratorClose 收尾里异常在普通寄存器，如 r9/r12）。
                //
                // 早先固定渲染成 catch context 槽 0 的守卫（`if (__ctx.ctx0 !== undefined)
                // throw __ctx.ctx0;`）：那个槽在 for-of 收尾形态里**没人写** → 异常被吞、
                // 函数返回残值（`for (… ) { …; throw new Error("boom") }` 的产物返回 3）。
                // 累加器为空（没东西可抛）时才退回老守卫，避免发出 `throw undefined`。
                // V8 语义是"抛累加器"，但**不能**直接照搬：for-of 的 IteratorClose
                // 收尾里有一批 handler 区被线性发射（handler 区本该折进 catch），
                // 那些位置上的累加器/寄存器与"当前异常"无关（正常路径上也会走到），
                // 直接 `throw` 会把正常返回变成抛垃圾。守卫引用 catch context 槽 0：
                // 正经 catch 形态里由 CreateCatchContext 写入，读不到就说明这条路径
                // 本来不会到达（那些线性残留块由此变成安全的死代码）。
                //
                // 仍**未修**的问题（见 tests/fixtures/behav/iter_close.js，known_fail）：
                // 循环体抛错时这个守卫恒假 → 异常被吞、函数返回残值。正解是把 handler 区
                // 折进 catch 并把异常寄存器接进来（不该线性发射）。
                let name = self.context_name(0, true);
                self.line(&format!(
                    "if ({name} !== undefined) throw {name}; // rethrow（仅当有挂起异常）"
                ));
                self.acc = None;
            }
            "ThrowReferenceErrorIfHole" => {
                // V8 的形态：`LdaContextSlot/LdaCurrentContextSlot …; ThrowReferenceErrorIfHole`
                // —— 紧跟 context 读取的这条是 TDZ 检查。我们把这些 context 变量
                // 摊平成文件级 `var`（值由后续赋值给出），`var` 初始就是 undefined，
                // 于是这条检查会**必然误报**（node12 的 closure 就死在
                // `if (n === undefined) throw ...`）。跳过它：acc 只被读，不丢值。
                let prev_is_ctx = matches!(
                    self.prev_base.as_str(),
                    "LdaContextSlot"
                        | "LdaImmutableContextSlot"
                        | "LdaScriptContextSlot"
                        | "LdaCurrentContextSlot"
                        | "LdaImmutableCurrentContextSlot"
                );
                if prev_is_ctx {
                    return;
                }
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
            // 生成器重写后的产物（见 plan_generator）：状态机外壳已折掉
            "__gskip" => {}
            // 6.8 async 的"完成码 0"出口：就地发 `return <值>;`（见 plan_async 的折叠）
            "__greturn" => {
                let idx = self.idx_of.get(&ins.offset).copied();
                let rv = idx
                    .and_then(|k| self.async_returns.get(&k).cloned())
                    .unwrap_or_else(|| "undefined".into());
                let e = self.operand_expr(&rv);
                let t = Self::render_stmt(&e);
                self.line(&format!("return {t};"));
                self.acc = None;
                self.acc_consumed();
            }
            // async 的挂起点：发 `rK = await <值>;`（值取 await 调用的第二个实参寄存器）
            "__gawait" => {
                if std::env::var("JSCD_DBG_ASYNC").is_ok() {
                    eprintln!("[async-emit] @{} idx={:?}", ins.offset, self.idx_of.get(&ins.offset));
                }
                let idx = self.idx_of.get(&ins.offset).copied();
                let value = idx
                    .and_then(|k| self.gen_yields.get(&k).cloned())
                    .flatten()
                    .map(|t| self.operand_expr(&t))
                    .or_else(|| self.acc.take())
                    .unwrap_or(Expr::Undefined);
                let e = Expr::Await(Box::new(value));
                let store = idx.and_then(|k| self.gen_yield_store.get(&k).cloned());
                if let Some(r) = store {
                    self.store_named(&r, e);
                    self.acc = None;
                    self.acc_consumed();
                } else {
                    self.acc = Some(e);
                    self.acc_stored = false;
                    self.acc_stored_reg = None;
                }
            }
            "__gyield" | "__gyield_stmt" => {
                let idx = self.idx_of.get(&ins.offset).copied();
                let value = idx
                    .and_then(|k| self.gen_yields.get(&k).cloned())
                    .flatten()
                    .map(|t| self.operand_expr(&t))
                    .or_else(|| self.acc.take())
                    .unwrap_or(Expr::Undefined);
                let e = Expr::Yield(Box::new(value));
                let store = idx.and_then(|k| self.gen_yield_store.get(&k).cloned());
                if let (Some(r), "__gyield") = (store, base.as_str()) {
                    // 6.2：恢复值的 Star 在续体 shim 里（已被折叠掉）→ 在这里落地
                    self.store_named(&r, e);
                    self.acc = None;
                    self.acc_consumed();
                } else if base == "__gyield" {
                    // 表达式形：恢复值由紧随的 Star 落进寄存器（`r = yield v;`）
                    self.acc = Some(e);
                    self.acc_stored = false;
                    self.acc_stored_reg = None;
                } else {
                    let t = Self::render_stmt(&e);
                    self.line(&format!("{t};"));
                    self.acc = None;
                    self.acc_consumed();
                }
            }
            "__gyieldstar" => {
                let d = self
                    .idx_of
                    .get(&ins.offset)
                    .copied()
                    .and_then(|k| self.gen_delegates.get(&k).cloned());
                if let Some((iter, store)) = d {
                    let iterable = self.operand_expr(&iter);
                    self.store_named(&store, Expr::YieldStar(Box::new(iterable)));
                }
                self.acc = None;
                self.acc_consumed();
            }

            // ── switch ──
            "SwitchOnSmiNoFeedback" => {
                if let Some(j) = self.emit_switch(ins, &ops) {
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
            // TestUndetectable：判断"不可检测"值（undefined / null / document.all）。
            // 6.x 带寄存器操作数，9.x 起改成只测累加器（`TestUndetectable` 无操作数）——
            // 按 arg(0) 取会解出空串→undefined，`return === undefined` 于是恒真，
            // IteratorClose 的守卫整个失效（for_of_in 直接调 undefined.return）。
            "TestUndetectable" => {
                let l = if arg(0).trim().is_empty() {
                    self.acc.clone().unwrap_or(Expr::Undefined)
                } else {
                    self.operand_expr(&arg(0))
                };
                self.acc = Some(Expr::Bin {
                    op: "==",
                    l: Box::new(l),
                    r: Box::new(Expr::Null),
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
                // **留在 acc 里**：`delete` 是有值的表达式（布尔），后面常有 `Star` 存它
                // （`const r = delete o.a` → 旧实现直接发语句并清 acc，读处拿到 undefined）。
                // 值没人要时由 acc flush 机制落成语句：所以 `has_effect` 必须认 delete。
                self.acc = Some(Expr::Un {
                    op: "delete ",
                    e: Box::new(target),
                    postfix: false,
                });
            }
            "GetSuperConstructor" => {
                self.acc = Some(Expr::Ident("Object.getPrototypeOf(this)".to_string()));
            }

            // ── for-in ──
            // for-in 的键枚举/推进协议还没重建：占位成一个会抛错的取值，
            // 让产物**立刻**以清晰消息失败，而不是 ReferenceError 或死循环。
            "ForInEnumerate" | "ForInPrepare" | "ForInNext" | "ForInStep" | "ForInContinue"
            | "JumpIfForInDone" | "JumpIfForInDoneConstant" => {
                if base == "ForInStep" || base == "ForInContinue" {
                    // 推进/判断：给一个"继续"的值，避免把循环卡死
                    self.acc = Some(Expr::Bool(true));
                } else {
                    self.acc = Some(Expr::Call {
                        callee: Box::new(Expr::Ident("__forin_unsupported".into())),
                        args: Vec::new(),
                        is_new: false,
                        spread_arg: None,
                    });
                }
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
                // 保守假设：可能写 acc。占位必须**跑得起来**：裸标识符（旧写法
                // `__unknown_X`）会 ReferenceError（node12 的 for-of 里一条
                // StackCheck 就把它存进寄存器、随后被求值）；`__runtime.X` 是
                // 万能桩（未知名退化成空函数），取值/调用都不炸。
                if !matches!(base.as_str(), "Jump" | "Star") {
                    self.acc = Some(Expr::Member {
                        obj: Box::new(Expr::Ident("__runtime".into())),
                        key: Key::Ident(base.to_string()),
                    });
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
        self.reg_prop.remove(&r);
        if self.regs.len() <= r as usize {
            self.regs.resize(r as usize + 1, None);
        }
        // 寄存器一律落成变量：内联字面量会在寄存器被改写后失真
        // （曾导致 `i++` 变成 `r1 = 0 + 1` 的死循环）。寄存器机语义 = 变量语义。
        let name = self.reg_display(r);
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
            // 脚本体被铺平后自身没有名字 → 用已声明的 __anonymous 占位
            return Expr::Ident(if self.inline_body {
                "__anonymous".to_string()
            } else {
                self.flat_name()
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

    /// RegList 操作数展开（"r5-r5" / "r1-r3" / 单个 "r0"）→ 表达式列表。
    /// 精确展开寄存器组：**名字从文本取**（≤8.4 的参数寄存器在文本里是 `a0` 这种参数相对
    /// 命名，用原始下标硬拼会得到 `a-7`），**个数用解析后的 RegCount** —— 文本形态 `r0-r0`
    /// 在 `count == 0` 时是 V8 的占位写法，分不出"0 个"和"1 个"（`new O()` 多一个实参、
    /// 0 实参的调用多一个参数都是这么来的）。
    fn reglist_of(&mut self, ins: &Instr, ops: &[String], idx: usize) -> Vec<Expr> {
        if let Some(Operand::RegList { count, .. }) = ins.operands.get(idx) {
            if *count == 0 {
                return Vec::new();
            }
        }
        self.reglist_exprs(ops, idx)
    }

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
                        if join.is_none_or(|c| jt > c) {
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


/// 把 `map` 里的名字（寄存器/临时）在文本里按标识符边界替换掉（长的先换，避免前缀撞车）。
fn substitute_names(text: &str, map: &HashMap<String, String>) -> String {
    let mut keys: Vec<&String> = map.keys().collect();
    keys.sort_by_key(|k| std::cmp::Reverse(k.len()));
    let mut out = text.to_string();
    for k in keys {
        out = replace_ident_boundary(&out, k, &map[k]);
    }
    out
}

/// 按标识符边界替换（不会把 `r1` 换进 `r12` / `r1x`）。
fn replace_ident_boundary(text: &str, needle: &str, repl: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0usize;
    while let Some(pos) = text[i..].find(needle) {
        let at = i + pos;
        let before_ok = at == 0 || !is_ident_byte(bytes[at - 1]);
        let after = at + needle.len();
        let after_ok = after >= bytes.len() || !is_ident_byte(bytes[after]);
        out.push_str(&text[i..at]);
        if before_ok && after_ok {
            out.push_str(repl);
        } else {
            out.push_str(needle);
        }
        i = after;
    }
    out.push_str(&text[i..]);
    out
}

/// 文本里是否还残留 `rN`/`tN` 形态的未解析名（typo 防护：默认值搬进签名后不能引用局部寄存器）。
fn references_unknown_reg(text: &str) -> bool {
    let bytes = text.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        let c = bytes[i] as char;
        if (c == 'r' || c == 't') && (i == 0 || !is_ident_byte(bytes[i - 1])) {
            let mut j = i + 1;
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                j += 1;
            }
            if j > i + 1 && (j >= bytes.len() || !is_ident_byte(bytes[j])) {
                return true;
            }
        }
        i += 1;
    }
    false
}

fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'$'
}

fn cmp_of(name: &str) -> &'static str {
    match name {
        "TestEqual" => "==",
        "TestEqualStrict" | "TestEqualStrictNoFeedback" | "TestReferenceEqual" => "===",
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
fn read_handler_table<'a>(
    d: &Decompiler<'a>,
    bca: ObjId,
    code_len: usize,
    idx_of: &HashMap<usize, usize>,
) -> Vec<Handler> {
    let ts = d.ts;
    let Some(h) = d
        .cache
        .slot_at(bca, d.dis.bca_handler_table_slot())
        .and_then(|v| v.as_ref())
        .and_then(|r| d.cache.ref_object(r))
    else {
        return Vec::new();
    };
    // HandlerTable 是 ByteArray：头部占 2 个 tagged 槽（map + 长度），
    // 长度是第二个槽里的**高 32 位**（指针压缩下 Smi 只占 4 字节）。
    // 之前把表头当成了条目起点、又按 Smi 解长度 —— 于是整个表解析成空，
    // try/catch 从来没能重建出来。
    // 长度位于 tag 槽，但**落在哪半随版本变**：9.x–12.x 在高 32 位，13.x 搬到低 32 位。
    // 两半都取，选像长度的那个（≤ 对象字节数）—— 13.6 上按高 32 位读会得到 0，
    // 于是 node24 的 try/catch 完全重建不出来。
    let obj_bytes = d.cache.obj(h).byte_size;
    if std::env::var("JSCD_DBG_HAND").is_ok() {
        let mut hex = String::new();
        for k in 0..(obj_bytes / ts).min(20) {
            if let Some(w) = d.cache.raw_at(h, k * ts, ts) {
                for b in w { hex.push_str(&format!("{b:02x}")); }
                hex.push(' ');
            } else { hex.push_str("???????????????? "); }
        }
        eprintln!("[hand-hdr] o={h} obj_bytes={obj_bytes} ty={}", d.cache.obj(h).ty.name(d.table));
        eprintln!("[hand-hdr] {hex}");
        let slots_n = obj_bytes / ts;
        for k in 0..slots_n {
            if let Some(w) = d.cache.raw_at(h, k * ts, ts) {
                if w.len() == 8 {
                    let v = u64::from_le_bytes(w.try_into().unwrap());
                    eprintln!("[hand-hdr]   slot{k}: raw={v:#018x} hi={} lo={}", (v >> 32) as u32 as i64, (v & 0xffff_ffff) as u32 as i64);
                }
            }
        }
    }
    let len = match d.cache.raw_at(h, ts, 8.min(ts)) {
        Some(x) if x.len() == 8 => {
            let w = u64::from_le_bytes(x.try_into().unwrap());
            let hi = (w >> 32) as u32 as usize;
            let lo = (w & 0xffff_ffff) as u32 as usize;
            if hi <= obj_bytes && (lo > obj_bytes || hi >= lo) {
                hi
            } else {
                lo
            }
        }
        Some(x) => u32::from_le_bytes(x.try_into().unwrap()) as usize,
        None => 0,
    };
    // 表长语义随版本变：9.x+ 的 `len` 是**字节数**，6.2 是**槽数** —— 两个都别用，
    // 直接按对象字节数取槽（map + length + data）。老实现按 `2 + len/8 + 1` 取，
    // node8 上只读到 4 个槽 → 整个表解析成空、try/catch 从来重建不出来。
    let slots = (obj_bytes / ts).max(2);
    if obj_bytes < 2 * ts + 8 {
        return Vec::new();
    }
    // 逐槽读取（每个槽是独立的 Raw 段，跨段读会失败）：
    // slot0 = map 的两半，slot1 = 长度（高 32 位），其余每槽两个 int32 字段。
    let mut words: Vec<i32> = Vec::new();
    for k in 0..slots {
        // 非 Raw 段（如 map 字段是引用）不能中断解析，否则索引错位
        match d.cache.raw_at_ts(h, k * ts, ts, ts) {
            Some(w) => {
                let mut c = 0;
                while c + 4 <= w.len() {
                    words.push(i32::from_le_bytes(w[c..c + 4].try_into().unwrap()));
                    c += 4;
                }
            }
            None => words.resize(words.len() + (ts / 4).max(1), 0),
        }
    }
    if std::env::var("JSCD_DBG_HAND").is_ok() {
        let mut hex = String::new();
        for k in 0..slots {
            if let Some(w) = d.cache.raw_at_ts(h, k * ts, ts, ts) {
                hex.push_str(&format!("[{k}]"));
                for b in w { hex.push_str(&format!("{b:02x}")); }
                hex.push(' ');
            }
        }
        eprintln!("[hand-raw] {hex}");
        eprintln!("[hand] bca={bca} len={len} obj_bytes={obj_bytes} words={words:?}");
    }
    // 数据区的词序随版本变：
    //   6.2（无指针压缩，Smi = value<<32）：**每槽一个值** → 低半恒 0、值在高半；
    //   6.8/9.x+：值按 int32 紧密排布（一槽两个值）。
    // 判据：数据区偶数位词恒为 0、奇数位不全为 0 → 换成"每槽一个值"的压缩视图，
    // 否则 6.2 的条目会整体错位（start=0、end=4 这种），try/catch 重建不出来。
    {
        let data: Vec<i32> = words.iter().skip(4).copied().collect();
        let even_all_zero = data.iter().step_by(2).all(|w| *w == 0);
        let odd_any = data.iter().skip(1).step_by(2).any(|w| *w != 0);
        if even_all_zero && odd_any {
            let compact: Vec<i32> = data.iter().skip(1).step_by(2).copied().collect();
            words.truncate(4);
            words.extend(compact);
        }
    }
    let f = |i: usize| -> Option<i32> { words.get(4 + i).copied() };
    let mut out = Vec::new();
    // 条目：[start, end, handler, data]（各 1 个 int32，连续排布）
    let avail = words.len().saturating_sub(4);
    let mut i = 0usize;
    while i + 4 <= avail {
        let (Some(start), Some(end), Some(handler)) = (f(i), f(i + 1), f(i + 2)) else {
            break;
        };
        let _ = len;
        // handler 字段是"偏移 << shift"，shift 随版本变：9.x–12.x 为 3，13.x 为 4
        // （13.6 实测：1200>>4=75 正好等于该条目 end，>>3 则越界）→
        // 用"必须落在字节码长度内"自校验，两种移位取合法的那个。
        let h3 = (handler as u32) >> 3;
        let h4 = (handler as u32) >> 4;
        // 判据：解出来的偏移必须**落在一条指令的起点**上（idx_of 里有），
        // 两个都合法时取"在区间终点之后、离终点最近"的那个。
        // 早先只按"是否等于 end"猜移位 —— 13.6 的 handler 落在 end 之后几条指令处
        // （191 → 197），于是猜错移位（394，正好是 2 倍）把 catch 区间整个吞掉，
        // node24 的对象解构代码直接消失。
        let in_code = |v: u32| idx_of.contains_key(&(v as usize));
        let target = match (in_code(h3), in_code(h4)) {
            (true, false) => h3,
            (false, true) => h4,
            (true, true) => {
                let after = |v: u32| (v as i64) - (end as i64) >= 0;
                let dist = |v: u32| ((v as i64) - (end as i64)).abs();
                match (after(h3), after(h4)) {
                    (true, false) => h3,
                    (false, true) => h4,
                    _ => {
                        if dist(h4) <= dist(h3) {
                            h4
                        } else {
                            h3
                        }
                    }
                }
            }
            (false, false) => {
                if code_len > 0 && h3 as usize > code_len && h4 as usize <= code_len {
                    h4
                } else {
                    h3
                }
            }
        };
        out.push(Handler {
            start: start as u32,
            end: end as u32,
            target,
        });
        i += 4;
    }
    out
}

/// 指令基名（去掉 `.Wide`/`.ExtraWide` 后缀与操作数点号）。
fn ins_base(ins: &Instr) -> String {
    ins.name.split('.').next().unwrap_or(&ins.name).to_string()
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
        // 生成器/异步函数的声明头带 `*` 与 `async`（`function* count(…)`）——
        // 只认 "function " 会漏掉它们，DeclareGlobals 里的引用于是留在 __uncompiled 占位
        // （generator fixture 的 `pair(n, 99)` 调的是空实现 → 整个循环一次都不跑）。
        // 声明名的前缀（生成器/异步/类都要认 —— 之前只认 "function "，"async function"
        // 顶格时收不到名字，DeclareGlobals 表里的 `__uncompiled.noAwait` 就永远换不掉）。
        for kw in [
            "async function* ",
            "async function ",
            "function* ",
            "function ",
            "class ",
            "var ",
            "let ",
            "const ",
        ] {
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

/// 参数槽的显示名（`a0` / `a12`）—— 老族与 9.x+ 的参数寄存器在文本里都长这样，
/// 但字节码里是**负下标**，用下标解析会串到局部寄存器上。
fn param_slot_name(text: &str) -> Option<String> {
    let t = text.trim().trim_matches(['[', ']']);
    let rest = t.strip_prefix('a')?;
    if rest.is_empty() || !rest.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(t.to_string())
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
            | "StaScriptContextSlot" | "StaGlobal" | "StaGlobalSloppy" | "StaGlobalStrict" | "StaLookupSlot"
            | "StaNamedProperty" | "SetNamedProperty" | "StaNamedOwnProperty"
            | "DefineNamedOwnProperty" | "StaKeyedProperty" | "SetKeyedProperty"
            | "StaNamedPropertySloppy" | "StaNamedPropertyStrict"
            | "StaKeyedPropertySloppy" | "StaKeyedPropertyStrict"
            | "StaDataPropertyInLiteral" | "DefineKeyedOwnPropertyInLiteral"
            | "StaInArrayLiteral" | "DefineKeyedOwnProperty" | "CollectTypeProfile"
    ) || name.starts_with("Star") && name[4..].chars().all(|c| c.is_ascii_digit())
}
