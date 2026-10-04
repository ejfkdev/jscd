//! 终端输出的小工具：断管（EPIPE）不算错误，也不能崩。
//!
//! `jscd x.jsc | head -3`、`| grep -q …` 这类用法会让下游先关掉管道，此时写 stdout
//! 得到 `BrokenPipe`；`println!` 遇到它直接 panic（Rust 默认把 SIGPIPE 忽略掉，
//! 于是写失败走到 panic 分支），终端里就会看到 "thread 'main' panicked"。
//! 凡是要吐给终端的内容都走这里，按 Unix 惯例安静收场。

use std::io::Write;

/// 写 stdout 并 flush。断管视为正常结束（下游看够了）；其余 IO 错误原样返回给调用方。
pub fn stdout(s: &str) -> std::io::Result<()> {
    let mut out = std::io::stdout().lock();
    match out.write_all(s.as_bytes()).and_then(|()| out.flush()) {
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
        other => other,
    }
}

/// 写一行 stderr。写失败一律咽掉 —— 报错的地方本身都不通了，也不能因此崩。
pub fn stderr_line(s: &str) {
    let mut err = std::io::stderr().lock();
    let _ = err.write_all(s.as_bytes());
    let _ = err.write_all(b"\n");
    let _ = err.flush();
}

/// 打一整段到 stdout，直接给退出码：成功 0、写失败报一行并给 1、断管 0。
pub fn print(s: &str) -> i32 {
    match stdout(s) {
        Ok(()) => 0,
        Err(e) => {
            stderr_line(&format!("jscd: {e}"));
            1
        }
    }
}