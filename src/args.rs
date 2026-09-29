//! clap 命令树（对齐 dae：平铺单层子命令 + 共享选项 flatten）。

use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;

/// 已知子命令表（main.rs 快捷形式判定用）。
pub const SUBCOMMANDS: &[&str] = &[
    "info",
    "strings",
    "functions",
    "disasm",
    "decompile",
    "ro-map",
    "version",
    "debug-parse",
    "help",
];

#[derive(Parser)]
#[command(
    name = "jscd",
    version = env!("JSCD_VERSION"),
    about = "Reverse bytenode-compiled .jsc (V8 code cache) back to JavaScript",
    long_about = None
)]
pub struct Cli {
    /// Emit machine-readable JSON（全局：`jscd --json info f` 与 `jscd info f --json` 均可）
    #[arg(long, global = true)]
    pub json: bool,
    #[command(subcommand)]
    pub cmd: Cmd,
}

#[derive(Subcommand)]
pub enum Cmd {
    /// Show .jsc header fields and the detected Node/V8 version
    Info {
        file: PathBuf,
        #[command(flatten)]
        common: Common,
    },
    /// Extract string constants / symbols from the constant pools
    Strings {
        file: PathBuf,
        #[command(flatten)]
        common: Common,
    },
    /// List the SharedFunctionInfo tree (function names, bytecode sizes)
    Functions {
        file: PathBuf,
        #[command(flatten)]
        common: Common,
    },
    /// Disassemble bytecode (View8-compatible text format)
    Disasm {
        file: PathBuf,
        #[command(flatten)]
        common: Common,
        /// Only emit functions whose name contains this substring
        #[arg(long)]
        filter: Option<String>,
    },
    /// Reconstruct JavaScript from bytecode
    Decompile {
        file: PathBuf,
        #[command(flatten)]
        common: Common,
        /// 只读堆名表（由 `jscd ro-map` 生成；用于还原属性名）
        #[arg(long)]
        ro_map: Option<PathBuf>,
        /// 语法门禁：用 node --check 实编译校验产物（需要 PATH 里有 node）
        #[arg(long)]
        verify: bool,
    },
    /// (probe) 从"探针 jsc"生成只读堆引用名表
    RoMap {
        file: PathBuf,
        #[command(flatten)]
        common: Common,
    },
    /// Print version
    Version,
    /// (dev) Parse the payload stream and dump the object table
    #[command(hide = true)]
    DebugParse {
        file: PathBuf,
    },
}

/// 各子命令共享选项。
#[derive(Args)]
pub struct Common {
    /// Write output to file ('-' = stdout, the default)
    #[arg(short, long)]
    pub output: Option<PathBuf>,
}
