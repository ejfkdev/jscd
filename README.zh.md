# jscd

把 [bytenode](https://github.com/bytenode/bytenode) 编译出的 `.jsc`（V8 code cache）逆向回 JavaScript 的命令行工具。

`jscd` 纯静态解析 V8 序列化代码缓存——不依赖魔改 V8 二进制、不需要 Node 运行时，单文件可执行。

> **物理上限**：`.jsc` 里从来不存在源码原文（V8 序列化时移除源码，头部的 `source hash` 字段只是原源码的**长度**）。`jscd` 还原的是文件里真实存在的一切：元数据、字符串常量、函数树、字节码反汇编，以及重建出的**语法合法、尽量可运行**的伪 JS。只以作用域槽位形式存活的变量名会以槽名兜底呈现。

## 安装

```sh
cargo install jscd   # 或源码构建：cargo build --release
```

## 用法

```sh
jscd info app.jsc         # 头部字段 + 自动识别 Node/V8 版本
jscd strings app.jsc      # 字符串常量 / 符号表
jscd functions app.jsc    # 函数（SharedFunctionInfo）树
jscd disasm app.jsc       # View8 兼容格式的字节码反汇编
jscd decompile app.jsc    # 重建 JavaScript（快捷形式：jscd app.jsc）
jscd --json info app.jsc  # JSON 输出（脚本消费）
```

## 支持版本

Node 8（V8 5.8）至当前版本；按 V8 版本（而非 Node 大版本）键控。
版本表由 `scripts/codegen.py` 从 Node 源码树生成并编译期内嵌。

## 工作原理

1. **header** — 解析 28 字节 `SerializedCodeData` 头（魔数、版本哈希、源码哈希、flag 哈希、只读快照校验和、payload 长度、校验和）；自动识别并解压 Brotli 包裹（`bytenode --compress`）的文件。
2. **identify** — 内嵌 90 项索引把 `version hash` 映射到 V8 版本；未知哈希用还原的 `Version::Hash()` 算法爆破。
3. **deserialize** — 解析 V8 序列化 payload：SharedFunctionInfo 树、常量池、字节码数组、源码位置表、异常表。
4. **disassemble** — 表驱动 opcode 解码（每版本的操作数宽度/缩放从 `bytecodes.h` 提取）。
5. **decompile** — 寄存器 IR → 控制流重建 → AST → 打印；输出保证语法合法。

## License

MIT
