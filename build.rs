//! 构建脚本：从 git tag 注入版本号（对齐 dae 的做法）。
//! 无 git 环境或无 tag 时回退 "dev"，保证 cargo build 永不因版本注入失败。

fn main() {
    // 打 tag 的构建：版本号 = tag 名（release.sh 保证 tag 与 Cargo.toml 一致）。
    // 没有 .git（crates.io 装、解包构建）时回退到 Cargo.toml 的版本 —— 报 "dev" 会误导用户。
    let version = std::process::Command::new("git")
        .args(["describe", "--tags", "--exact-match"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .or_else(|| std::env::var("CARGO_PKG_VERSION").ok())
        .unwrap_or_else(|| "dev".to_string());
    println!("cargo:rustc-env=JSCD_VERSION={version}");
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=build.rs");
}
