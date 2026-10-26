//! AST 级优化：用 [cure](https://github.com/ejfkdev/cure) 的 TS/JS 后端做语义保持的
//! 简化与格式化（替代原来的 swc 压缩器）。
//!
//! cure 的规则引擎面向反编译产物：保留语句结构、不改名，只做可证明等价的折叠与清理。
//! 失败（解析错误 / 产物为空）返回 `None`，调用方原样使用文本级结果。

use cure_engine::Config;

/// 优化失败（或不该动）时返回 `None`；成功给出简化后的文本。
pub fn optimize(text: &str) -> Option<String> {
    // 与文本级一致：`JSCD_NO_OPT=1` 全关，便于拿未优化产物对拍
    if std::env::var_os("JSCD_NO_OPT").is_some() {
        return None;
    }
    if text.trim().is_empty() {
        return None;
    }
    let mut parsed = cure_ts_parser::parse_js(text);
    // 恢复性错误说明有我们没把握的区域，交给文本级结果
    if !parsed.errors.is_empty() {
        return None;
    }
    let cfg = Config::default();
    let _ = cure_ts_simplify::simplify_unit(&mut parsed.ast, &mut parsed.unit, &cfg);
    let printed = cure_ts_print::print_module(&parsed.ast, &parsed.unit);
    if printed.trim().is_empty() {
        None
    } else {
        Some(printed)
    }
}

#[cfg(test)]
mod tests {
    use super::optimize;

    #[test]
    fn keeps_unreferenced_top_level_declarations() {
        // 载荷里"没人引用"的顶层声明不能删：产物是给别人读/用的。
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
