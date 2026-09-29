//! 集成测试：解析真机生成的 probe .jsc（由 mise Node 产出，路径 /tmp/probe_*.jsc）。
//! 样本缺失时跳过（对齐 dae 的 skip_or_fail 模式）——CI 上无本地 Node 也能过。

use std::process::Command;

fn run_jscd(args: &[&str]) -> (i32, String, String) {
    let bin = env!("CARGO_BIN_EXE_jscd");
    let out = Command::new(bin).args(args).output().expect("run jscd");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn probe(name: &str) -> Option<std::path::PathBuf> {
    let p = std::path::PathBuf::from(format!("/tmp/probe_{name}.jsc"));
    p.exists().then_some(p)
}

#[test]
fn parses_node16_probe() {
    let Some(p) = probe("16.20.2") else {
        eprintln!("skip: /tmp/probe_16.20.2.jsc not found");
        return;
    };
    let (code, stdout, stderr) = run_jscd(&["info", p.to_str().unwrap()]);
    assert_eq!(code, 0, "info failed: {stderr}");
    assert!(stdout.contains("9.4.146.26"), "expected exact v8 id: {stdout}");
    assert!(stdout.contains("exact"), "should identify via manifest: {stdout}");
}

#[test]
fn debug_parse_probe_payload() {
    // 用 debug-parse 观察流解析质量；样本缺失时跳过
    let Some(p) = probe("16.20.2") else {
        eprintln!("skip: probe not found");
        return;
    };
    let (code, stdout, stderr) = run_jscd(&["debug-parse", p.to_str().unwrap()]);
    if code != 0 {
        panic!("debug-parse failed: {stderr}");
    }
    println!("{stdout}");
}

#[test]
fn functions_and_strings_smoke() {
    let Some(p) = std::path::PathBuf::from("/tmp/probe_16.20.2.jsc").exists().then(|| {
        std::path::PathBuf::from("/tmp/probe_16.20.2.jsc")
    }) else {
        eprintln!("skip: probe not found");
        return;
    };
    let (code, out, err) = run_jscd(&["functions", p.to_str().unwrap()]);
    assert_eq!(code, 0, "functions failed: {err}");
    assert!(out.contains("[0]"), "expected function ids: {out}");

    let (code, out, err) = run_jscd(&["strings", p.to_str().unwrap()]);
    assert_eq!(code, 0, "strings failed: {err}");
    assert!(!out.trim().is_empty(), "expected some strings");
}
