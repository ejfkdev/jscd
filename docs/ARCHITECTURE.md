# jscd 架构

## 目标

把 bytenode 编译的 `.jsc`（V8 code cache）**纯静态**逆向回 JS 语义，单二进制、无 Node 依赖、
支持 Node 8（V8 5.8）起的全部官方版本。产物要求：反汇编忠实、伪 JS 语法合法且尽量可运行。

## 层次

```
      ┌──────────── CLI (args.rs / cli.rs) ────────────┐
      │  info · strings · functions · disasm ·         │
      │  decompile · version        --json / -o        │
      └───────────────────────┬────────────────────────┘
                              │
   header.rs      28/24 字节 SerializedCodeData 头 + Brotli 嗅探
   vhash.rs       Version::Hash() 双折叠算法（≤11 右折叠 / ≥12 左折叠）+ 爆破
   tables.rs      codegen 生成的版本表（JSON 内嵌，按 V8 版本键控）
   serializer.rs  通用快照流 → 对象树（tag/pending/hot 环语义对齐 serializer.cc）
   bytecode.rs    表驱动字节码解码（操作数宽度/缩放/寄存器命名）
   disasm.rs      反汇编文本（对齐 node --print-bytecode，可逐字节对拍）
```

## 版本矩阵

- 解析表按 **V8 major.minor**（36 个）生成；`version_hash` 索引按 **完整 V8 构建**（90 项）生成。
- 表内容：opcode 顺序表、操作数类型宽度/缩放、SerializedCodeData 头布局、序列化 tag 枚举、
  roots 顺序（截断到 torque 段前，之后走结构指纹）、runtime/intrinsic 名表、tagged 指针宽度。
- 表由 `scripts/codegen.py` 从 node 源码树按 tag 提取（`git show <tag>:deps/v8/...`）。

## 关键设计约束

1. **不重建堆**：流在槽粒度自描述（raw chunk 与引用编码首字节空间不相交），对象体线性消费即可。
2. **家族常量**（帧布局/寄存器命名/BCA 头偏移）由真机 golden 校准，见 `docs/stream-format.md`。
3. **tagged_size 是构建属性**：压缩（linux/win x64）与非压缩（macOS/arm64）不同，解析失败时自动回退另一种。
4. **反汇编必须可对拍**：输出格式 = `node --print-bytecode`，`scripts/golden_diff.py` 做逐指令 diff。
