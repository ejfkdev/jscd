//! 产物文本的保守优化：把字节码的"寄存器搬运"还原成人读的样子。
//!
//! 反编译器是**逐指令**翻译的，于是产物里到处是 `r3 = greet; r4 = "world"; r3 = r3(r4);`
//! 这种三行胶水。这里做一轮纯文本的后处理（输出形态很规整：一行一条语句），规则如下：
//!
//! 1. `let r0, r1, …;` 里**没被读到**的寄存器删掉；一个都不剩就整行删掉。
//! 2. `rX = <纯表达式>;` 且这次写的值**只被读一次** → 把表达式搬到读处、删掉这条。
//! 3. **带副作用的表达式也允许搬**，但仅当"赋值"与"读取"之间没有任何带副作用的语句
//!    —— 这样副作用发生的先后顺序不变（`r2 = r0(); return r2 + r0();` →
//!    `return r0() + r0();`）。
//! 4. 写了之后**从未被读**、且右侧是纯表达式的语句直接删（死存储）。
//! 5. `let r0; … r0 = E;`（同块、全区域只出现这两次）→ `let r0 = E;`：原地提升成
//!    声明器初始化。**不搬运、不换序**，纯粹给 AST 层（[`crate::opt_js`]）铺路 ——
//!    swc 的复制传播只认声明器初始化，不认"先空声明、后赋值"。
//!
//! "纯"取保守面：字面量、裸标识符、另一个寄存器。**成员访问（`a.b`）算有副作用**
//! （可能有 getter），调用/`new`/`++`/赋值更是 —— 于是 `r1 = r2.log;` 这种不会被删，
//! 宁可少优化，不可改语义。之后产物再交给 AST 级那一轮（[`crate::opt_js`]，借 swc 做
//! 复制传播），整轮跑完还要过结构门禁（`syntax_check`）；行为矩阵（26 fixture × N 版本）
//! 是最终裁判。

use std::collections::{HashMap, HashSet};

/// 一行语句的形状。
#[derive(Debug, Clone)]
enum Line {
    /// `let r0, r1, …;`
    Decl(Vec<usize>),
    /// `rX = <rhs>;`（rhs 可能为空串，例如 `rX = ;` 不会出现，但保守留空分支）
    Store { reg: usize, rhs: String },
    Other(String),
}

fn parse(line: &str) -> Line {
    let t = line.trim();
    if let Some(rest) = t.strip_prefix("let ") {
        if let Some(list) = rest.strip_suffix(';') {
            let mut regs = Vec::new();
            let mut ok = true;
            for part in list.split(',') {
                match reg_name(part.trim()) {
                    Some(r) => regs.push(r),
                    None => {
                        ok = false;
                        break;
                    }
                }
            }
            if ok && !regs.is_empty() {
                return Line::Decl(regs);
            }
        }
    }
    if t.ends_with(';') {
        if let Some((lhs, rhs)) = t.strip_suffix(';').unwrap_or(t).split_once('=') {
            let lhs = lhs.trim();
            // `==`/`>=`/`<=`/`=>` 之类不是赋值
            if !lhs.is_empty() && !rhs.starts_with('=') && !lhs.ends_with(['!', '<', '>', '=']) {
                if let Some(reg) = reg_name(lhs) {
                    return Line::Store {
                        reg,
                        rhs: rhs.trim().to_string(),
                    };
                }
            }
        }
    }
    Line::Other(line.to_string())
}

impl Line {
    /// 这一行里"表达式"部分的文本（供文本匹配用；声明行没有表达式）。
    fn text(&self) -> &str {
        match self {
            Line::Decl(_) => "",
            Line::Store { rhs, .. } => rhs,
            Line::Other(t) => t,
        }
    }
}

/// 裸寄存器名（`r12`）→ 序号；别的都返回 None。
fn reg_name(s: &str) -> Option<usize> {
    let n = s.strip_prefix('r')?;
    if n.is_empty() || !n.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    n.parse().ok()
}

/// 这一行里出现的寄存器（去掉赋值目标本身），带出现次数。
fn reads_in(line: &Line) -> HashMap<usize, usize> {
    let mut out = HashMap::new();
    let text = match line {
        Line::Decl(_) => return out,
        Line::Store { rhs, .. } => rhs.as_str(),
        Line::Other(t) => t.as_str(),
    };
    let mut cur = String::new();
    let flush = |cur: &mut String, out: &mut HashMap<usize, usize>| {
        if !cur.is_empty() {
            if let Some(r) = reg_name(cur) {
                *out.entry(r).or_insert(0) += 1;
            }
            cur.clear();
        }
    };
    for ch in text.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' || ch == '$' {
            cur.push(ch);
        } else {
            flush(&mut cur, &mut out);
        }
    }
    flush(&mut cur, &mut out);
    out
}

/// 表达式的"可搬运等级"：越高越能自由前送。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Purity {
    /// 字面量 / 寄存器读：值不会变，搬到哪儿都安全（只要不越过对同一个寄存器的写）
    Stable,
    /// 裸标识符（`console`、`greet`）：值理论上可被别的语句改写 → 只允许**紧邻**搬
    Ident,
    /// 调用 / 成员读 / `new` / 自增…：有副作用 → 只允许紧邻搬，且**行内插入点之前必须无副作用**
    Effectful,
}

fn purity(rhs: &str) -> Purity {
    let t = strip_comments(rhs).trim().to_string();
    let t = t.as_str();
    if t.is_empty() {
        return Purity::Effectful;
    }
    if reg_name(t).is_some() {
        return Purity::Stable;
    }
    if matches!(t, "undefined" | "null" | "true" | "false" | "NaN" | "Infinity") {
        return Purity::Stable;
    }
    let b = t.as_bytes();
    if b[0].is_ascii_digit() || (b[0] == b'-' && b.len() > 1 && b[1].is_ascii_digit()) {
        return if t.bytes().all(|c| {
            c.is_ascii_digit() || c == b'.' || c == b'-' || c == b'+' || c == b'e' || c == b'x' || c == b'_'
        }) {
            Purity::Stable
        } else {
            Purity::Effectful
        };
    }
    if (b[0] == b'"' || b[0] == b'\'') && t.len() >= 2 && t.ends_with(b[0] as char) {
        return Purity::Stable; // 字符串字面量
    }
    // 数组字面量：元素都是稳定值时，整体只分配、无副作用（`[/* function greet */ greet, 0]`
    // 这种 DeclareGlobals 的参数就是它）
    if t.starts_with('[') && t.ends_with(']') {
        let inner = &t[1..t.len() - 1];
        let mut depth = 0i32;
        let mut cur = String::new();
        let mut elems = Vec::new();
        for ch in inner.chars() {
            match ch {
                '[' | '(' | '{' => depth += 1,
                ']' | ')' | '}' => depth -= 1,
                ',' if depth == 0 => {
                    elems.push(std::mem::take(&mut cur));
                    continue;
                }
                _ => {}
            }
            cur.push(ch);
        }
        if !cur.trim().is_empty() {
            elems.push(cur);
        }
        return if elems
            .iter()
            .all(|e| matches!(purity(e), Purity::Stable | Purity::Ident))
        {
            Purity::Stable
        } else {
            Purity::Effectful
        };
    }
    let mut it = t.chars();
    match it.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' || c == '$' => {}
        _ => return Purity::Effectful,
    }
    if it.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$') {
        Purity::Ident
    } else {
        Purity::Effectful
    }
}

/// 去掉行内注释（`/* … */` 与 `// …`）——纯度判定不该被注释里的字带偏。
fn strip_comments(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
            match s[i + 2..].find("*/") {
                Some(p) => {
                    i += 2 + p + 2;
                    out.push(' ');
                    continue;
                }
                None => break,
            }
        }
        if bytes[i] == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
            break;
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

/// 表达式是否有副作用（保守：拿不准就算有）。
fn pure_expr(rhs: &str) -> bool {
    purity(rhs) != Purity::Effectful
}

/// 在一行里把**恰好一次**出现的 `rX` 换掉（用于把值搬进读处）。
fn replace_reg(line: &str, reg: usize, replacement: &str) -> Option<String> {
    let needle = format!("r{reg}");
    // 按标识符边界找
    let mut hits = Vec::new();
    let bytes = line.as_bytes();
    let mut i = 0;
    while let Some(pos) = line[i..].find(&needle) {
        let at = i + pos;
        let before_ok = at == 0 || !is_ident_byte(bytes[at - 1]);
        let after = at + needle.len();
        let after_ok = after >= bytes.len() || !is_ident_byte(bytes[after]);
        if before_ok && after_ok {
            hits.push(at);
        }
        i = at + needle.len();
    }
    if hits.len() != 1 {
        return None;
    }
    let at = hits[0];
    // 粘连保护：替换值插进去不能和邻居拼成别的 token。
    //   `r2-r7` 里把 `-1` 换进去 → `r2--1`（`--` 自减，语法错）
    //   `r0.toString()` 里把 `1` 换进去 → `1.toString()`（数字后跟点，语法错）
    let before = line[..at].chars().last();
    let after = line[at + needle.len()..].chars().next();
    let first = replacement.chars().next();
    let last = replacement.chars().last();
    let glue_left = matches!((before, first), (Some('-'), Some('-')) | (Some('+'), Some('+')));
    let glue_right = matches!((last, after), (Some(c), Some('.')) if c.is_ascii_digit());
    let text = if glue_left || glue_right {
        format!("({replacement})")
    } else {
        replacement.to_string()
    };
    Some(format!("{}{}{}", &line[..at], text, &line[at + needle.len()..]))
}

fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'$'
}

/// 是不是一个"简单名字"（标识符或寄存器名，不含点/括号/问号等）。
fn is_simple_ident(s: &str) -> bool {
    let mut b = s.bytes();
    match b.next() {
        Some(c) if c.is_ascii_alphabetic() || c == b'_' || c == b'$' => {}
        _ => return false,
    }
    b.all(is_ident_byte)
}

/// 文本里是否出现标识符 `name`（按标识符边界，`r1` 不匹配 `r10`）。
fn mentions_ident(text: &str, name: &str) -> bool {
    let bytes = text.as_bytes();
    let mut i = 0;
    while let Some(pos) = text[i..].find(name) {
        let at = i + pos;
        let before_ok = at == 0 || !is_ident_byte(bytes[at - 1]);
        let after = at + name.len();
        let after_ok = after >= bytes.len() || !is_ident_byte(bytes[after]);
        if before_ok && after_ok {
            return true;
        }
        i = at + name.len();
    }
    false
}

/// ⑥ 渲染副产品：`rX = A.B;` 是喂给 `CallPropertyN` 的**方法加载寄存器**——调用那行会被
/// 渲染成 `A.B(args)`（带接收者、属性只读一次），于是这条加载就成了"值为死、却让 getter
/// 多跑一次"的残留。删掉它才和原始源码一致（原始源码里属性只读一次）。
///
/// 只在**可证明没人用**时删：`rX` 在整个函数区域里一次都没被读，且后面确实有同一个成员
/// 表达式的调用，中间 `A` 没被改写（`A` 是寄存器 → 中间没有写它的语句；是普通标识符 →
/// 中间连提都没提过）。判不准就不删。
fn call_scratch_lines(
    parsed: &[Line],
    lines: &[String],
    region: &[usize],
) -> HashSet<usize> {
    let mut drop_idx = HashSet::new();
    for (i, l) in parsed.iter().enumerate() {
        let Line::Store { reg, rhs } = l else { continue };
        let expr = rhs.trim();
        // 只看"裸成员读"：`A.B`（`A` 是寄存器或标识符，`B` 是标识符）
        let Some((obj, prop)) = expr.split_once('.') else { continue };
        if !is_simple_ident(obj) || !is_simple_ident(prop) {
            continue;
        }
        // 这次装载的**值**必须是死的：到下一次写这个寄存器（或出了这个函数区域）之前没人读它。
        // （不能要求"整个区域都没人读" —— 寄存器是复用的：`r1 = O; new r1(); …; r1 = r2.log;`
        // 里 r1 前面被读过，那和这次装载的值无关。）
        let mut live = false;
        for k in i + 1..parsed.len() {
            if region[k] != region[i] {
                break;
            }
            if matches!(&parsed[k], Line::Store { reg: r2, .. } if r2 == reg) {
                break; // 被覆盖 → 这次的值到此为止
            }
            if reads_in(&parsed[k]).contains_key(reg) {
                live = true;
                break;
            }
        }
        if live {
            continue;
        }
        let call = format!("{expr}(");
        let obj_reg = reg_name(obj);
        // 调用行可能把结果写进寄存器（`r4 = r5.m(…)` 是 Store）→ 按**原始行文本**找，
        // 不能只看 `Line::Other`（那样会漏掉最常见的一类）。
        let Some(j) = (i + 1..lines.len()).find(|j| {
            region[*j] == region[i] && lines[*j].contains(&call)
        }) else {
            continue;
        };
        // 中间不许改写对象本身
        let interfered = (i + 1..j).any(|k| {
            match obj_reg {
                Some(r) => matches!(&parsed[k], Line::Store { reg, .. } if *reg == r),
                None => mentions_ident(parsed[k].text(), obj),
            }
        });
        if interfered {
            continue;
        }
        drop_idx.insert(i);
    }
    drop_idx
}

/// 主入口：跑若干轮直到不再变化。
pub fn optimize(text: &str) -> String {
    // 调试：`JSCD_NO_OPT=1` 关掉优化，便于和未优化的产物对拍
    if std::env::var_os("JSCD_NO_OPT").is_some() {
        return text.to_string();
    }
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    let keep_ending = text.ends_with('\n');
    // `DeclareGlobals` 是 V8 的簿记（把顶层函数名登记到全局对象）：源码里没有对应写法，
    // 我们的前导里它本来就是空实现 → 直接删掉这一行（参数随之变成没人读，交给死存储规则）。
    lines.retain(|l| {
        let t = l.trim();
        !(t.starts_with("__runtime.DeclareGlobals(")
            || t.starts_with("__runtime.DeclareGlobalsForInterpreter("))
    });
    // **只跑一趟**：所有判断都基于原始文本。多趟会让后面的判断看到"已经被删掉的行"，
    // 于是又把共享寄存器搬错（实测 try_catch：handler 还要读的 r1 被判成"之后没人读"）。
    // 想再进一步就得做真正的控制流分析 —— 那是另一个量级的事，这里宁少勿错。
    let dbg = std::env::var("JSCD_DBG_OPT").is_ok();
    let dump = |tag: &str, ls: &[String]| {
        if dbg {
            eprintln!("[opt] ── {tag} ──");
            for (i, l) in ls.iter().enumerate() {
                eprintln!("[opt] {i:4} {}", l);
            }
        }
    };
    dump("in", &lines);
    prune_dead_temps(&mut lines);
    optimize_once(&mut lines);
    dump("after optimize_once", &lines);
    // ⑧ 死 phi 清理（它挡着 ⑦ 的扫描路径）
    prune_dead_phis(&mut lines);
    dump("after prune_dead_phis", &lines);
    // ⑦ 表达式重建：把"算进寄存器、随后只读一次"的赋值并回调用点
    //    （`r2 = console; r3 = greet("world"); r2.log(r3);` → `console.log(greet("world"))`）
    rebuild_calls(&mut lines);
    dump("after rebuild_calls", &lines);
    prune_decls(&mut lines);
    dump("after prune_decls", &lines);
    // ⑤ 空声明 + 之后的一次赋值 → 声明器初始化（给 AST 层铺路；本身也是可读性提升）
    bind_declarators(&mut lines);
    dump("after bind_declarators", &lines);
    if dbg {
        eprintln!("[opt] input was:\n{}", text);
    }
    let mut out = lines.join("\n");
    if keep_ending && !out.ends_with('\n') {
        out.push('\n');
    }
    out
}

/// 每行是否位于"循环体"内（`while`/`for`/`do` 括起来的块）。
///
/// 循环体里的寄存器赋值**不能**做前送/删死存储：这次写的值可能要到下一轮迭代才被读
/// （文本上"之后再没读到"，运行时却读得到）→ 宁可不动。
fn inside_loop(lines: &[String]) -> Vec<bool> {
    let mut out = vec![false; lines.len()];
    let mut stack: Vec<bool> = Vec::new(); // 每层块：是不是循环块
    let mut depth = 0usize;
    let mut in_loop = 0usize;
    for (i, l) in lines.iter().enumerate() {
        out[i] = in_loop > 0;
        let t = l.trim();
        let opens_loop = t.starts_with("while (")
            || t.starts_with("while(")
            || t.starts_with("for (")
            || t.starts_with("for(")
            || t == "do {"
            || t.ends_with(" do {")
            || t.contains(": while (")
            || t.contains(": for (");
        let opens = t.ends_with('{');
        if opens {
            depth += 1;
            stack.push(opens_loop);
            if opens_loop {
                in_loop += 1;
            }
        }
        if t.contains('}') {
            // 简化：一行里的闭括号按层数退栈
            let closes = t.matches('}').count();
            for _ in 0..closes {
                if depth == 0 {
                    break;
                }
                depth -= 1;
                if stack.pop().unwrap_or(false) {
                    in_loop -= 1;
                }
            }
        }
    }
    out
}


/// 每行所属的"块路径"：从文件级到当前行的块 id 链（`try {` 与 `} catch (e) {` 是**不同**的块）。
///
/// 死存储规则用"文本上后一次写覆盖前一次写"判定可删 —— 那只在**同一条直线路径**上成立。
/// try 里写的值在 catch 里被重写时，两条路是互斥的：成功路径上根本不会执行 catch，
/// 那个值要一直活到 try/catch 之后的读（`try { out = x } catch { out = -1 } return out`
/// 的 `r0 = a0` 就是这么被误删的）。块路径不同的两行一律按"可能不在同一条路径上"处理。
fn block_paths(lines: &[String]) -> Vec<Vec<u32>> {
    let mut out: Vec<Vec<u32>> = Vec::with_capacity(lines.len());
    let mut stack: Vec<u32> = Vec::new();
    let mut next_id = 0u32;
    for l in lines.iter() {
        let t = l.trim();
        // 先退闭括号（`} else {` 这类：先关掉本层，再开新层）
        let mut opens_after_close = false;
        if t.starts_with('}') {
            let closes = t.matches('}').count();
            for _ in 0..closes.saturating_sub(if t.ends_with('{') { 1 } else { 0 }) {
                stack.pop();
            }
            opens_after_close = t.ends_with('{');
        }
        out.push(stack.clone());
        if t.ends_with('{') && (!t.starts_with('}') || opens_after_close) {
            stack.push(next_id);
            next_id += 1;
        } else if t.ends_with('{') {
            // `} else {`：上面已经弹掉一层
            stack.push(next_id);
            next_id += 1;
        }
    }
    out
}

/// 每个下标所属的"函数区域"：顶格开始的函数（或 `var X = class`）另起一个区域，
/// 顶层代码是区域 0。寄存器按函数编号，跨区域统计读次数会多算（挡住本可以做的优化）。
fn regions(lines: &[String]) -> Vec<usize> {
    let mut out = vec![0usize; lines.len()];
    let mut cur = 0usize;
    for (i, l) in lines.iter().enumerate() {
        let t = l.trim_start();
        let head = l.len() - t.len() == 0;
        if head
            && (t.starts_with("function ")
                || t.starts_with("var ")
                || t.starts_with("let ")
                || t.starts_with("const ")
                || t.starts_with("class "))
            // `let phi…;` 是反编译器的簿记、不是新区域（老版本曾把它插在行首无缩进处）
            && !t.starts_with("let phi")
        {
            cur += 1;
        }
        out[i] = cur;
    }
    out
}

/// 每行所属的**块**（花括号配对算出来的 id）：`<` 同块 `>` 才允许把声明搬到赋值处。
///
/// 只数字符串/注释/正则**之外**的括号 —— 产物里的 `"{"` 或 `/}/` 不能让层数错位。
fn block_ids(lines: &[String]) -> Vec<usize> {
    let mut out = vec![0usize; lines.len()];
    let mut stack: Vec<usize> = vec![0];
    let mut next = 1usize;
    for (i, l) in lines.iter().enumerate() {
        out[i] = *stack.last().unwrap_or(&0);
        let t = strip_lexical(l);
        for ch in t.chars() {
            match ch {
                '{' => {
                    stack.push(next);
                    next += 1;
                }
                '}' if stack.len() > 1 => {
                    stack.pop();
                }
                _ => {}
            }
        }
    }
    out
}

/// 去掉注释、字符串/模板串、正则字面量，只留结构字符（用来数括号、看语句是否收尾）。
fn strip_lexical(s: &str) -> String {
    let b: Vec<char> = s.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    // 正则字面量的启发式：`/` 前面（跳过空白）是这些字符之一 → 当正则处理
    let regex_prev = |c: char| "=(,:[!&|?{};+-*%^~<>".contains(c);
    while i < b.len() {
        let c = b[i];
        if c == '/' && b.get(i + 1) == Some(&'/') {
            break; // 行注释吃掉整行剩余
        }
        if c == '/' && b.get(i + 1) == Some(&'*') {
            i += 2;
            while i + 1 < b.len() && !(b[i] == '*' && b[i + 1] == '/') {
                i += 1;
            }
            i += 2;
            continue;
        }
        if c == '"' || c == '\'' || c == '`' {
            i += 1;
            while i < b.len() {
                if b[i] == '\\' {
                    i += 2;
                    continue;
                }
                if b[i] == c {
                    i += 1;
                    break;
                }
                i += 1;
            }
            out.push('S');
            continue;
        }
        if c == '/' && out.trim_end().chars().last().map(regex_prev).unwrap_or(true) {
            // 正则字面量：跳过到收尾的 `/`（字符类里的 `/` 不算）
            i += 1;
            let mut in_class = false;
            while i < b.len() {
                match b[i] {
                    '\\' => i += 2,
                    '[' => {
                        in_class = true;
                        i += 1;
                    }
                    ']' => {
                        in_class = false;
                        i += 1;
                    }
                    '/' if !in_class => {
                        i += 1;
                        break;
                    }
                    _ => i += 1,
                }
                continue;
            }
            out.push('S');
            continue;
        }
        out.push(c);
        i += 1;
    }
    out
}

/// phi 赋值右值"无副作用"的宽松判据：没有调用、没有成员/下标读、没有赋值。
/// （比较/算术/寄存器/字面量都算 —— `phiN = r7 === 1` 这种簿记要能整行删掉，
/// 而不是留一句 `r7 === 1;`。）
fn phi_rhs_pure(rhs: &str) -> bool {
    let t = strip_lexical(rhs);
    if t.contains('(') || t.contains('[') || t.contains('.') {
        return false;
    }
    let b: Vec<char> = t.chars().collect();
    for (i, c) in b.iter().enumerate() {
        if *c != '=' {
            continue;
        }
        let prev = if i > 0 { b[i - 1] } else { ' ' };
        let next = b.get(i + 1).copied().unwrap_or(' ');
        // 排除 == === != !== <= >=，其余单个 `=` 视为赋值
        if prev != '=' && prev != '!' && prev != '<' && prev != '>' && next != '=' {
            return false;
        }
    }
    true
}

/// `name` 在文本里出现的次数（按标识符边界）。
fn count_ident(text: &str, name: &str) -> usize {
    let bytes = text.as_bytes();
    let mut n = 0;
    let mut i = 0;
    while let Some(pos) = text[i..].find(name) {
        let at = i + pos;
        let before_ok = at == 0 || !is_ident_byte(bytes[at - 1]);
        let after = at + name.len();
        let after_ok = after >= bytes.len() || !is_ident_byte(bytes[after]);
        if before_ok && after_ok {
            n += 1;
        }
        i = at + name.len();
    }
    n
}

/// 是不是 `phiN`（反编译器的分支汇合簿记名）。
fn phi_name(s: &str) -> Option<usize> {
    let n = s.strip_prefix("phi")?;
    if n.is_empty() || !n.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    n.parse().ok()
}

/// ⑧ 死 phi 清理：`phiN` 是反编译器在分支汇合处的**簿记**（每条 `if` 都会写一条
/// `phiN = <条件值>`，汇合点再回填）。产物里没人读它时就是纯噪声 —— 删掉赋值
/// （右值不纯就留着，宁可留一行也别丢副作用）与声明里的名字。
///
/// 死 phi 还会**挡住 ⑦**（它的赋值不是寄存器、走不动），所以这趟放在 ⑦ 之前。
fn prune_dead_phis(lines: &mut Vec<String>) {
    for _ in 0..3 {
        // 收集 phi 的赋值行与声明行
        let mut stores: HashMap<usize, Vec<usize>> = HashMap::new(); // phi → 行号
        let mut decl_lines: Vec<(usize, Vec<usize>)> = Vec::new();
        for (i, l) in lines.iter().enumerate() {
            let t = l.trim_start();
            if let Some(rest) = t.strip_prefix("let ") {
                if let Some(list) = rest.strip_suffix(';') {
                    let names: Vec<usize> = list
                        .split(',')
                        .filter_map(|p| phi_name(p.trim()))
                        .collect();
                    if !names.is_empty()
                        && names.len() == list.split(',').count()
                    {
                        decl_lines.push((i, names));
                    }
                    continue;
                }
            }
            if let Some((lhs, _)) = t.strip_suffix(';').and_then(|x| x.split_once('=')) {
                if let Some(p) = phi_name(lhs.trim()) {
                    stores.entry(p).or_default().push(i);
                }
            }
        }
        if stores.is_empty() && decl_lines.is_empty() {
            return;
        }
        // 读次数：出现次数 − 自己的赋值行 − 声明行
        let mut dead: HashSet<usize> = HashSet::new();
        for (p, rows) in &stores {
            let name = format!("phi{p}");
            let mut total = 0usize;
            for l in lines.iter() {
                total += count_ident(l, &name);
            }
            let own: usize = rows
                .iter()
                .map(|i| count_ident(&lines[*i], &name))
                .sum();
            let in_decl: usize = decl_lines
                .iter()
                .filter(|(_, ns)| ns.contains(p))
                .map(|(i, _)| count_ident(&lines[*i], &name))
                .sum();
            if total <= own + in_decl {
                dead.insert(*p);
            }
        }
        if dead.is_empty() {
            return;
        }
        // 落笔：删死 phi 的赋值行（右值不纯就留成表达式语句）、摘掉声明里的名字
        let mut drop: HashSet<usize> = HashSet::new();
        let mut rewrite: HashMap<usize, String> = HashMap::new();
        for (p, rows) in &stores {
            if !dead.contains(p) {
                continue;
            }
            for i in rows {
                if let Some(rhs) = lines[*i]
                    .trim_start()
                    .strip_suffix(';')
                    .and_then(|x| x.split_once('='))
                    .map(|(_, r)| r.trim().to_string())
                {
                    if pure_expr(&rhs) || phi_rhs_pure(&rhs) {
                        drop.insert(*i);
                    } else {
                        let indent = &lines[*i][..lines[*i].len() - lines[*i].trim_start().len()];
                        rewrite.insert(*i, format!("{indent}{rhs};"));
                    }
                }
            }
        }
        let mut out = Vec::with_capacity(lines.len());
        for (i, l) in lines.iter().enumerate() {
            if drop.contains(&i) {
                continue;
            }
            if let Some(r) = rewrite.get(&i) {
                out.push(r.clone());
                continue;
            }
            if let Some((_, names)) = decl_lines.iter().find(|(j, _)| *j == i) {
                let keep: Vec<String> = names
                    .iter()
                    .filter(|p| !dead.contains(p))
                    .map(|p| format!("phi{p}"))
                    .collect();
                if keep.is_empty() {
                    continue;
                }
                let indent = &l[..l.len() - l.trim_start().len()];
                out.push(format!("{indent}let {};", keep.join(", ")));
                continue;
            }
            out.push(l.clone());
        }
        *lines = out;
    }
}

/// ⑦ 表达式重建：把"算进寄存器、随后只读一次"的赋值并回**调用点**。
///
/// V8 对 `console.log(greet("world"))` 的产物是
/// `r2 = console; r1 = r2.log; r3 = greet("world"); r2.log(r3);`（第六条先删掉中转加载）。
/// 这里从调用行往前扫**同块内连续**的语句，把"该寄存器在整个函数区域里只被读一次、且那一次
/// 正落在这一行"的赋值并进调用（接收者 / 实参位置），删掉这些赋值 —— 得到
/// `console.log(greet("world"))`。
///
/// **为什么不换序**：整段一并搬进调用表达式，而 JS 的求值顺序恰好是"接收者 → 属性 → 实参
/// 从左到右"，与字节码里这些寄存器赋值的先后**一致**；段内每句的相对次序因此不变。
/// 不越过的情形（遇到就停在这句之前）：跨区域/跨块、该寄存器别处还要读、值依赖的寄存器
/// 或标识符被段内更靠后的句子改写、这行自己是被重建过的调用。
fn rebuild_calls(lines: &mut Vec<String>) {
    let region = regions(lines);
    let block = block_ids(lines);

    let mut drop: HashSet<usize> = HashSet::new();
    let mut rewritten: HashMap<usize, String> = HashMap::new();
    // 某行被"哪个调用行"吸收的。本趟链上（inlined/自己）吸收掉的可以直接跳过继续往前扫；
    // 被别人吸收掉的不能跳 —— 那意味着它的求值位置在中间某个调用里。
    let mut drop_owner: HashMap<usize, usize> = HashMap::new();
    for li in 0..lines.len() {
        if drop.contains(&li) || !lines[li].contains('(') {
            continue; // 不是调用行 / 已被并走
        }
        if !starts_statement(lines, li) {
            continue; // 这行本身是无括号控制体（`if (c) use(r0);`）→ 条件执行，不能并
        }
        if is_control_header(lines[li].trim_start()) {
            continue; // 控制流头（if/while/for/switch）：条件每轮/每次重新求值，
            // 且把聚合表达式（对象/数组字面量）并进条件会非常难读
        }
        let mut cur = lines[li].clone();
        let mut k = li;
        let mut inlined: Vec<usize> = Vec::new();
        let mut steps = 0usize;
        while k > 0 && steps < 64 {
            steps += 1;
            let prev = k - 1;
            if drop.contains(&prev) {
                let ours = drop_owner.get(&prev).is_some_and(|o| *o == li || inlined.contains(o));
                if ours {
                    k = prev; // 它的值已经在本趟的调用行里了 → 可以继续往前
                    continue;
                }
                break; // 被别人吸收 → 中间隔着一次调用，不能越过
            }
            // 这一行可能自己也被重建过（`r4 = r4(1, 2, 3);`）→ 用**当前文本**当候选
            let prev_text = rewritten
                .get(&prev)
                .cloned()
                .unwrap_or_else(|| lines[prev].clone());
            let Line::Store { reg, rhs } = parse(&prev_text) else { break };
            if region[prev] != region[li] || block[prev] != block[li] {
                if std::env::var("JSCD_DBG7").is_ok() { eprintln!("[7] L={li} prev={prev} reg={reg} stop: region/block"); } break; // 跨区域/跨块 → 可能不在同一条执行路径上
            }
            if !starts_statement(lines, prev) {
                if std::env::var("JSCD_DBG7").is_ok() {
                    eprintln!("[7] L={li} prev={prev} stop: braceless");
                }
                break; // 无括号控制体（`if (c) r0 = f();`）→ 条件执行、括号配平也是另一个块
            }
            if reads_in(&parse(&cur)).get(&reg).copied().unwrap_or(0) != 1 {
                if std::env::var("JSCD_DBG7").is_ok() { eprintln!("[7] L={li} prev={prev} reg={reg} stop: read not in L"); } break; // 那一次读不在这行（或不止一次）
            }
            // 活跃性：**到下一次写这个寄存器之前**不能再有人读它。
            // （不能要求"整个区域只读一次" —— 寄存器是复用的：`r2 = console; …; r2.log(…)`
            // 之后同一块里还会再写 `r2`，那是另一条语句组的事。）
            let mut later = 0usize;
            for idx in li + 1..lines.len() {
                if drop.contains(&idx) {
                    continue; // 这行已被并走，它的读跟着表达式搬了
                }
                if region[idx] != region[li] {
                    break; // 出了函数区域：读的是另一个寄存器
                }
                if matches!(parse(&lines[idx]), Line::Store { reg: r2, .. } if r2 == reg) {
                    // 覆盖之前，这一行自己也可能读旧值（`r2 = new r2(a0)` 这种自引用写）
                    // —— 那就是"之后还有人用这个值"，不能并。
                    later += reads_in(&parse(&lines[idx])).get(&reg).copied().unwrap_or(0);
                    break;
                }
                later += reads_in(&parse(&lines[idx])).get(&reg).copied().unwrap_or(0);
            }
            if later > 0 {
                if std::env::var("JSCD_DBG7").is_ok() { eprintln!("[7] L={li} prev={prev} reg={reg} stop: later reads"); } break;
            }
            // 值依赖不能被段内更靠后的句子改写（寄存器撞车 / 同名标识符被赋值）
            if breaks_dependency(&rhs, &inlined, lines) {
                break;
            }
            // 替换点的上下文：`new <reg>(…)` 的被调位置必须加括号（`new f(x)(a)` 会被解析成
            // "先 new 再调用"，语义全变）；`<reg> = …` / `<reg>++` 那种"左值位置"直接放弃。
            let Some(ctx) = occurrence_context(&cur, reg) else { break };
            if ctx.as_target {
                break;
            }
            let text = paren_if_needed(&rhs, ctx.after_new);
            let Some(next) = replace_reg(&cur, reg, &text) else { break };
            cur = next;
            if rewritten.contains_key(&prev) {
                rewritten.remove(&prev);
                drop.insert(prev); // 内容已并进我们这行 → 原来那行不能再输出
                drop_owner.insert(prev, li);
            }
            inlined.push(prev);
            k = prev;
        }
        if !inlined.is_empty() {
            for i in &inlined {
                drop.insert(*i);
                drop_owner.insert(*i, li);
            }
            rewritten.insert(li, cur);
        }
    }
    let mut out = Vec::with_capacity(lines.len());
    for (i, l) in lines.iter().enumerate() {
        if drop.contains(&i) {
            continue;
        }
        out.push(rewritten.remove(&i).unwrap_or_else(|| l.clone()));
    }
    *lines = out;
}

/// `rhs` 依赖的寄存器/标识符，是否被**已经并进去的那些行**（`inlined`，它们会搬到调用点、
/// 求值位置往后挪）改写 —— 改写就说明不能并（`r5 = r6; r6 = f(); use(r5, r6)` 里的 `r6`）。
fn breaks_dependency(rhs: &str, inlined: &[usize], lines: &[String]) -> bool {
    if inlined.is_empty() {
        return false;
    }
    let deps: HashSet<usize> = reads_in(&parse(&format!("x = {rhs};"))).into_keys().collect();
    for i in inlined {
        let Line::Store { reg, .. } = parse(&lines[*i]) else { continue };
        if deps.contains(&reg) {
            return true;
        }
        // 标识符被赋值的形态（`X = …` / `X++` / `++X`）—— 保守地只认这几个
        for name in ident_words(rhs) {
            let pat = format!("{name} =");
            if lines[*i].contains(&pat) || lines[*i].contains(&format!("{name}++"))
                || lines[*i].contains(&format!("++{name}"))
            {
                return true;
            }
        }
    }
    false
}

/// 文本里出现的裸标识符词（跳过 `rN` 与字符串/注释里的内容 —— 粗筛，宁可多报）。
fn ident_words(s: &str) -> Vec<String> {
    let clean = strip_lexical(s);
    let mut out = Vec::new();
    let mut cur = String::new();
    for ch in clean.chars().chain(std::iter::once(' ')) {
        if is_ident_byte(ch as u8) && !ch.is_ascii_digit() || !cur.is_empty() && is_ident_byte(ch as u8) {
            cur.push(ch);
        } else {
            if !cur.is_empty() {
                if !cur.starts_with('r') || !cur[1..].bytes().all(|b| b.is_ascii_digit()) {
                    out.push(cur.clone());
                }
                cur.clear();
            }
        }
    }
    out
}

/// 替换点的形态：`after_new` = 前面紧邻 `new`（被调位置，括号必需）；
/// `as_target` = 后面是 `=`/`++`/`--` 这类左值位置（不能替换成任意表达式）。
struct OccurCtx {
    after_new: bool,
    as_target: bool,
}

/// 找 `rN` 在行里的**唯一**出现并判断上下文；找不到/多处出现返回 `None`。
fn occurrence_context(line: &str, reg: usize) -> Option<OccurCtx> {
    let needle = format!("r{reg}");
    let bytes = line.as_bytes();
    let mut hits = Vec::new();
    let mut i = 0;
    while let Some(pos) = line[i..].find(&needle) {
        let at = i + pos;
        let before_ok = at == 0 || !is_ident_byte(bytes[at - 1]);
        let after = at + needle.len();
        let after_ok = after >= bytes.len() || !is_ident_byte(bytes[after]);
        if before_ok && after_ok {
            hits.push(at);
        }
        i = at + needle.len();
    }
    if hits.len() != 1 {
        return None;
    }
    let at = hits[0];
    let before = line[..at].trim_end();
    let after = line[at + needle.len()..].trim_start();
    let as_target = after.starts_with("++")
        || after.starts_with("--")
        || (after.starts_with('=') && !after.starts_with("=="));
    Some(OccurCtx {
        after_new: before.ends_with("new"),
        as_target,
    })
}

/// 并进调用点时要不要加括号：不是"原子表达式"就加（`a.b`、`f(x)`、字面量、`[..]` 是原子；
/// 二元/一元/条件/逗号/对象字面量不是）。`new` 的被调位置一律加。多余括号由 AST 层
/// （swc 重新打印）清掉。
fn paren_if_needed(rhs: &str, after_new: bool) -> String {
    // `new` 的被调位置：只有"裸引用"（`C`、`a.b`）能直接跟，带调用的（`f(x)`）必须加括号，
    // 否则 `new f(x)(a)` 会被解析成"先 new 再调用"。
    if after_new {
        return if is_plain_ref(rhs) {
            rhs.to_string()
        } else {
            format!("({rhs})")
        };
    }
    if is_primary_expr(rhs) {
        rhs.to_string()
    } else {
        format!("({rhs})")
    }
}

/// 裸引用：标识符或点号路径（`C`、`a.b.c`）—— 放在 `new` 后面不会歧义。
fn is_plain_ref(s: &str) -> bool {
    let t = s.trim();
    !t.is_empty()
        && t.split('.').all(|part| {
            let mut b = part.bytes();
            matches!(b.next(), Some(c) if c.is_ascii_alphabetic() || c == b'_' || c == b'$')
                && b.all(is_ident_byte)
        })
}

/// 粗判"原子表达式"：不含顶层运算符、不以一元运算符或函数/类关键字开头。
fn is_primary_expr(s: &str) -> bool {
    let t = s.trim();
    let Some(first) = t.chars().next() else { return false };
    if matches!(first, '-' | '+' | '!' | '~' | '?' | '{') {
        return false;
    }
    for kw in ["typeof ", "void ", "delete ", "await ", "yield", "function", "class", "async "] {
        if t.starts_with(kw) {
            return false;
        }
    }
    let b: Vec<char> = t.chars().collect();
    let mut depth = 0i32;
    let mut i = 0usize;
    while i < b.len() {
        let c = b[i];
        match c {
            '"' | '\'' | '`' => {
                i += 1;
                while i < b.len() {
                    if b[i] == '\\' {
                        i += 2;
                        continue;
                    }
                    if b[i] == c {
                        i += 1;
                        break;
                    }
                    i += 1;
                }
                continue;
            }
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth -= 1,
            '+' | '-' | '*' | '/' | '%' | '<' | '>' | '=' | '&' | '|' | '^' | '~' | '!' | '?'
            | ',' if depth == 0 => return false,
            _ => {}
        }
        i += 1;
    }
    true
}

/// ⑤ `let r0, r1;` + 之后**同块内**的一次 `r0 = E;` → 把 `r0` 从声明里摘掉、把那行原地
/// 写成 `let r0 = E;`。
///
/// 安全性：不搬运任何东西、不改求值顺序。条件三条：
///   * 该寄存器在这个函数区域里**只有一处声明、只有这一次写**；
///   * 这次写之前**没有任何出现**（否则原来读到的是 `undefined`，改完会变成 TDZ 报错）；
///   * 这次写是同块内的直接语句（`if (c)` 这类无括号控制头后面不能直接跟 `let`）。
fn bind_declarators(lines: &mut Vec<String>) {
    let region = regions(lines);
    let block = block_ids(lines);
    let parsed: Vec<Line> = lines.iter().map(|l| parse(l)).collect();
    type Key = (usize, usize); // (函数区域, 寄存器)
    let mut decls: HashMap<Key, usize> = HashMap::new(); // 声明出现次数
    let mut decl_at: HashMap<Key, usize> = HashMap::new(); // 声明所在行
    let mut stores: HashMap<Key, usize> = HashMap::new(); // 写出现次数
    let mut first_use: HashMap<Key, usize> = HashMap::new(); // 最早一次"读或写"的行号
    for (i, l) in parsed.iter().enumerate() {
        if let Line::Decl(regs) = l {
            for r in regs {
                *decls.entry((region[i], *r)).or_insert(0) += 1;
                decl_at.entry((region[i], *r)).or_insert(i);
            }
        }
        if let Line::Store { reg, .. } = l {
            let k = (region[i], *reg);
            *stores.entry(k).or_insert(0) += 1;
            first_use.entry(k).or_insert(i);
        }
        for r in reads_in(l).keys() {
            first_use.entry((region[i], *r)).or_insert(i);
        }
    }

    let mut drop_names: HashMap<usize, HashSet<usize>> = HashMap::new(); // 声明行 → 摘掉的名字
    let mut promote: HashMap<usize, usize> = HashMap::new(); // 赋值行 → 提升为声明的寄存器
    for (i, l) in parsed.iter().enumerate() {
        let Line::Decl(regs) = l else { continue };
        for r in regs {
            let k = (region[i], *r);
            if decls.get(&k).copied() != Some(1) || stores.get(&k).copied() != Some(1) {
                continue; // 多处声明或多次写 → 不动
            }
            if decl_at.get(&k).copied() != Some(i) {
                continue;
            }
            // 唯一那次"读或写"必须就是那次写，且在同块、是块内直接语句
            let Some(&j) = first_use.get(&k) else { continue };
            if j <= i
                || region[j] != region[i]
                || block[j] != block[i]
                || !matches!(&parsed[j], Line::Store { reg, .. } if reg == r)
                || !starts_statement(lines, j)
            {
                continue;
            }
            drop_names.entry(i).or_default().insert(*r);
            promote.insert(j, *r);
        }
    }
    if drop_names.is_empty() {
        return;
    }

    let mut out = Vec::with_capacity(lines.len());
    for (i, l) in lines.iter().enumerate() {
        if let Some(drop) = drop_names.get(&i) {
            let Line::Decl(regs) = &parsed[i] else { unreachable!() };
            let keep: Vec<String> = regs
                .iter()
                .filter(|r| !drop.contains(r))
                .map(|r| format!("r{r}"))
                .collect();
            if keep.is_empty() {
                continue; // 整行没人要了 → 删
            }
            let indent = &l[..l.len() - l.trim_start().len()];
            out.push(format!("{indent}let {};", keep.join(", ")));
            continue;
        }
        if promote.contains_key(&i) {
            let indent = &l[..l.len() - l.trim_start().len()];
            out.push(format!("{indent}let {}", l.trim_start()));
            continue;
        }
        out.push(l.clone());
    }
    *lines = out;
}

/// `j` 行能不能原地变成 `let x = …;`：往前找第一条非空行，必须以 `;`/`{`/`}`/`:` 收尾。
/// （`if (c)`、`for (…)`、`else` 这类无括号控制头后面跟 `let` 是语法错误。）
fn starts_statement(lines: &[String], j: usize) -> bool {
    for k in (0..j).rev() {
        let t = strip_lexical(&lines[k]);
        let t = t.trim();
        if t.is_empty() {
            continue;
        }
        return t.ends_with(';') || t.ends_with('{') || t.ends_with('}') || t.ends_with(':');
    }
    true // 文件开头
}


/// ⑨ 死临时清理：`tN = <纯表达式>;` 且 `tN` 全文无人读、又被 `let` 声明过 → 删。
///
/// 这些 `tN` 是发射端在"覆盖寄存器前先把 acc 落下来"时造的（见 `store_reg` 的说明）。
/// 很多情况下 acc 紧接着就被下一条指令覆盖，落下来的那一行根本没人读（`Mov a0, r0`
/// 前面那条 `t0 = r0;`）—— 它们不在 `rN` 命名空间里，① 那条死存储规则看不见。
/// 只在**确认声明过**（`let t0;`）时才动：未声明的 `tX = …` 是全局赋值，删了会改行为。
fn prune_dead_temps(lines: &mut Vec<String>) {
    let declared: std::collections::HashSet<String> = {
        let mut out = std::collections::HashSet::new();
        for l in lines.iter() {
            let t = l.trim();
            let Some(rest) = t.strip_prefix("let ") else { continue };
            let Some(list) = rest.strip_suffix(';') else { continue };
            for part in list.split(',') {
                let n = part.trim();
                if is_temp_name(n) {
                    out.insert(n.to_string());
                }
            }
        }
        out
    };
    if declared.is_empty() {
        return;
    }
    // 全文中每个 temp 名被读的次数（`t0 = …` 的左侧不算读）
    let mut reads: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for l in lines.iter() {
        let t = l.trim();
        // 声明行（`let t0;`）里的名字不是读
        if t.starts_with("let ") || t.starts_with("var ") || t.starts_with("const ") {
            continue;
        }
        for name in &declared {
            let mut count = 0usize;
            let bytes = t.as_bytes();
            let mut i = 0usize;
            while let Some(pos) = t[i..].find(name.as_str()) {
                let at = i + pos;
                let before_ok = at == 0 || !is_ident_byte(bytes[at - 1]);
                let after = at + name.len();
                let after_ok = after >= bytes.len() || !is_ident_byte(bytes[after]);
                if before_ok && after_ok {
                    count += 1;
                }
                i = at + name.len();
            }
            // 赋值左侧那次不算读
            let is_store = t.strip_suffix(';').and_then(|x| x.split_once('=')).map(|(a, _)| a.trim() == name).unwrap_or(false);
            if is_store {
                count = count.saturating_sub(1);
            }
            if count > 0 {
                *reads.entry(name.clone()).or_insert(0) += count;
            }
        }
    }
    lines.retain(|l| {
        let t = l.trim();
        let Some(name) = t.strip_suffix(';').and_then(|x| x.split_once('=')).map(|(a, _)| a.trim()) else {
            return true;
        };
        if !is_temp_name(name) || !declared.contains(name) || reads.get(name).copied().unwrap_or(0) > 0 {
            return true;
        }
        let rhs = t.strip_suffix(';').and_then(|x| x.split_once('=')).map(|(_, b)| b.trim()).unwrap_or("");
        !pure_expr(rhs)
    });
}

/// `t<数字>` 形态（发射端的临时名）。
fn is_temp_name(s: &str) -> bool {
    s.strip_prefix('t')
        .map(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
        .unwrap_or(false)
}

fn optimize_once(lines: &mut Vec<String>) {
    let loop_body = inside_loop(lines);
    // 寄存器是**每个函数各自编号**的：`r3` 在顶层和在另一个函数里是两个不同的寄存器
    // → 读次数按"函数区域"分别统计（区域 = 最近一个顶格的 `function`/`var X = class` 之后）。
    let region = regions(lines);
    let parsed: Vec<Line> = lines.iter().map(|l| parse(l)).collect();
    let mut reads: HashMap<(usize, usize), usize> = HashMap::new();
    for (i, l) in parsed.iter().enumerate() {
        for (r, n) in reads_in(l) {
            *reads.entry((region[i], r)).or_insert(0) += n;
        }
    }

    // ① 死存储：写了、之后再没被读、且右侧纯（`read_later` **只来自真正的读** ——
    // 早先把"保留但没人读"的写也塞进去，于是挡掉了后面本该删掉的死存储）。
    // 反向扫，记下每个寄存器"下一个动作"是读还是写：
    //   * 之后是**写**（说明这次写的值在被读之前就被覆盖了）→ 死存储，可删；
    //   * 之后是读，或之后再没出现过 → 留着（保守：读可能在别的分支里）。
    #[derive(PartialEq)]
    enum Next {
        Read,
        Write,
    }
    // 键是 (区域, 寄存器)：跨函数的同名寄存器是两个寄存器，残留状态会让
    // "之后再没出现过" 误判（后一个区域的写被当成前一个区域那次写的覆盖者）。
    let mut next: HashMap<(usize, usize), (Next, usize)> = HashMap::new();
    let paths = block_paths(lines);
    // ⑥ 方法加载中转（渲染副产品）先标出来：后面两条规则都该当它不存在
    let mut drop_idx: HashSet<usize> = call_scratch_lines(&parsed, lines, &region);
    for i in (0..parsed.len()).rev() {
        let reads_here = reads_in(&parsed[i]);
        if let Line::Store { reg, rhs } = &parsed[i] {
            let key = (region[i], *reg);
            // 之后的动作是"写"（且那次写在**同一块路径**上 —— 否则是互斥分支，值还活着），
            // 或者之后再没出现过（纯右值时删掉不影响行为）→ 这次写的值没人要
            let dead = match next.get(&key) {
                Some((Next::Write, j)) => paths[i] == paths[*j],
                None => true,
                Some((Next::Read, _)) => false,
            };
            if dead && pure_expr(rhs) && !loop_body[i] && !reads_here.contains_key(reg) {
                drop_idx.insert(i);
            }
        }
        for r in reads_here.keys() {
            next.insert((region[i], *r), (Next::Read, i));
        }
        if let Line::Store { reg, .. } = &parsed[i] {
            if !reads_here.contains_key(reg) {
                next.insert((region[i], *reg), (Next::Write, i));
            }
        }
    }

    // ② 单次使用前送：`rX = E;` 的值在之后恰好被读一次，且中间没有带副作用的语句
    let mut forward: HashMap<usize, (usize, usize, String)> = HashMap::new(); // 读所在行 → (被删的赋值行, 寄存器, 表达式)
    let mut forwarded_store: HashSet<usize> = HashSet::new();
    for i in 0..parsed.len() {
        if drop_idx.contains(&i) {
            continue;
        }
        let Line::Store { reg, rhs } = &parsed[i] else {
            continue;
        };
        // 只前送"这次写的值只读一次"的：扫到下一次写之前，读次数恰好 1 且只出现在同一行
        if loop_body[i] {
            continue; // 循环体里不做前送（下一轮迭代才读的写法看不出来）
        }
        // 只在**紧邻下一行**读时前送：同块、顺序天然保持，不用做（不可靠的）线性副作用分析。
        // 跨行的搬运看似能省更多，实测在分支/合并点（phi）上会替错寄存器 —— operators、
        // generator 那批 fixture 就是这么挂的。
        let Some(j) = (i + 1 < parsed.len()).then_some(i + 1) else {
            continue;
        };
        if drop_idx.contains(&j) {
            continue; // 目标行本身要被删（⑥ 的方法加载中转）→ 别往那儿搬
        }
        if matches!(&parsed[j], Line::Store { reg: r2, .. } if r2 == reg) {
            continue;
        }
        let n = reads_in(&parsed[j]).get(reg).copied().unwrap_or(0);
        if n != 1 {
            continue;
        }
        // 这次写的值只能被读一次：往后扫到下一次写之前不能再有读
        let mut later_reads = 0usize;
        for l in &parsed[j + 1..] {
            if matches!(l, Line::Store { reg: r2, .. } if r2 == reg) {
                break;
            }
            later_reads += reads_in(l).get(reg).copied().unwrap_or(0);
        }
        if later_reads > 0 {
            continue;
        }
        // 行内插入点之前不能有副作用（`g() + h(r1)` 里塞 f() 会变成 g,h,f）
        // **可证明安全**才搬，三条都满足：
        //   ① 右值是字面量/寄存器（值不会变、无副作用）；
        //   ② 这个寄存器在**整份文件里只被读这一次**（别处再读就说明它是复用的共享寄存器，
        //      替换会把另一条路径上的值弄错 —— operators/async 那批 fixture 就是这么挂的）；
        //   ③ 紧邻下一行（同块、中间没有任何语句）。
        // 满足这三条时，值被搬到的位置与原地求值等价（字面量/寄存器读无副作用，
        // 且调用点也看不到我们的局部寄存器）—— 于是连"行内前缀"都不用怕。
        // 字面量/寄存器随时可搬；裸标识符（`greet`、`console`）在**紧邻**时也可
        // —— 中间没有任何语句，值不可能被改写。带副作用的表达式一律不搬。
        if !matches!(purity(rhs), Purity::Stable | Purity::Ident) {
            continue;
        }
        // 还要求"这个寄存器在整个函数区域里只被读一次"：紧邻与"之后没人读"只覆盖了
        // 这一次值的生命周期，register 复用（另一条路径也读它）仍会把值弄错 ——
        // 实测放松这条会挂掉 try_catch / async 一类 fixture。
        if reads.get(&(region[i], *reg)).copied().unwrap_or(0) != 1 {
            continue;
        }
        if replace_reg(&lines[j], *reg, rhs).is_none() {
            continue;
        }
        forward.insert(j, (i, *reg, rhs.clone()));
        forwarded_store.insert(i);
    }

    // 剪枝：一条语句既当"前送来源"（会被删）又当别人的"前送目标"（会被改写）时，
    // 两个改写都基于**原始文本**、不能叠加：`r0 = r4; r1 = r0; new r1(a0);` 里
    // 前一条删 `r0 = r4` 并把下一行改写成 `r1 = r4`，后一条删 `r1 = r0` 并把 new 改成
    // `new r0` —— 结果 r0 的值凭空消失（twoclasses 就栽在这）。丢掉目标会被删的那条。
    let conflicted: Vec<usize> = forward
        .keys()
        .filter(|d| forwarded_store.contains(d))
        .cloned()
        .collect();
    for d in conflicted {
        if let Some((src, _, _)) = forward.remove(&d) {
            forwarded_store.remove(&src);
        }
    }

    // 落笔：先改写读处、再删行
    let mut new_lines = Vec::with_capacity(lines.len());
    for (i, l) in lines.iter().enumerate() {
        if forwarded_store.contains(&i) || drop_idx.contains(&i) {
            continue;
        }
        let mut cur = l.clone();
        if let Some((_, reg, rhs)) = forward.get(&i) {
            if let Some(replaced) = replace_reg(&cur, *reg, rhs) {
                cur = replaced;
            }
        }
        new_lines.push(cur);
    }

    // ③ 声明裁剪
    *lines = new_lines;
    prune_decls(lines);
}

/// 是不是控制流头（条件/步进每轮重新求值）。
fn is_control_header(t: &str) -> bool {
    t.starts_with("while (")
        || t.starts_with("while(")
        || t.starts_with("for (")
        || t.starts_with("for(")
        || t.starts_with("if (")
        || t.starts_with("if(")
        || t.starts_with("switch (")
        || t.starts_with("switch(")
        || t.starts_with("do {")
        || t.contains(": while (")
        || t.contains(": for (")
        || t.contains("} else if (")
}

/// ③ 声明裁剪：`let r0, r1…;` 只留还被用到的（读或写都算 —— 只删"真没出现"的，
/// 否则留下的赋值会变成隐式全局）。表达式重建（⑦）之后要再跑一次。
///
/// 按**函数区域**统计：顶层声明的 `r0` 和某个函数体里的 `r0` 是两个寄存器，
/// 混在一起会把顶层的死声明留住（`let r0, r1, r5;` 这种残留）。
fn prune_decls(lines: &mut Vec<String>) {
    let region = regions(lines);
    // (区域, 寄存器) → 是否被读/写
    let mut used: HashSet<(usize, usize)> = HashSet::new();
    for (i, l) in lines.iter().enumerate() {
        for r in reads_in(&parse(l)).keys() {
            used.insert((region[i], *r));
        }
        if let Line::Store { reg, .. } = parse(l) {
            used.insert((region[i], reg));
        }
    }
    let mut with_decls = Vec::with_capacity(lines.len());
    for (i, l) in lines.drain(..).enumerate() {
        match parse(&l) {
            Line::Decl(regs) => {
                let keep: Vec<String> = regs
                    .iter()
                    .filter(|r| used.contains(&(region[i], **r)))
                    .map(|r| format!("r{r}"))
                    .collect();
                if keep.is_empty() {
                    continue;
                }
                let indent = &l[..l.len() - l.trim_start().len()];
                with_decls.push(format!("{indent}let {};", keep.join(", ")));
            }
            _ => with_decls.push(l),
        }
    }
    *lines = with_decls;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forwards_literals_and_identifiers() {
        let src = "let r0, r1;\nr1 = greet;\nr0 = r1(1);\n";
        let out = optimize(src);
        assert!(out.contains("r0 = greet(1);"), "{out}");
        assert!(!out.contains("r1 = greet;"), "{out}");
        assert!(!out.contains("let r0, r1;"), "{out}");
    }

    #[test]
    fn does_not_reorder_effectful_calls() {
        // 三调用链：把 f() 搬到 h() 之后就变成 f,h,g —— 顺序变了 → 必须挡住
        let src = "let r0, r1, r2, r3;\nr1 = f();\nr2 = g();\nr3 = h(r1);\nr0 = r2 + r3;\n";
        let out = optimize(src);
        assert!(out.contains("r1 = f();"), "f() 必须留在原地：{out}");
        assert!(!out.contains("h(f())"), "不许把 f() 越过 h() 前送：{out}");
        // 保序：文本里 f() 仍出现在 g() 之前
        let (pf, pg) = (
            out.find("f()").unwrap_or(usize::MAX),
            out.find("g()").unwrap_or(usize::MAX),
        );
        assert!(pf < pg, "f 必须仍在 g 之前：{out}");
    }

    #[test]
    fn merges_adjacent_stable_values() {
        // 字面量/寄存器（无副作用）相邻时可以合并
        let src = "let r0, r1, r2;\nr1 = 1;\nr0 = r1 + r2;\n";
        let out = optimize(src);
        assert!(out.contains("r0 = 1 + r2;"), "{out}");
        assert!(!out.contains("r1 = 1;"), "{out}");
    }

    #[test]
    fn does_not_touch_loop_bodies() {
        let src = "let r0;\nwhile (true) {\n  use(r0);\n  r0 = 1;\n}\n";
        let out = optimize(src);
        assert!(out.contains("r0 = 1;"), "循环体里的写不许删：{out}");
    }

    #[test]
    fn does_not_move_calls() {
        // 调用有副作用 → 一律不搬（要搬得先做真正的控制流分析）
        let src = "let r0, r1;\nr1 = f();\nr0 = r1 + 1;\n";
        let out = optimize(src);
        assert!(out.contains("r1 = f();"), "{out}");
        assert!(!out.contains("f() + 1"), "{out}");
    }

    #[test]
    fn keeps_property_reads() {
        // `a.b` 可能有 getter → 不删、不搬
        let src = "let r0, r1;\nr1 = r0.log;\n";
        let out = optimize(src);
        assert!(out.contains("r1 = r0.log;"), "属性读不许删：{out}");
    }

    #[test]
    fn drops_unused_declared_registers() {
        let src = "let r0, r1, r2;\nr0 = 1;\nreturn r0;\n";
        let out = optimize(src);
        // r0 的值被前送进 return；r1/r2 连声明一起没了
        assert!(out.contains("return 1;"), "{out}");
        assert!(!out.contains("r1"), "{out}");
        assert!(!out.contains("let "), "没有寄存器再用时不该留声明：{out}");
    }

    #[test]
    fn keeps_token_boundaries_on_forward() {
        // `-1` 换进 `r2-r7` 不能拼成 `r2--1`
        let src = "r7 = -1;\nr2 = r2-r7;\n";
        let out = optimize(src);
        assert!(!out.contains("--"), "不许拼出自减：{out}");
        // 数字字面量后跟点访问不能拼成 `1.toString()`
        let src2 = "r0 = 1;\nr2 = r0.toString();\n";
        let out2 = optimize(src2);
        assert!(!out2.contains("1.toString"), "数字后跟点要括号：{out2}");
    }

    #[test]
    fn does_not_cancel_chained_forwards() {
        // 链式前送不能互相抵消：要么来源留下、要么整条链并进使用处（都不能丢值）
        let src = "let r0, r1, r3;\nr4 = C;\nr0 = r4;\nr1 = r0;\nr3 = new r1(a0);\n";
        let out = optimize(src);
        assert!(!out.contains("r1 = r0;"), "链会被并掉：{out}");
        assert!(out.contains("new C(a0)"), "整条链要重建到 C：{out}");
    }

    #[test]
    fn does_not_inline_into_a_braceless_if_body() {
        // 无条件求值 → 变成条件求值：`f()` 可能不再执行
        let src = "let r0;\nr0 = f();\nif (c)\n  use(r0);\n";
        let out = optimize(src);
        assert!(!out.contains("use(f())"), "不能把无条件求值挪进条件体：{out}");
    }

    #[test]
    fn does_not_inline_across_blocks() {
        // 赋值在块外、读取在块内（反之亦然）→ 执行路径可能不同
        let src = "let r0;\nr0 = f();\nif (c) {\n  use(r0);\n}\n";
        let out = optimize(src);
        assert!(!out.contains("use(f())"), "跨块不许并：{out}");
        let src2 = "let r0;\nif (c) {\n  r0 = f();\n}\nuse(r0);\n";
        let out2 = optimize(src2);
        assert!(!out2.contains("use(f())"), "跨块不许并：{out2}");
    }

    #[test]
    fn does_not_inline_into_loop_header() {
        // 循环头每轮求值：带副作用的表达式并进去会变成"每轮都调"
        let src = "let r0;\nr0 = f();\nwhile (r0 < 10) {\n  r0 = r0 + 1;\n}\n";
        let out = optimize(src);
        assert!(!out.contains("while (f()"), "循环头不许并：{out}");
    }

    #[test]
    fn rebuilds_call_arguments() {
        // 接收者 + 实参一起并回调用（用户要的那一步）
        let src = "let r2, r3;\nr2 = console;\nr3 = greet(\"world\");\nr2.log(r3);\n";
        let out = optimize(src);
        assert!(out.contains("console.log(greet(\"world\"))"), "{out}");
    }

    #[test]
    fn dead_pure_store_removed() {
        let src = "let r0, r1;\nr1 = \"x\";\nreturn 1;\n";
        let out = optimize(src);
        assert!(!out.contains("r1 = \"x\";"), "{out}");
    }

    #[test]
    fn binds_declaration_to_single_assignment() {
        // 用户那个例子：声明 + 赋值 + 单次读 → 最终整条并进调用
        let src = "let r0, r2, r3;\nr0 = greet;\nr2 = console;\nr3 = r0(\"world\");\nr2.log(r3);\n";
        let out = optimize(src);
        assert!(out.contains("console.log(greet(\"world\"))"), "{out}");
        assert!(!out.contains("let r0, r2, r3;"), "旧声明行该没了：{out}");
    }

    #[test]
    fn does_not_bind_when_read_before_store() {
        // 赋值**之前**就有读（读到的是 undefined）→ 提升会变成 TDZ 报错，必须挡住
        let src = "let r0;\nif (c) {\n  use(r0);\n}\nr0 = f();\n";
        let out = optimize(src);
        assert!(out.contains("r0 = f();"), "不许提升：{out}");
        assert!(!out.contains("let r0 = f();"), "{out}");
    }

    #[test]
    fn does_not_bind_into_braceless_if() {
        // `if (c) let r0 = 1;` 是语法错误 → 必须挡住
        let src = "let r0;\nif (c)\n  r0 = f();\nuse(r0);\n";
        let out = optimize(src);
        assert!(out.contains("  r0 = f();"), "无括号 if 体里不许放 let：{out}");
        assert!(!out.contains("let r0 = f();"), "{out}");
    }

    #[test]
    fn binds_inside_same_block_only() {
        // 赋值在更深的块里 → 块不同，不动
        let src = "let r0, r1;\nr1 = f();\nif (c) {\n  r0 = g();\n}\nuse(r0, r1);\n";
        let out = optimize(src);
        assert!(out.contains("r0 = g();"), "跨块不许提升：{out}");
        assert!(!out.contains("let r0 = g();"), "{out}");
    }
}
