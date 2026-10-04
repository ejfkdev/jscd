#!/usr/bin/env bash
# 打 tag 发版：预检 → 打「带说明」的 annotated tag → 推送。
#
#   scripts/release.sh v0.1.0                # tag 说明用 .github/release-body.md 模板
#   scripts/release.sh v0.1.0 NOTES.md       # 或者换成自己写的说明文件
#
# 推送 tag 即触发 .github/workflows/release.yml：先跑 checks（cargo test + clippy
# -D warnings），再构建六个平台的裸二进制挂到 GitHub Release。
#
# tag 说明不是摆设：发版 workflow 会优先把 **annotated tag 的说明**当成
# Release 描述（见 workflow 里的 "Compose release body" 步骤），
# Release 正文 = tag 说明 + GitHub 自动生成的 release notes。
# 所以用 `git tag -a`（本脚本），不要用轻量 tag。
set -euo pipefail

ver="${1:-}"
notes_file="${2:-.github/release-body.md}"

if [[ -z "$ver" ]]; then
  echo "用法: scripts/release.sh vX.Y.Z [说明文件]" >&2
  exit 2
fi
if [[ "$ver" != v* ]]; then
  echo "版本号要以 v 开头（如 v0.1.0）—— build.rs 与 CI 都直接拿 tag 名当版本号" >&2
  exit 2
fi

cd "$(dirname "$0")/.."

if ! git remote get-url origin >/dev/null 2>&1; then
  echo "还没有配置 origin，先加一下（换成你自己的仓库地址）：" >&2
  echo "  git remote add origin git@github.com:ejfkdev/jscd.git" >&2
  exit 2
fi
if [[ -n "$(git status --porcelain)" ]]; then
  echo "工作区不干净，先把改动提交掉再打 tag：" >&2
  git status --short >&2
  exit 2
fi
# tag 名会由 build.rs 写进 `jscd --version`，也必须与 Cargo.toml 的版本一致，
# 否则 cargo/crates.io 元数据与二进制报的版本对不上。
pkg_ver="$(sed -n 's/^version *= *"\([^"]*\)".*/\1/p' Cargo.toml | head -1)"
if [ "$ver" != "v$pkg_ver" ]; then
  echo "tag 与 Cargo.toml 的版本对不上：tag=${ver}，Cargo.toml=${pkg_ver}" >&2
  echo "先把 Cargo.toml 改成 version = \"${ver#v}\"，跑一遍 cargo test 让 Cargo.lock 跟上，提交后再发版。" >&2
  exit 2
fi

if git rev-parse -q --verify "refs/tags/$ver" >/dev/null; then
  # 注意 `${ver}` 的花括号：变量后面紧跟全角字符时，bash 会把全角字节当成变量名的一部分。
  echo "tag ${ver} 已经存在（要重打先删：git tag -d ${ver} && git push origin :${ver}）" >&2
  exit 2
fi
if [[ ! -f "$notes_file" ]]; then
  echo "找不到说明文件：$notes_file" >&2
  exit 2
fi

echo "==> 发版门禁：cargo test --release"
cargo test --release
echo "==> 发版门禁：cargo clippy --all-targets -- -D warnings"
cargo clippy --all-targets -- -D warnings

msg="$(sed "s/@VERSION@/$ver/g" "$notes_file")"
printf '%s\n' "$msg" | git tag -a "$ver" -F -

branch="$(git rev-parse --abbrev-ref HEAD)"
echo "==> 推送 ${branch} 分支与 tag ${ver}（tag 一推就开跑 release workflow）"
git push origin "$branch"
git push origin "$ver"
echo "==> 完成：https://github.com/ejfkdev/jscd/actions 看构建，"
echo "          https://github.com/ejfkdev/jscd/releases/tag/${ver} 看产物"