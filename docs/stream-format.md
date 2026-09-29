# V8 code cache payload 流格式（静态逆向笔记）

来源：V8 源码精读（node 仓库各 tag 的 `deps/v8`），已用真机产物逐字节验证。
版本：笔记以 V8 9.4（Node 16.20.2）为基准，差异随版本标注。

## 1. 总体结构

```
[header 24/28 字节]  layout 见 src/header.rs；12.x+ 多 ro_checksum
[payload]            通用快照序列化流（serializer.cc 的 Sink 字节流）
```

payload 开头不是对象，而是 `CodeSerializer::SerializeSharedFunctionInfo` 的输出：
顶层 SFI 作为 root 被访问，随后 `SerializeDeferredObjects()`，最后 `kSynchronize` + padding。

## 2. 编码原语

### PutInt 变体（snapshot-source-sink.cc）
`value <<= 2`，按移位后大小选 1..4 字节，**首字节低 2 位 = 字节数-1**，其余位
+后续字节组成小端整数。解码：读 1 字节 `b0`，`n = (b0 & 3) + 1`，
`value = LE(b0..b0+n-1) >> 2`。

### 序列化 bytecode（serializer-deserializer.h `SerializationTag::Bytecode`）
V8 9.4 数值（从枚举直接取；各版本可能增删，由 codegen 提取）：

| 值 | 名称 | 后续 |
|---|---|---|
| 0x00..0x03 | kNewObject(+space) | PutInt(size_in_words)，[map 对象]，body |
| 0x04 | kBackref | PutInt(back_ref_index)（1-based） |
| 0x08 | kReadOnlyHeapRef | PutInt(chunk_index) PutInt(chunk_offset) |
| 0x09 | kStartupObjectCache | PutInt(idx) |
| 0x0A | kRootArray | PutInt(root_index) |
| 0x0B | kAttachedReference | PutInt(idx)（**0 = 源码字符串占位**） |
| 0x0C | kReadOnlyObjectCache | PutInt(idx) |
| 0x0D | kNop | 无（padding） |
| 0x0E | kSynchronize | 无 |
| 0x0F | kVariableRepeat | PutInt(count - 18)，count ≥ 18 |
| 0x12 | kVariableRawData | PutInt(tagged 槽数 n)，n×kTaggedSize 字节 |
| 0x18 | kClearedWeakReference | 无 |
| 0x19 | kWeakPrefix | 后随一个强引用编码 |
| 0x1B | kRegisterPendingForwardRef | PutInt(id) |
| 0x1C | kResolvePendingForwardRef | PutInt(id) |
| 0x1D | kNewMetaMap | 无 size/map，直接 body |
| 0x40..0x5F | RootArrayConstant(0..31) | 无（roots[0..31] 快捷编码） |
| 0x60..0x7F | FixedRawData(1..32 槽) | n×kTaggedSize 字节 |
| 0x80..0x8F | FixedRepeat(2..17) | 无（重复前一个对象引用） |
| 0x90..0x97 | HotObject(0..7) | 无 |

- SnapshotSpace（kNewObject 低 2 位）：`kReadOnlyHeap=0, kOld=1, kCode=2, kMap=3`（references.h）
- kTaggedSize：V8 8.0+（Node 14+）指针压缩 = 4；更早 = 8。表按版本携带。
- Smi 不单独编码：VisitPointers 跳过 Smi 槽，连同 raw 空隙由
  `OutputRawData` 以 FixedRawData/VariableRawData 输出（**Smi 值藏在 raw chunk 里**，
  按字段偏移解读）。

### 引用优先级（Serializer::SerializeObjectImpl）
1. ThinString → 直接序列化 actual
2. SerializeHotObject（最近 8 个对象环形）
3. SerializeRoot（roots 表 → kRootArray / RootArrayConstant）
4. SerializeBackReference（已序列化对象 → kBackref + index；或 attached ref）
5. SerializeReadOnlyObject（RO 堆 → kReadOnlyHeapRef + page/offset）
6. 否则 kNewObject 递归

热对象：每次被引用即加入环形（kHotObjectCount=8）。注意 backref 也会 `hot_objects_.Add`。

## 3. 对象体布局

`kNewObject` body = map 对象（递归）+ body descriptor 决定的 tagged 槽序列
+ 尾部 raw 数据（OutputRawData 吞掉 Smi 槽与字节域）。
静态解析必须按实例类型知道「哪些偏移是指针槽」——这正是版本表 `layouts` 段
要携带的信息（codegen 从 objects-body-descriptors-inl.h + 字段偏移头提取；
M1 先对锚点版本手工编码，语料回归验证）。

- Map 的 instance_type 在 map 对象自身的 raw 前缀里（kInstanceTypeOffset）。
- RO 堆里的 map 通常以 kReadOnlyHeapRef / RootArrayConstant 出现：
  root 索引可从 roots.h 解析；非 root 的 RO 对象按 (chunk_index, chunk_offset)
  校准（语料标定，风险随 V8 patch 版本浮动）。
- BytecodeArray：age 字节在 raw 输出中被固定为 kNoAgeBytecodeAge。

## 4. code cache 顶层流程（deserializer 视角）

1. 读一个对象 = 顶层 SFI。
2. 之后跟着 deferred 段（延迟对象依次 kNewObject…）。
3. `kSynchronize` 收尾；`kNop` 对齐填充。
4. 注意：SFI 的 constant_pool 里嵌套 SFI（子函数）会在此段出现——函数树即由此重建。

## 5. 与 bytenode 的关系

bytenode 不改 payload，只动头部（fixBytecode 回填 flag_hash/ro_checksum）与
源码占位（kAttachedReference 0 + source_hash 长度）。所以本工具解析的 payload
与 `node --print-bytecode` 看到的字节码一致，可互为黄金参照。
