//! jscd — 把 bytenode 编译的 .jsc（V8 code cache）静态逆向回 JS。
//!
//! 模块分层：
//! - `tables`  codegen 生成的多版本配置（基线 + 每版本差异，JSON 内嵌）
//! - `header`  28 字节 SerializedCodeData 头 + Brotli 嗅探
//! - `vhash`   Version::Hash() 还原（MurmurHash 变体）与爆破
//! - `serializer` payload 反序列化（SFI 树/常量池/字节码）
//! - `bytecode`/`disasm` 表驱动字节码解码与 View8 兼容输出

pub mod args;
pub mod cli;
pub mod bytecode;
pub mod decompile;
pub mod disasm;
pub mod header;
pub mod serializer;
pub mod tables;
pub mod vhash;
