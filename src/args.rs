//! clap 命令树（对齐 dae：平铺单层子命令 + 共享选项 flatten）。

use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;

/// 已知子命令表（main.rs 快捷形式判定用）。
pub const SUBCOMMANDS: &[&str] = &[
    "info", "strings", "functions", "disasm", "decompile", "version", "debug-parse", "help",
];

#[derive(Parser)]
#[command(
    name = "jscd",
    version = env!("JSCD_VERSION"),
    about = "Reverse bytenode-compiled .jsc (V8 code cache) back to JavaScript",
    long_about = None
)]
pub struct Cli {
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
    /// Emit machine-readable JSON
    #[arg(long)]
    pub json: bool,
    /// Write output to file ('-' = stdout, the default)
    #[arg(short, long)]
    pub output: Option<PathBuf>,
}
