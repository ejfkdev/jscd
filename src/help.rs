//! 双语 help / version 文本（手写，不用 clap 生成的英文骨架）。
//!
//! 结构对齐 ddc：首行「名字 版本 — 一句话」，第二行仓库地址与许可，语言说明，
//! 描述段，Usage 三行，INPUT / OUTPUT 语义，选项，子命令，示例。

use crate::bi;
use crate::lang::pick_owned;
use std::fmt::Write as _;

pub const REPO: &str = "https://github.com/ejfkdev/jscd";
pub const LICENSE: &str = "MIT";

/// 一行版本信息：`jscd <ver> — V8 code cache → JavaScript` + 仓库 + 许可。
pub fn version_line() -> String {
    let ver = env!("JSCD_VERSION");
    crate::bif!("jscd {0} — V8 code cache → JavaScript", "jscd {0} — V8 代码缓存 → JavaScript"; ver)
}

/// `jscd --version` / `-v` / `version` 的输出。
pub fn version_text() -> String {
    let repo = pick_owned(format!("repo:    {REPO}"), format!("仓库：   {REPO}"));
    let lic = pick_owned(format!("{LICENSE} license"), format!("{LICENSE} 许可"));
    format!("{}\n{repo}  ({lic})\n", version_line())
}

/// 主 help。`secs` 为子命令名（由 args.rs 传入，避免两处名单漂移）。
pub fn help_text() -> String {
    let ver = env!("JSCD_VERSION");
    let head = crate::bif!(
        "{0} {1} — V8 code cache → JavaScript",
        "{0} {1} — V8 代码缓存 → JavaScript";
        "jscd",
        ver
    );
    let mut s = String::new();
    let _ = writeln!(s, "{head}");
    let _ = writeln!(s, "{REPO}  ({LICENSE} license)");
    let _ = writeln!(s);
    let _ = writeln!(
        s,
        "{}",
        bi!(
            "Reconstructs JavaScript from bytenode-compiled .jsc files (V8 code caches).\n\
             Static parsing — no patched V8, no Node runtime. Directories are scanned\n\
             recursively.",
            "把 bytenode 编译的 .jsc（V8 代码缓存）还原成 JavaScript。纯静态解析 ——\n\
             不需要打过补丁的 V8，也不需要 Node 运行时；目录输入会递归处理。"
        )
    );
    let _ = writeln!(s);
    let _ = writeln!(
        s,
        "{}",
        bi!(
            "Usage: jscd [OPTIONS] <INPUT> [OUTPUT]     # decompile (default action)\n\
             \x20      jscd <SUBCOMMAND> [ARGS...]         # stage-by-stage analysis\n\
             \x20      jscd help [SUBCOMMAND] | version | -h | -v | -V",
            "用法：jscd [选项] <输入> [输出]              # 默认动作就是反编译\n\
             \x20     jscd <子命令> [参数...]              # 分阶段分析\n\
             \x20     jscd help [子命令] | version | -h | -v | -V"
        )
    );
    let _ = writeln!(s);
    let _ = writeln!(
        s,
        "{}",
        bi!(
            "INPUT   a .jsc file, or a directory (scanned recursively for *.jsc).\n\
             OUTPUT  a file, a directory, or `-` for stdout:\n\
             \x20         <file.js>   one input file\n\
             \x20         <dir>       directory input, tree mirrored\n\
             \x20         default: stdout for a file; <INPUT>-out/ next to it for a directory",
            "输入    .jsc 文件，或目录（递归找 *.jsc）。\n\
             输出    文件、目录，或 `-` 表示 stdout：\n\
             \x20         <文件.js>   单个输入文件时写这个文件\n\
             \x20         <目录>      目录输入时写入该目录，保持层级\n\
             \x20         默认：文件输入 → stdout；目录输入 → 输入同级的 <输入名>-out/"
        )
    );
    let _ = writeln!(s);
    let _ = writeln!(s, "{}", bi!("Options:", "选项："));
    for (en, zh) in [
        (
            "  -o, --output <path>   output file / directory / - (same as OUTPUT)",
            "  -o, --output <路径>   输出文件 / 目录 / -（与位置参数 OUTPUT 等价）",
        ),
        (
            "      --ro-map <path>   read-only-heap name table (decompile)",
            "      --ro-map <路径>   只读堆名表（decompile 用，还原内建属性名）",
        ),
        (
            "      --verify          syntax-gate the result with `node --check` (decompile)",
            "      --verify          用 `node --check` 实编译校验产物（decompile）",
        ),
        (
            "      --runtime         keep the runnable prelude (`__runtime` stubs) so the output\n\
             \x20                       can be run with node; off by default — the output is\n\
             \x20                       then just the reconstructed code",
            "      --runtime         保留可运行前导（`__runtime` 占位实现），产物能直接 `node` 跑；\n\
             \x20                       默认**不带**，只输出还原出来的代码本身",
        ),
        (
            "      --filter <substr> only functions whose name contains substr (disasm)",
            "      --filter <子串>   只输出名字含该子串的函数（disasm）",
        ),
        (
            "      --json            machine-readable output (info/strings/functions/disasm/ro-map)",
            "      --json            机器可读输出（info/strings/functions/disasm/ro-map）",
        ),
        (
            "      --quiet           suppress per-file progress (directory input)",
            "      --quiet           不打印逐文件进度（目录输入时）",
        ),
        (
            "  -h, --help            print this help",
            "  -h, --help            打印本帮助",
        ),
        (
            "  -v, -V, --version     print name, version, repository",
            "  -v, -V, --version     打印名字、版本与仓库地址",
        ),
        (
            "      JSCD_LANG=zh|en   force the help/error language (default: auto-detect)",
            "      JSCD_LANG=zh|en   强制帮助/报错的语言（默认自动识别）",
        ),
    ] {
        let _ = writeln!(s, "{}", bi!(en, zh));
    }
    let _ = writeln!(s);
    let _ = writeln!(s, "{}", bi!("Subcommands:", "子命令："));
    for (name, en, zh) in crate::args::SUBCOMMAND_HELP {
        let desc = bi!(en, zh);
        let _ = writeln!(s, "  {name:<11} {desc}");
    }
    let _ = writeln!(s);
    let _ = writeln!(s, "{}", bi!("Examples:", "示例："));
    for (en, zh) in [
        (
            "  jscd app.jsc                        decompile to stdout",
            "  jscd app.jsc                        反编译并打到 stdout",
        ),
        (
            "  jscd app.jsc app.js                 decompile to a file",
            "  jscd app.jsc app.js                 反编译到文件",
        ),
        (
            "  jscd dist/                          dist/**.jsc → dist-out/**.js",
            "  jscd dist/                          dist/**.jsc → dist-out/**.js",
        ),
        (
            "  jscd dist/ out/                     ... into out/ instead",
            "  jscd dist/ out/                     ……改写到 out/",
        ),
        (
            "  jscd decompile --ro-map m.json a.jsc   with a name table",
            "  jscd decompile --ro-map m.json a.jsc   带只读堆名表",
        ),
        (
            "  jscd info app.jsc                   header fields and V8 version",
            "  jscd info app.jsc                   头部字段与 V8 版本",
        ),
        (
            "  jscd disasm --filter main app.jsc   one function's bytecode",
            "  jscd disasm --filter main app.jsc   只看某个函数的字节码",
        ),
        (
            "  jscd help disasm                    per-subcommand help with examples",
            "  jscd help disasm                    单个子命令的详细帮助（带示例）",
        ),
        (
            "  jscd ro-map probe.jsc -o names.json\n\
             \x20                                     build a name table (see `jscd help ro-map`)",
            "  jscd ro-map probe.jsc -o names.json\n\
             \x20                                     生成只读堆名表（见 `jscd help ro-map`）",
        ),
    ] {
        let _ = writeln!(s, "{}", bi!(en, zh));
    }
    s
}

/// 单个子命令的详细帮助（`jscd help info` / `jscd info --help`）。
///
/// 每个子命令都给出：用法、补充说明、自己的选项、可直接抄的示例。
pub fn subcommand_help(name: &str) -> Option<String> {
    let (_, en, zh) = crate::args::SUBCOMMAND_HELP
        .iter()
        .find(|(n, _, _)| *n == name)?;
    type Lines = &'static [(&'static str, &'static str)];
    let (usage, blurb, options, examples): (&str, &str, Lines, Lines) = match name {
        "decompile" => (
            bi!(
                "Usage: jscd decompile [OPTIONS] <INPUT> [OUTPUT]\n\
                 \x20      jscd [OPTIONS] <INPUT> [OUTPUT]     # same thing",
                "用法：jscd decompile [选项] <输入> [输出]\n\
                 \x20     jscd [选项] <输入> [输出]           # 等价写法"
            ),
            bi!(
                "INPUT is a .jsc file, or a directory (scanned recursively for *.jsc).\n\
                 OUTPUT is a file, a directory, or `-` for stdout. Default: stdout for a\n\
                 file input; <INPUT>-out/ next to it for a directory.",
                "输入是 .jsc 文件，或目录（递归找 *.jsc）。输出是文件、目录或 `-`（stdout）。\n\
                 默认：文件输入 → stdout；目录输入 → 输入同级的 <输入名>-out/。"
            ),
            &[
                (
                    "  -o, --output <path>   output file / directory / - (default: stdout)",
                    "  -o, --output <路径>   输出文件 / 目录 / -（默认 stdout）",
                ),
                (
                    "      --ro-map <path>   read-only-heap name table from `jscd ro-map`;\n\
                     \x20                       restores builtin names (`a.length`, `o.push`)",
                    "      --ro-map <路径>   只读堆名表（`jscd ro-map` 生成）；\n\
                     \x20                       用来还原内建属性名（`a.length`、`o.push`）",
                ),
                (
                    "      --verify          syntax-gate the result with `node --check`",
                    "      --verify          用 `node --check` 实编译校验产物",
                ),
                (
                    "      --runtime         keep the runnable prelude (`__runtime` stubs) so the\n\
                     \x20                       output runs under node; off by default",
                    "      --runtime         保留可运行前导（`__runtime` 占位实现），产物能直接跑；\n\
                     \x20                       默认不带，只输出还原出来的代码",
                ),
                (
                    "      --quiet           suppress per-file progress (directory input)",
                    "      --quiet           不打印逐文件进度（目录输入时）",
                ),
            ],
            &[
                (
                    "  jscd decompile app.jsc                 decompile to stdout",
                    "  jscd decompile app.jsc                 反编译并打到 stdout",
                ),
                (
                    "  jscd decompile app.jsc app.js          write to app.js",
                    "  jscd decompile app.jsc app.js          写到 app.js",
                ),
                (
                    "  jscd decompile dist/                   dist/**.jsc -> dist-out/**.js",
                    "  jscd decompile dist/                   dist/**.jsc -> dist-out/**.js",
                ),
                (
                    "  jscd --ro-map names.json a.jsc > a.js  with a read-only-heap name table",
                    "  jscd --ro-map names.json a.jsc > a.js  带只读堆名表",
                ),
                (
                    "  jscd decompile --runtime --verify a.jsc -o a.run.js\n\
                     \x20                                        runnable, syntax-checked output",
                    "  jscd decompile --runtime --verify a.jsc -o a.run.js\n\
                     \x20                                        可运行、已语法校验的产物",
                ),
            ],
        ),
        "info" => (
            bi!(
                "Usage: jscd info [OPTIONS] <FILE>",
                "用法：jscd info [选项] <文件>"
            ),
            bi!(
                "Prints the code-cache header: magic, version hash, V8/Node version, flags,\n\
                 payload size, and whether this build supports that V8 version.",
                "打印代码缓存头部：magic、版本哈希、V8/Node 版本、旗标、payload 大小，\n\
                 以及当前版本是否支持该 V8 版本。"
            ),
            &[
                (
                    "  -o, --output <path>   write to a file ('-' = stdout; default stdout)",
                    "  -o, --output <路径>   写到文件（'-' = stdout；默认就是 stdout）",
                ),
                (
                    "      --json            one JSON object: file, size_raw, brotli,\n\
                     \x20                       version_hash, v8, node, supported, ...",
                    "      --json            单个 JSON 对象：file、size_raw、brotli、\n\
                     \x20                       version_hash、v8、node、supported 等",
                ),
            ],
            &[
                (
                    "  jscd info app.jsc                      header fields and V8 version",
                    "  jscd info app.jsc                      头部字段与 V8 版本",
                ),
                (
                    "  jscd info --json app.jsc | jq .supported   is this V8 version supported?",
                    "  jscd info --json app.jsc | jq .supported   这个 V8 版本受支持吗？",
                ),
            ],
        ),
        "strings" => (
            bi!(
                "Usage: jscd strings [OPTIONS] <FILE>",
                "用法：jscd strings [选项] <文件>"
            ),
            bi!(
                "Dumps the string/symbol constants this cache can reference, one per line.\n\
                 The JSON `from_roots` list is the subset reachable from the root function.",
                "把该缓存能引用到的字符串/符号常量逐行列出。JSON 里的 `from_roots`\n\
                 是从根函数可达的那一部分。"
            ),
            &[
                (
                    "  -o, --output <path>   write to a file ('-' = stdout; default stdout)",
                    "  -o, --output <路径>   写到文件（'-' = stdout；默认就是 stdout）",
                ),
                (
                    "      --json            {\"count\":N,\"strings\":[...],\"from_roots\":[...]}",
                    "      --json            {\"count\":N,\"strings\":[...],\"from_roots\":[...]}",
                ),
            ],
            &[
                (
                    "  jscd strings app.jsc                   all constants, one per line",
                    "  jscd strings app.jsc                   所有常量，一行一个",
                ),
                (
                    "  jscd strings --json app.jsc | jq .count    how many constants",
                    "  jscd strings --json app.jsc | jq .count    数一数有多少条",
                ),
            ],
        ),
        "functions" => (
            bi!(
                "Usage: jscd functions [OPTIONS] <FILE>",
                "用法：jscd functions [选项] <文件>"
            ),
            bi!(
                "Lists the SharedFunctionInfo tree: id, name, parameter count, bytecode\n\
                 length, frame size, whether it was compiled, and nesting depth.",
                "列出 SharedFunctionInfo 树：id、名字、形参个数、字节码长度、栈帧大小、\n\
                 是否已编译、嵌套深度。"
            ),
            &[
                (
                    "  -o, --output <path>   write to a file ('-' = stdout; default stdout)",
                    "  -o, --output <路径>   写到文件（'-' = stdout；默认就是 stdout）",
                ),
                (
                    "      --json            {\"count\":N,\"functions\":[{\"id\",\"name\",\"params\",\n\
                     \x20                       \"bytecode_length\",\"frame_size\",\"compiled\",\"depth\"}]}",
                    "      --json            {\"count\":N,\"functions\":[{\"id\",\"name\",\"params\"、\n\
                     \x20                       \"bytecode_length\"、\"frame_size\"、\"compiled\"、\"depth\"}]}",
                ),
            ],
            &[
                (
                    "  jscd functions app.jsc                 the whole SFI tree",
                    "  jscd functions app.jsc                 整棵 SFI 树",
                ),
                (
                    "  jscd functions --json app.jsc | jq '.functions[].name'",
                    "  jscd functions --json app.jsc | jq '.functions[].name'",
                ),
            ],
        ),
        "disasm" => (
            bi!(
                "Usage: jscd disasm [OPTIONS] <FILE>",
                "用法：jscd disasm [选项] <文件>"
            ),
            bi!(
                "Prints View8-compatible bytecode disassembly for every function, or only\n\
                 the ones whose name contains --filter.",
                "打印 View8 兼容的字节码反汇编：默认每个函数都打，`--filter` 只看\n\
                 名字含该子串的那些。"
            ),
            &[
                (
                    "      --filter <substr> only functions whose name contains substr",
                    "      --filter <子串>   只输出名字含该子串的函数",
                ),
                (
                    "  -o, --output <path>   write to a file ('-' = stdout; default stdout)",
                    "  -o, --output <路径>   写到文件（'-' = stdout；默认就是 stdout）",
                ),
                (
                    "      --json            {\"disassembly\":\"<the whole listing>\"}",
                    "      --json            {\"disassembly\":\"<完整反汇编文本>\"}",
                ),
            ],
            &[
                (
                    "  jscd disasm app.jsc                    full bytecode listing",
                    "  jscd disasm app.jsc                    完整字节码清单",
                ),
                (
                    "  jscd disasm --filter main app.jsc      only functions named *main*",
                    "  jscd disasm --filter main app.jsc      只看名字含 main 的函数",
                ),
                (
                    "  jscd disasm --json app.jsc > d.json    machine-readable listing",
                    "  jscd disasm --json app.jsc > d.json    机器可读的清单",
                ),
            ],
        ),
        "ro-map" => (
            bi!(
                "Usage: jscd ro-map [OPTIONS] <PROBE.jsc>",
                "用法：jscd ro-map [选项] <探针.jsc>"
            ),
            bi!(
                "Walks a probe cache's read-only heap and writes a name table (root index ->\n\
                 property name), so decompiled output says `a.length` instead of `<ro0_1120>`.\n\
                 Build one table per V8 version (see docs/VERSIONS.md for the probe recipe).",
                "走一遍探针缓存的只读堆，写出「根索引 -> 属性名」的名表，反编译产物里\n\
                 就会是 `a.length` 而不是 `<ro0_1120>`。每个 V8 版本建一张\n\
                 （探针构造方法见 docs/VERSIONS.md）。"
            ),
            &[
                (
                    "  -o, --output <path>   write the table to a file (default: stdout)",
                    "  -o, --output <路径>   把名表写到文件（默认 stdout）",
                ),
                (
                    "      --json            {\"schema\",\"v8\",\"tags\",\"entries\":{\"0/1120\":\"length\"}}",
                    "      --json            {\"schema\",\"v8\",\"tags\",\"entries\":{\"0/1120\":\"length\"}}",
                ),
            ],
            &[
                (
                    "  jscd ro-map probe.jsc -o names.json    build a name table",
                    "  jscd ro-map probe.jsc -o names.json    生成一张名表",
                ),
                (
                    "  jscd ro-map probe.jsc | head           peek at root 0's names",
                    "  jscd ro-map probe.jsc | head           瞄一眼根 0 的名字",
                ),
                (
                    "  jscd ro-map --json probe.jsc | jq '.entries'   just the entries",
                    "  jscd ro-map --json probe.jsc | jq '.entries'   只看 entries",
                ),
            ],
        ),
        "help" => (
            bi!(
                "Usage: jscd help [SUBCOMMAND]",
                "用法：jscd help [子命令]"
            ),
            bi!(
                "Prints the main help, or one subcommand's help (usage, options, examples).",
                "打印主帮助；带子命令名时打印那个子命令的帮助（用法、选项、示例）。"
            ),
            &[],
            &[
                (
                    "  jscd help                              main help",
                    "  jscd help                              主帮助",
                ),
                (
                    "  jscd help disasm                       the disasm page with examples",
                    "  jscd help disasm                       disasm 的详细帮助（带示例）",
                ),
            ],
        ),
        "version" => (
            bi!("Usage: jscd version", "用法：jscd version"),
            bi!(
                "Prints name, version, repository and license (`-v` / `-V` / `--version` too).",
                "打印名字、版本、仓库地址与许可（`-v` / `-V` / `--version` 同效）。"
            ),
            &[],
            &[
                (
                    "  jscd version                           same as `jscd -v`",
                    "  jscd version                           与 `jscd -v` 相同",
                ),
            ],
        ),
        _ => return None,
    };
    let mut s = String::new();
    let _ = writeln!(
        s,
        "{}",
        crate::bif!("jscd {0} — {1}", "jscd {0} — {1}"; name, bi!(en, zh))
    );
    let _ = writeln!(s, "{REPO}  ({LICENSE} license)");
    let _ = writeln!(s);
    let _ = writeln!(s, "{usage}");
    let _ = writeln!(s);
    for line in blurb.lines() {
        let _ = writeln!(s, "{line}");
    }
    let _ = writeln!(s);
    if !options.is_empty() {
        let _ = writeln!(s, "{}", bi!("Options:", "选项："));
        for (en, zh) in options {
            let _ = writeln!(s, "{}", bi!(en, zh));
        }
        let _ = writeln!(s);
    }
    if !examples.is_empty() {
        let _ = writeln!(s, "{}", bi!("Examples:", "示例："));
        for (en, zh) in examples {
            let _ = writeln!(s, "{}", bi!(en, zh));
        }
        let _ = writeln!(s);
    }
    let _ = writeln!(
        s,
        "{}",
        bi!(
            "Global: --json works on every subcommand; `jscd --help` lists subcommands and options.",
            "全局：每个子命令都支持 --json；子命令与全局选项见 `jscd --help`。"
        )
    );
    Some(s)
}

/// 是否认识这个子命令。
pub fn is_subcommand(name: &str) -> bool {
    crate::args::SUBCOMMANDS.contains(&name)
}