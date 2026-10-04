//! 命令行定义：两种入场形态（对齐 ddc 的用法）。
//!
//! - `jscd [选项] <输入> [输出]` —— 默认动作就是反编译；输入是目录时递归处理。
//! - `jscd <子命令> [参数…]`   —— info/strings/functions/disasm/ro-map/decompile…
//!
//! `-h/--help`、`-v/-V/--version`、`help`、`version` 由 `main.rs` 先拦下来打印
//! 双语文本（见 `help.rs`），所以 clap 自带的英文帮助/版本旗标是关掉的。

use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;

/// 已知子命令（`main.rs` 判定"这是子命令还是输入路径"用；`help`/`version` 也在内）。
pub const SUBCOMMANDS: &[&str] = &[
    "info",
    "strings",
    "functions",
    "disasm",
    "decompile",
    "ro-map",
    "help",
    "version",
    "debug-parse",
];

/// `jscd help` 里列出的子命令（名字, 英文描述, 中文描述）。
pub const SUBCOMMAND_HELP: &[(&str, &str, &str)] = &[
    (
        "decompile",
        "reconstruct JavaScript (default action: `jscd <INPUT>`)",
        "重建 JavaScript（默认动作，等价于直接 `jscd <输入>`）",
    ),
    (
        "info",
        "header fields and the detected Node/V8 version",
        "头部字段与识别出的 Node/V8 版本",
    ),
    (
        "strings",
        "string and symbol constants from the constant pools",
        "常量池里的字符串与符号",
    ),
    (
        "functions",
        "SharedFunctionInfo tree (names, params, bytecode sizes)",
        "SharedFunctionInfo 树（名字、形参、字节码长度）",
    ),
    (
        "disasm",
        "bytecode disassembly (View8-compatible text)",
        "字节码反汇编（View8 兼容文本）",
    ),
    (
        "ro-map",
        "build a read-only-heap name table from a probe cache",
        "从探针缓存生成只读堆名表",
    ),
    (
        "help",
        "print help (`jscd help <SUBCOMMAND>`)",
        "打印帮助（可跟子命令名）",
    ),
    (
        "version",
        "print name, version and repository",
        "打印名字、版本与仓库地址",
    ),
];

/// 各子命令共享选项。
#[derive(Args, Debug)]
pub struct Common {
    /// 写文件（'-' = stdout，缺省即 stdout）
    #[arg(short, long)]
    pub output: Option<PathBuf>,
    /// 不打印逐文件进度（目录输入时）
    #[arg(long)]
    pub quiet: bool,
}

/// 子命令形态。
#[derive(Parser, Debug)]
#[command(
    name = "jscd",
    disable_help_flag = true,
    disable_version_flag = true,
    about = "Reverse bytenode-compiled .jsc (V8 code cache) back to JavaScript"
)]
pub struct Cli {
    /// 机器可读输出（全局：`jscd --json info f` 与 `jscd info f --json` 均可）
    #[arg(long, global = true)]
    pub json: bool,
    #[command(subcommand)]
    pub cmd: Cmd,
}

#[derive(Subcommand, Debug)]
pub enum Cmd {
    /// 头部字段与版本识别
    Info {
        file: PathBuf,
        #[command(flatten)]
        common: Common,
    },
    /// 常量池字符串 / 符号
    Strings {
        file: PathBuf,
        #[command(flatten)]
        common: Common,
    },
    /// SharedFunctionInfo 树
    Functions {
        file: PathBuf,
        #[command(flatten)]
        common: Common,
    },
    /// 字节码反汇编
    Disasm {
        file: PathBuf,
        #[command(flatten)]
        common: Common,
        /// 只输出名字含该子串的函数
        #[arg(long)]
        filter: Option<String>,
    },
    /// 反编译（文件或目录）
    Decompile {
        file: PathBuf,
        /// 输出文件 / 目录 / `-`
        output: Option<PathBuf>,
        #[command(flatten)]
        common: Common,
        /// 只读堆名表（`jscd ro-map` 生成）
        #[arg(long = "ro-map")]
        ro_map: Option<PathBuf>,
        /// 语法门禁：产物过一遍 `node --check`
        #[arg(long)]
        verify: bool,
        /// 带上"可运行前导"（`__runtime` 占位实现），产物能直接 `node` 跑
        #[arg(long)]
        runtime: bool,
    },
    /// 从探针缓存生成只读堆引用名表
    RoMap {
        file: PathBuf,
        #[command(flatten)]
        common: Common,
    },
    /// (dev) 解析 payload 并导出对象表
    #[command(hide = true)]
    DebugParse {
        file: PathBuf,
    },
}

/// 默认形态：`jscd [选项] <输入> [输出]`（不写子命令就是反编译）。
#[derive(Parser, Debug)]
#[command(
    name = "jscd",
    disable_help_flag = true,
    disable_version_flag = true,
    about = "Reverse bytenode-compiled .jsc (V8 code cache) back to JavaScript"
)]
pub struct Direct {
    #[arg(long)]
    pub json: bool,
    #[arg(short, long)]
    pub output: Option<PathBuf>,
    #[arg(long = "ro-map")]
    pub ro_map: Option<PathBuf>,
    #[arg(long)]
    pub verify: bool,
    /// 带上"可运行前导"（`__runtime` 占位实现）
    #[arg(long)]
    pub runtime: bool,
    #[arg(long)]
    pub quiet: bool,
    /// 输入：`.jsc` 文件，或含 `.jsc` 的目录（递归）
    pub input: PathBuf,
    /// 输出：文件 / 目录 / `-`（缺省：文件 → stdout；目录 → 同级的 `<输入名>-out`）
    pub dest: Option<PathBuf>,
}

impl Direct {
    /// `-o` 与位置参数同时给出且不一致 → 用法错误。
    pub fn resolved_output(&self) -> Result<Option<PathBuf>, String> {
        match (&self.output, &self.dest) {
            (Some(a), Some(b)) if a != b => Err(crate::bif!(
                "-o {0} conflicts with the OUTPUT argument {1}",
                "-o {0} 与位置参数 OUTPUT {1} 冲突";
                a.display(),
                b.display()
            )),
            (Some(a), _) => Ok(Some(a.clone())),
            (None, Some(b)) => Ok(Some(b.clone())),
            (None, None) => Ok(None),
        }
    }
}

/// 把 clap 的报错揉成一句人话（按语言）。
pub fn localize_clap_error(err: &clap::Error) -> String {
    use clap::error::ErrorKind as K;
    // clap 的英文原文里带具体 token，抠出来复用
    let raw = err.to_string();
    let token = raw.split('\'').nth(1).unwrap_or("").to_string();
    let hint = crate::bi!("see `jscd --help`", "用法见 `jscd --help`");
    match err.kind() {
        K::UnknownArgument => crate::bif!(
            "unknown option '{0}' — {1}",
            "未知选项 '{0}' —— {1}";
            token,
            hint
        ),
        K::InvalidSubcommand => crate::bif!(
            "unknown subcommand '{0}' — {1}",
            "未知子命令 '{0}' —— {1}";
            token,
            hint
        ),
        K::MissingRequiredArgument => {
            crate::bif!("missing argument: {0} — {1}", "缺少参数：{0} —— {1}"; raw.trim(), hint)
        }
        K::InvalidValue | K::ValueValidation => crate::bif!(
            "invalid value for '{0}' — {1}",
            "'{0}' 的取值不合法 —— {1}";
            token,
            hint
        ),
        K::TooManyValues | K::WrongNumberOfValues => crate::bif!(
            "too many arguments: {0} — {1}",
            "参数过多：{0} —— {1}";
            raw.trim(),
            hint
        ),
        _ => raw.trim().to_string(),
    }
}