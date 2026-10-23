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
    let child = std::thread::Builder::new()
        .stack_size(4096 * 1024 * 1024)
        .spawn(real_main)
        .expect("failed to spawn main thread");
    let code = child.join().expect("main thread panicked");
    exit(code);
}

fn real_main() -> i32 {
    let argv: Vec<String> = env::args().collect();
    let args: Vec<&str> = argv[1..].iter().map(|s| s.as_str()).collect();

    let _ = lang::lang();

    if args.is_empty() {
        return out::print(&help::help_text());
    }

    let first_cmd = args.iter().find(|a| !a.starts_with('-')).copied().unwrap_or("");

    if args.iter().any(|a| *a == "-h" || *a == "--help" || *a == "help") {
        let sub = if first_cmd == "help" {
            args.iter().skip_while(|a| **a != "help").nth(1).copied()
        } else {
            Some(first_cmd).filter(|c| SUBCOMMANDS.contains(c))
        };
        let text = sub
            .and_then(help::subcommand_help)
            .unwrap_or_else(help::help_text);
        return out::print(&text);
    }

    if args.iter().any(|a| *a == "-v" || *a == "-V" || *a == "--version") || first_cmd == "version"
    {
        return out::print(&help::version_text());
    }

    if SUBCOMMANDS.contains(&first_cmd) {
        let cli = Cli::try_parse_from(&argv).unwrap_or_else(|e| {
            out::stderr_line(&format!("jscd: {}", localize_clap_error(&e)));
            exit(2);
        });
        return run(cli::run_cmd(cli));
    }

    let direct = Direct::try_parse_from(&argv).unwrap_or_else(|e| {
        out::stderr_line(&format!("jscd: {}", localize_clap_error(&e)));
        exit(2);
    });
    let output = match direct.resolved_output() {
        Ok(o) => o,
        Err(msg) => {
            out::stderr_line(&format!("jscd: {msg}"));
            return 2;
        }
    };
    run(cli::decompile_path(
        &direct.input,
        output.as_deref(),
        direct.quiet,
        direct.ro_map.as_deref(),
        direct.verify,
        direct.json,
        direct.runtime,
    ))
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
