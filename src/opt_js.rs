//! AST 级优化：借 [swc](https://swc.rs) 的压缩器——terser 的 Rust 移植，Next.js 生产在用。
//!
//! 文本级那轮（[`crate::opt`]）只能做"看着像就删"的局部清理；真正的冗余消除——**复制传播**
//! （`r0 = greet; … r0("world")` → `greet("world")`）、无用变量删除、常量折叠——需要控制流
//! 与作用域信息，那是 AST 的活。
//!
//! 两个必须踩准的点（都踩过坑）：
//!
//! 1. **管线顺序**：`paren_remover` → `resolver` → `optimize` → `hygiene` → `fixer`。
//!    少 `paren_remover` 会**误编译**——压缩器把 `(a = console).log` 这类"括号保护"的
//!    结构当成普通取属性，实测会把 `a = console; b = a.log;` 改写成 `a = console.log`
//!    （语义全错）。swc 官方 `minify()` 就是这么排的。
//! 2. **包一层函数**：压缩器只在**函数作用域**里做复制传播。脚本顶层它把顶层绑定当全局
//!    对象属性（可能被别的脚本改），一律不碰。而我们的载荷在真实运行时本来就是
//!    `Module.wrap` 的**函数体**——包一层既解锁优化、又更贴近真实语义；优化完把壳剥掉。
//!
//! 取向是**可读性**而不是最小体积：不改名、不合并语句、不转箭头函数、保函数名。
//! 解析失败 / 结构不认识 / 产物为空 → 返回 `None`，调用方原样输出文本级结果。

use swc_core::common::comments::{Comments, SingleThreadedComments};
use swc_core::common::{FileName, Globals, Mark, SourceMap, GLOBALS, sync::Lrc};
use swc_core::ecma::ast::{Callee, EsVersion, Expr, Function, Program, Script, Stmt, UnaryOp};
use swc_core::ecma::codegen::{Config, Emitter, text_writer::JsWriter};
use swc_core::ecma::minifier::optimize as swc_optimize;
use swc_core::ecma::minifier::option::{CompressOptions, ExtraOptions, MinifyOptions};
use swc_core::ecma::parser::{EsSyntax, Parser, StringInput, Syntax, lexer::Lexer};
use swc_core::ecma::transforms::base::fixer::{fixer, paren_remover};
use swc_core::ecma::transforms::base::hygiene::hygiene;
use swc_core::ecma::transforms::base::resolver;

/// 优化失败（或不该动）时返回 `None`；成功给出装好壳剥好的新文本。
pub fn optimize(text: &str) -> Option<String> {
    // 与文本级一致：`JSCD_NO_OPT=1` 全关，便于拿未优化产物对拍
    if std::env::var_os("JSCD_NO_OPT").is_some() {
        return None;
    }
    if text.trim().is_empty() {
        return None;
    }
    // **不包壳**：载荷按脚本喂给 swc。
    //   * swc 只在函数作用域做复制传播（脚本顶层它把绑定当全局对象属性）—— 但那一类折叠
    //     现在由文本层（`opt::rebuild_calls`）完成了；
    //   * 反过来，包成 IIFE 会让 swc 把载荷的顶层声明当"函数内私有"：没人引用就直接删
    //     （`function target` / `Class = ctor` 这类"给外部用"的东西整段消失 —— 产物是给人
    //     读的，丢代码比少优化严重得多）。脚本顶层它一律保留。
    let src = text.to_string();

    let cm: Lrc<SourceMap> = Default::default();
    let fm = cm.new_source_file(FileName::Anon.into(), src);
    let comments = SingleThreadedComments::default();
    let lexer = Lexer::new(
        Syntax::Es(EsSyntax::default()),
        EsVersion::EsNext,
        StringInput::from(&*fm),
        Some(&comments),
    );
    let mut parser = Parser::new_from(lexer);

    GLOBALS.set(&Globals::new(), || {
        let script = parser.parse_script().ok()?;
        // 有恢复性错误 → 这份文本我们没把握，不动它
        if !parser.take_errors().is_empty() {
            return None;
        }
        let mut program = Program::Script(script);
        let unresolved_mark = Mark::new();
        let top_level_mark = Mark::new();
        // 顺序照抄 swc 官方 minify()：paren_remover 必须在 resolver 之前
        program.mutate(&mut paren_remover(Some(&comments as &dyn Comments)));
        program.mutate(&mut resolver(unresolved_mark, top_level_mark, false));
        let options = MinifyOptions {
            compress: Some(compress_options()),
            // 绝不改名：寄存器名 / `__v8name` 别名 / 全局引用都要原样
            mangle: None,
            ..Default::default()
        };
        let extra = ExtraOptions {
            unresolved_mark,
            top_level_mark,
            mangle_name_cache: None,
        };
        let mut program =
            swc_optimize(program, cm.clone(), Some(&comments), None, &options, &extra);
        program.mutate(&mut hygiene());
        program.mutate(&mut fixer(None));

        let body = unwrap_program(program)?;
        let code = print_stmts(&cm, body)?;
        if code.trim().is_empty() {
            // 全被删光（载荷本身没有副作用时会发生）→ 宁可退回文本级结果
            return None;
        }
        // 门禁：剥壳后的文本必须是能解析的脚本（防剥错壳 / 防产物破损）
        let check_fm = cm.new_source_file(FileName::Anon.into(), code.clone());
        let check_lexer = Lexer::new(
            Syntax::Es(EsSyntax::default()),
            EsVersion::EsNext,
            StringInput::from(&*check_fm),
            None,
        );
        let mut check = Parser::new_from(check_lexer);
        check.parse_script().ok()?;
        if !check.take_errors().is_empty() {
            return None;
        }
        Some(code)
    })
}

/// 压缩器选项：能删冗余，但别动"人读的东西"。
fn compress_options() -> CompressOptions {
    CompressOptions {
        // —— 冗余消除的主力 ——
        reduce_vars: true, // 默认 false：不开就没有跨语句的复制传播
        collapse_vars: true,
        unused: true,
        inline: 3,
        // —— 可读性：不改形态 ——
        arrows: false,       // 别把 function 转成箭头（this/arguments/new 语义会变，且产物是给人读的）
        negate_iife: false,  // 别把 `(function(){…})()` 写成 `!function(){…}()`
        join_vars: false,    // 别把多条声明合成 `let a = …, b = …;`
        sequences: 0,        // 别合成逗号表达式
        keep_fnames: true,
        keep_classnames: true,
        keep_fargs: true,    // 参数别删（`fn.length` 可观测）
        keep_infinity: true, // 别把 Infinity 写成 1/0
        drop_console: false,
        ..Default::default()
    }
}

/// 剥出外壳函数的函数体；外壳被压缩器**内联掉**时（小载荷会这样），顶层语句列表就是答案。
fn unwrap_program(program: Program) -> Option<Vec<Stmt>> {
    let Program::Script(script) = program else {
        return None;
    };
    let mut body = script.body;
    if body.len() == 1 {
        match body.into_iter().next()? {
            Stmt::Expr(mut expr_stmt) => {
                if let Some(func) = find_wrapper(&mut expr_stmt.expr) {
                    if let Some(fb) = func.body.as_mut() {
                        return Some(std::mem::take(&mut fb.stmts));
                    }
                }
                // 不是外壳（或没有函数体）→ 仍按普通顶层语句打印
                body = vec![Stmt::Expr(expr_stmt)];
            }
            other => body = vec![other],
        }
    }
    Some(body)
}

/// `(function(){…})()` / `!function(){…}()` / 带括号的等价写法 → 里层函数。
fn find_wrapper(expr: &mut Expr) -> Option<&mut Function> {
    match expr {
        Expr::Paren(p) => find_wrapper(&mut p.expr),
        Expr::Unary(u) if u.op == UnaryOp::Bang => find_wrapper(&mut u.arg),
        Expr::Call(call) => match &mut call.callee {
            Callee::Expr(callee) => find_wrapper(callee),
            _ => None,
        },
        Expr::Fn(f) => Some(&mut f.function),
        _ => None,
    }
}

/// 把语句列表按"顶层脚本"打印出来（缩进/换行保持人读的样子）。
fn print_stmts(cm: &Lrc<SourceMap>, body: Vec<Stmt>) -> Option<String> {
    let script = Script {
        span: Default::default(),
        body,
        shebang: None,
    };
    let mut buf = Vec::new();
    {
        let wr = JsWriter::new(cm.clone(), "\n", &mut buf, None);
        let mut emitter = Emitter {
            cfg: Config::default().with_minify(false),
            cm: cm.clone(),
            comments: None,
            wr,
        };
        emitter.emit_script(&script).ok()?;
    }
    String::from_utf8(buf).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collapses_single_use_copies() {
        // 用户报的那个例子走**整条管线**（文本层 ⑦ + 这里）：寄存器形态进，理想形态出。
        let src = "let r0, r1, r2, r3;\nr0 = greet;\nr2 = console;\nr1 = r2.log;\nr3 = r0(\"world\");\nr2.log(r3);\nfunction greet(a0) {\n  return \"hello \" + a0;\n}\n";
        let text = crate::opt::optimize(src);
        let out = optimize(&text).unwrap_or(text);
        assert!(out.contains("console.log(greet(\"world\"))"), "{out}");
        assert!(out.contains("function greet"), "函数本体要留着：{out}");
    }

    #[test]
    fn keeps_unreferenced_top_level_declarations() {
        // 载荷里"没人引用"的顶层声明**不能删**：产物是给别人读/用的（曾经包 IIFE 优化，
        // swc 把它们当函数内私有死代码整段删掉 —— `function target` 消失过）。
        let src = "someCall();\nfunction target(a0) { return a0; }\nvar Counter;\nCounter = target;\n";
        let out = optimize(src).expect("parse+optimize");
        assert!(out.contains("function target"), "顶层函数声明不许丢：{out}");
        assert!(out.contains("Counter = target"), "顶层赋值不许丢：{out}");
        assert!(out.contains("someCall()"), "{out}");
    }

    #[test]
    fn keeps_registers_with_multiple_uses() {
        let src = "let r0 = greet;\nuse(r0);\nuse(r0);\nfunction greet(a0) { return a0; }\n";
        let out = optimize(src).expect("parse+optimize");
        assert!(out.contains("r0"), "多次读的寄存器不许折叠掉：{out}");
    }

    #[test]
    fn returns_none_on_unparseable_input() {
        assert!(optimize("let r0 = ;;;").is_none());
    }

    #[test]
    fn returns_none_on_empty_input() {
        assert!(optimize("").is_none());
    }

    #[test]
    fn does_not_rename() {
        let src = "let r0 = 1;\nlet r1 = r0 + 1;\nexports.x = r1;\n";
        let out = optimize(src).expect("parse+optimize");
        assert!(out.contains("exports.x"), "全局引用不许改名：{out}");
    }
}
