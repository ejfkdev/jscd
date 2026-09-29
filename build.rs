//! 构建脚本：从 git tag 注入版本号（对齐 dae 的做法）。
//! 无 git 环境或无 tag 时回退 "dev"，保证 cargo build 永不因版本注入失败。

fn main() {
    let version = std::process::Command::new("git")
        .args(["describe", "--tags", "--exact-match"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "dev".to_string());
    println!("cargo:rustc-env=JSCD_VERSION={version}");
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=build.rs");
}
