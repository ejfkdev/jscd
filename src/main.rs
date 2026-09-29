//! 薄入口：快捷形式补 `decompile`，其余交给 clap + cli::run_cmd。

use clap::Parser as _;
use jscd::args::{Cli, SUBCOMMANDS};
use jscd::cli;
use std::env;

fn main() {
    let mut argv: Vec<String> = env::args().collect();
    // 快捷形式（对齐 dae）：第一个参数不是已知子命令、也不是 flag，
    // 则视为反编译目标，自动补 decompile：`jscd app.jsc` ≡ `jscd decompile app.jsc`。
    if argv.len() > 1 && !SUBCOMMANDS.contains(&argv[1].as_str()) && !argv[1].starts_with('-') {
        argv.insert(1, "decompile".to_string());
    }
    let cli = Cli::parse_from(argv);
    let code = match cli::run_cmd(cli) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("jscd: {e}");
            1
        }
    };
    std::process::exit(code);
}
