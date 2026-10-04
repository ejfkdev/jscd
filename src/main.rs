//! 入口：先处理 help/version 与"输入即反编译"的默认形态，再交给 clap。

use clap::Parser as _;
use jscd::args::{localize_clap_error, Cli, Direct, SUBCOMMANDS};
use jscd::cli;
use jscd::help;
use jscd::lang;
use jscd::out;
use std::env;
use std::process::exit;

fn main() {
    let argv: Vec<String> = env::args().collect();
    let args: Vec<&str> = argv[1..].iter().map(|s| s.as_str()).collect();

    // 语言先定下来：后面每一句输出都跟着它走
    let _ = lang::lang();

    // ① 空参数 → help（退出码 0，不是错误）
    if args.is_empty() {
        exit(out::print(&help::help_text()));
    }

    // ② 第一个"看起来像命令"的位置参数（跳过 flag）
    let first_cmd = args.iter().find(|a| !a.starts_with('-')).copied().unwrap_or("");

    // ③ -h/--help（任意位置）；`jscd help <子命令>` 与 `jscd <子命令> --help` 给对应子命令的帮助
    if args.iter().any(|a| *a == "-h" || *a == "--help" || *a == "help") {
        let sub = if first_cmd == "help" {
            args.iter().skip_while(|a| **a != "help").nth(1).copied()
        } else {
            Some(first_cmd).filter(|c| SUBCOMMANDS.contains(c))
        };
        let text = sub
            .and_then(help::subcommand_help)
            .unwrap_or_else(help::help_text);
        exit(out::print(&text));
    }

    // ④ -v/-V/--version 与 `jscd version`
    if args.iter().any(|a| *a == "-v" || *a == "-V" || *a == "--version") || first_cmd == "version"
    {
        exit(out::print(&help::version_text()));
    }

    // ⑤ 子命令形态
    if SUBCOMMANDS.contains(&first_cmd) {
        let cli = Cli::try_parse_from(&argv).unwrap_or_else(|e| {
            out::stderr_line(&format!("jscd: {}", localize_clap_error(&e)));
            exit(2);
        });
        exit(run(cli::run_cmd(cli)));
    }

    // ⑥ 默认形态：第一个位置参数就是输入（文件或目录）
    let direct = Direct::try_parse_from(&argv).unwrap_or_else(|e| {
        out::stderr_line(&format!("jscd: {}", localize_clap_error(&e)));
        exit(2);
    });
    let output = match direct.resolved_output() {
        Ok(o) => o,
        Err(msg) => {
            out::stderr_line(&format!("jscd: {msg}"));
            exit(2);
        }
    };
    exit(run(cli::decompile_path(
        &direct.input,
        output.as_deref(),
        direct.quiet,
        direct.ro_map.as_deref(),
        direct.verify,
        direct.json,
        direct.runtime,
    )));
}

fn run(r: Result<(), String>) -> i32 {
    match r {
        Ok(()) => 0,
        Err(e) => {
            out::stderr_line(&format!("jscd: {e}"));
            1
        }
    }
}