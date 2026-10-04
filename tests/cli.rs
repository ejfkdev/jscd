//! CLI 行为验收：help / 版本 / 语言切换 / 文件与目录两种默认路径 / 错误码。
//!
//! 需要 `.jsc` 的用例会现场用 `node scripts/mkcorpus.js` 编译一份（对齐 bytenode 的做法）；
//! 环境里没有 `node` 时这些用例**跳过**而不是失败 —— CI 里只跑 `cargo test` 也能过。

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn jscd(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_jscd"))
        .args(args)
        .output()
        .expect("run jscd")
}

/// 带环境变量的调用（语言相关的用例靠它，不动进程级 env）。
fn jscd_env(args: &[&str], envs: &[(&str, &str)]) -> Output {
    let mut c = Command::new(env!("CARGO_BIN_EXE_jscd"));
    c.args(args);
    for (k, v) in envs {
        c.env(k, v);
    }
    c.output().expect("run jscd")
}

fn out(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).to_string()
}

fn err(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).to_string()
}

const REPO: &str = "https://github.com/ejfkdev/jscd";

#[test]
fn empty_args_print_help() {
    let o = jscd(&[]);
    assert!(o.status.success(), "空参数应退出 0");
    let s = out(&o);
    assert!(s.contains("Usage:") || s.contains("用法："), "help 里要有 Usage：{s}");
    assert!(s.contains(REPO), "help 里要有仓库地址");
    assert!(s.contains("jscd "), "help 里要有名字与版本");
    assert!(s.contains("Examples:") || s.contains("示例："), "help 里要有示例");
}

#[test]
fn help_flags_and_subcommand_help() {
    for args in [
        vec!["-h"],
        vec!["--help"],
        vec!["help"],
        vec!["help", "info"],
        vec!["info", "--help"],
    ] {
        let o = jscd(&args);
        assert!(o.status.success(), "{args:?} 应退出 0");
        assert!(out(&o).contains(REPO), "{args:?} 要带仓库地址");
    }
    // 子命令帮助要收缩到该子命令
    let s = out(&jscd(&["help", "info"]));
    assert!(s.contains("info"), "子命令帮助要提到 info");
    assert!(s.contains("用法：") || s.contains("Usage:"), "子命令帮助要有用法行");
}

#[test]
fn version_flags() {
    for args in [vec!["-v"], vec!["-V"], vec!["--version"], vec!["version"]] {
        let o = jscd(&args);
        assert!(o.status.success(), "{args:?} 应退出 0");
        let s = out(&o);
        assert!(s.contains("jscd "), "{args:?} 要打印名字");
        assert!(s.contains(REPO), "{args:?} 要打印仓库地址");
    }
}

#[test]
fn language_follows_env() {
    // 显式中文
    let zh = out(&jscd_env(&["-v"], &[("JSCD_LANG", "zh"), ("LC_ALL", "")]));
    assert!(zh.contains("仓库："), "JSCD_LANG=zh → 中文：{zh}");
    // 显式英文压过 LANG=zh
    let en = out(&jscd_env(
        &["-v"],
        &[("JSCD_LANG", "en"), ("LANG", "zh_CN.UTF-8"), ("LC_ALL", "")],
    ));
    assert!(en.contains("repo:"), "JSCD_LANG=en → 英文：{en}");
    // 繁体/港澳也算中文
    for loc in ["zh_TW.UTF-8", "zh_HK", "zh_MO", "zh_SG"] {
        let s = out(&jscd_env(&["-v"], &[("LC_ALL", loc), ("JSCD_LANG", "")]));
        assert!(s.contains("仓库："), "LC_ALL={loc} → 中文：{s}");
    }
    // 其它语言 → 英文
    for loc in ["fr_FR.UTF-8", "ja_JP", "en_US.UTF-8"] {
        let s = out(&jscd_env(&["-v"], &[("LC_ALL", loc), ("JSCD_LANG", "")]));
        assert!(s.contains("repo:"), "LC_ALL={loc} → 英文：{s}");
    }
}

#[test]
fn usage_errors_are_localized_and_exit_2() {
    let o = jscd(&["--definitely-not-a-flag"]);
    assert_eq!(o.status.code(), Some(2), "未知选项应退出 2");
    assert!(err(&o).contains("--definitely-not-a-flag"), "错误里要点名：{}", err(&o));

    let o = jscd_env(&["--definitely-not-a-flag"], &[("JSCD_LANG", "en")]);
    assert!(err(&o).contains("unknown option"), "英文错误：{}", err(&o));

    let o = jscd_env(&["--definitely-not-a-flag"], &[("JSCD_LANG", "zh")]);
    assert!(err(&o).contains("未知选项"), "中文错误：{}", err(&o));
}

#[test]
fn output_flag_conflict_exits_2() {
    let o = jscd(&["a.jsc", "-o", "x.js", "y.js"]);
    assert_eq!(o.status.code(), Some(2));
    assert!(err(&o).contains("冲突") || err(&o).contains("conflict"), "{}", err(&o));
}

#[test]
fn missing_input_exits_1() {
    let o = jscd(&["/definitely/not/here.jsc"]);
    assert_eq!(o.status.code(), Some(1), "读不了输入应退出 1");
}

#[test]
fn empty_directory_reports_no_jsc() {
    let dir = temp_dir("empty");
    std::fs::create_dir_all(&dir).unwrap();
    let o = jscd(&[dir.to_str().unwrap()]);
    assert_eq!(o.status.code(), Some(1));
    let e = err(&o);
    assert!(e.contains(".jsc"), "错误里要说明没找到 .jsc：{e}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn directory_with_dash_output_is_rejected() {
    let dir = temp_dir("dash");
    std::fs::create_dir_all(&dir).unwrap();
    let o = jscd(&[dir.to_str().unwrap(), "-"]);
    assert_ne!(o.status.code(), Some(0), "目录 + '-' 应报错");
    let _ = std::fs::remove_dir_all(&dir);
}

// ── 需要 node 编译 fixture 的用例：没有 node 就跳过 ──────────────────────────

fn node() -> Option<String> {
    let o = Command::new("node").arg("--version").output().ok()?;
    if o.status.success() {
        Some("node".to_string())
    } else {
        None
    }
}

fn temp_dir(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!("jscd-cli-{tag}-{}", std::process::id()))
}

fn compile_fixture(node: &str, src_rel: &str, dest: &Path) -> bool {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    if let Some(p) = dest.parent() {
        let _ = std::fs::create_dir_all(p);
    }
    Command::new(node)
        .arg(root.join("scripts/mkcorpus.js"))
        .arg(root.join(src_rel))
        .arg(dest)
        .output()
        .map(|o| o.status.success() && dest.exists())
        .unwrap_or(false)
}

#[test]
fn file_input_defaults_to_stdout_and_writes_files() {
    let Some(node) = node() else {
        eprintln!("跳过：环境里没有 node");
        return;
    };
    let dir = temp_dir("file");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let jsc = dir.join("app.jsc");
    if !compile_fixture(&node, "tests/fixtures/behav/arith.js", &jsc) {
        eprintln!("跳过：fixture 编译失败");
        return;
    }

    // 默认 → stdout
    let o = jscd(&[jsc.to_str().unwrap()]);
    assert!(o.status.success(), "{}", err(&o));
    let s = out(&o);
    assert!(s.contains("function"), "stdout 应是重建的 JS：{}", &s[..s.len().min(200)]);
    assert!(err(&o).is_empty(), "stdout 模式不该有诊断输出：{}", err(&o));

    // 指定文件
    let dest = dir.join("app.js");
    let o = jscd(&[jsc.to_str().unwrap(), dest.to_str().unwrap(), "--quiet"]);
    assert!(o.status.success(), "{}", err(&o));
    assert!(dest.exists(), "应写出 app.js");
    assert_eq!(out(&o), "", "写文件时 stdout 应为空");

    // '-' 显式 stdout
    let o = jscd(&[jsc.to_str().unwrap(), "-"]);
    assert!(o.status.success());
    assert!(out(&o).contains("function"));

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn directory_input_mirrors_tree_into_input_out() {
    let Some(node) = node() else {
        eprintln!("跳过：环境里没有 node");
        return;
    };
    let base = temp_dir("dir");
    let _ = std::fs::remove_dir_all(&base);
    let src = base.join("dist");
    std::fs::create_dir_all(src.join("sub/deep")).unwrap();
    for (rel, fixture) in [
        ("a.jsc", "tests/fixtures/behav/arith.js"),
        ("sub/b.jsc", "tests/fixtures/behav/strings.js"),
        ("sub/deep/c.jsc", "tests/fixtures/behav/closure.js"),
    ] {
        if !compile_fixture(&node, fixture, &src.join(rel)) {
            eprintln!("跳过：fixture 编译失败");
            return;
        }
    }
    // 无关文件不应被处理
    std::fs::write(src.join("note.txt"), "x").unwrap();

    let o = jscd(&[src.to_str().unwrap(), "--quiet"]);
    assert!(o.status.success(), "{}", err(&o));
    let out_root = base.join("dist-out");
    for rel in ["a.js", "sub/b.js", "sub/deep/c.js"] {
        assert!(out_root.join(rel).exists(), "应有 {rel}");
    }
    assert!(!out_root.join("note.txt").exists(), "非 .jsc 不该被处理");

    // 指定输出目录
    let mine = base.join("mine");
    let o = jscd(&[
        src.to_str().unwrap(),
        mine.to_str().unwrap(),
        "--quiet",
    ]);
    assert!(o.status.success(), "{}", err(&o));
    assert!(mine.join("sub/deep/c.js").exists(), "指定目录也要保层级");

    let _ = std::fs::remove_dir_all(&base);
}
/// bytenode 的 `.jsc` 是 CommonJS 源码被 `Module.wrap` 包一层后编译的：
/// 顶层是"闭包工厂"，运行时真正调用的是包装函数 `(exports, require, module,
/// __filename, __dirname)`。摊平它，产物才是"加载即执行"的模块。
///
/// 这里刻意用**顶层赋值给 var**（而不是 `console.log`）：属性的名字在 13.x 上可能
/// 落在只读堆里、要 `--ro-map` 才解得出，而这条用例要验证的是"顶层代码有没有被摊平"。
#[test]
fn cjs_wrapped_jsc_runs_top_level_code() {
    let Some(node) = node() else {
        eprintln!("跳过：环境里没有 node");
        return;
    };
    let dir = temp_dir("wrapper");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let src = dir.join("hello.js");
    // 显式写到一个自定义全局名上：自定义属性名在常量池里（不像 `log` 这类内建名
    // 在 13.x 会落到只读堆、需要 --ro-map），所以这条用例与版本无关。
    std::fs::write(
        &src,
        "function greet(name) { return \"hello \" + name; }\n\
         globalThis.__jscd_hello = greet(\"world\");\n",
    )
    .unwrap();

    // --module = Module.wrap 包一层（与 bytenode --compile 同形）
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let jsc = dir.join("hello.jsc");
    let ok = Command::new(&node)
        .arg(root.join("scripts/mkcorpus.js"))
        .arg(&src)
        .arg(&jsc)
        .arg("--module")
        .output()
        .map(|o| o.status.success() && jsc.exists())
        .unwrap_or(false);
    if !ok {
        eprintln!("跳过：fixture 编译失败");
        return;
    }

    let dest = dir.join("hello.out.js");
    // 产物要拿去跑 → 带上可运行前导（默认输出只给原始代码）
    let o = jscd(&[jsc.to_str().unwrap(), dest.to_str().unwrap(), "--quiet", "--runtime"]);
    assert!(o.status.success(), "{}", err(&o));
    let js = std::fs::read_to_string(&dest).unwrap();
    assert!(
        js.contains("// ── 模块/脚本顶层代码"),
        "顶层（包装函数体）应被摊平到文件级，而不是包成没人调用的函数"
    );

    // 把产物在 vm 里跑起来，读它留下的顶层变量
    let script = format!(
        "const fs=require('fs'),vm=require('vm');\n\
         const ctx=vm.createContext({{}});\n\
         vm.runInContext(fs.readFileSync({0:?},'utf8'),ctx,{{timeout:5000}});\n\
         process.stdout.write(String(ctx.__jscd_hello));\n",
        dest.to_str().unwrap()
    );
    let run = Command::new(&node).arg("-e").arg(&script).output().expect("run product");
    assert!(run.status.success(), "产物应能跑：{}", String::from_utf8_lossy(&run.stderr));
    assert_eq!(
        String::from_utf8_lossy(&run.stdout),
        "hello world",
        "顶层语句要真的执行"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// Node 26 的二进制：`mise exec node@26.10.0`（V8 14.6）。没有就跳过（CI 里没有）。
fn node26() -> Option<String> {
    let o = Command::new("mise")
        .args(["exec", "node@26.10.0", "--", "which", "node"])
        .output()
        .ok()?;
    if !o.status.success() {
        return None;
    }
    let p = String::from_utf8_lossy(&o.stdout).trim().to_string();
    (!p.is_empty() && Path::new(&p).exists()).then_some(p)
}

#[test]
fn node26_product_is_recognised_and_decompiled() {
    // Node 26（V8 14.6）现在有表：认得出（exact）且能反编译出顶层函数。
    let Some(node26) = node26() else {
        eprintln!("跳过：环境里没有 node@26.10.0");
        return;
    };
    let dir = temp_dir("node26");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let src = dir.join("greet.js");
    std::fs::write(&src, "function greet(n){ return 'hello ' + n; }\ngreet('world');\n").unwrap();
    let jsc = dir.join("greet.jsc");
    if !compile_fixture(&node26, src.to_str().unwrap(), &jsc) {
        eprintln!("跳过：fixture 编译失败");
        return;
    }
    let i = jscd(&["info", jsc.to_str().unwrap()]);
    let s = out(&i);
    assert!(s.contains("14.6.202.34"), "{s}");
    assert!(s.contains("supported:     yes"), "{s}");
    let _ = std::fs::remove_dir_all(&dir);
}


#[test]
fn runtime_prelude_is_stripped_by_default_and_opt_in_via_runtime_flag() {
    let Some(node) = node() else {
        eprintln!("跳过：环境里没有 node");
        return;
    };
    let dir = temp_dir("runtime-flag");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let src = dir.join("greet.js");
    std::fs::write(&src, "function greet(n){ return 'hello ' + n; }\ngreet('world');\n").unwrap();
    let jsc = dir.join("greet.jsc");
    if !compile_fixture(&node, src.to_str().unwrap(), &jsc) {
        eprintln!("跳过：fixture 编译失败");
        return;
    }

    // 默认：只有原始代码，没有前导**定义**（正文里的 `__runtime.X(...)` 调用点是忠实于
    // 字节码的，不算前导）
    let plain = out(&jscd(&[jsc.to_str().unwrap()]));
    assert!(
        !plain.contains("var __runtime = new Proxy"),
        "默认不该带运行时前导：{plain}"
    );
    assert!(!plain.contains("var __intrinsic = new Proxy"), "{plain}");
    assert!(!plain.contains("// DefineClass(boilerplate"), "{plain}");
    assert!(plain.contains("function greet"), "原始代码要在：{plain}");
    assert!(plain.len() < 1200, "默认输出应只剩下原始代码：{} 字节", plain.len());

    // `--runtime`：补上可运行前导
    let full = out(&jscd(&[jsc.to_str().unwrap(), "--runtime"]));
    assert!(full.contains("var __runtime = new Proxy"), "--runtime 要带前导");
    assert!(full.contains("var __intrinsic = new Proxy"));
    assert!(full.len() > plain.len() + 3000, "--runtime 明显更长");

    let _ = std::fs::remove_dir_all(&dir);
}
