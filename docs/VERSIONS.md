# 版本矩阵：V8 code cache 的结构差异

每一条都来自 node 源码 + 真机产物对拍校准（`scripts/codegen.py` 已把这些差异固化进 `tables/*.json`）。
表中版本为锚点：9.4(Node16) / 10.2(Node18) / 11.3(Node20) / 12.4(Node22) / 13.6(Node24)。

## 1. 头部（SerializedCodeData）

| 版本 | 头长 | 字段 |
|---|---|---|
| 9.4 | 24 | magic/ver/src/flags/ **payload_len@16** / checksum@20 |
| 10.2 | 24 | 同上 |
| 11.3 | 24 | 同上 |
| 12.4+ | 32 | 多 **ro_checksum@16**，payload_len@20、checksum@24，`POINTER_SIZE_ALIGN(28)=32` |

bytenode 的 `fixBytecode` 注释里的"ro checksum@16"只对 12.x+ 成立——所以它按版本分支回填。

## 2. `Version::Hash()` 折叠方向

| 版本 | 算法 |
|---|---|
| ≤ 11.x | 变参递归：`hash_combine(hash_combine(hash_combine(hash_combine(0,p),b),n),m)`（patch 先） |
| ≥ 12.x | `base::Hasher` 左折叠：seed=0 依次 Add(major,minor,build,patch)（major 先） |

真机校验向量（`src/vhash.rs` 单测）：
9.4.146.26→0xacdd64ee、10.2.154.26→0x3569a082、11.3.244.8→0x00e4c20b、12.4.254.21→0x79dafe74、13.6.233.17→0xdc338cfa。

## 3. 序列化 tag 枚举（snapshot/serializer-deserializer.h）

- 9.4：`kNewObject=0, kBackref=4`（4 个空间），`kNop=10`
- 10.2：新增 `kSharedHeapObjectCache`、`kOffHeapResizableBackingStore`，整体后移 1
- 11.3：**空间数变 3**（`kBackref=3`），新增 3 个 sandbox/raw external ref 变体
- 12.4：**删 `kReadOnlyObjectCache`**，新增 `kNewContextlessMetaMap/kNewContextfulMetaMap/kIndirectPointerPrefix/kProtectedPointerPrefix/kInitializeSelfIndirectPointer`；`enum Bytecode : uint8_t`（此前 `: byte`）
- 13.6：**repeat 拆成 \*Root 变体**（`kVariableRepeatRoot`/`kFixedRepeatRoot`，编码为 `[count][1 字节 root 索引]`）；新增 `kAllocateJSDispatchEntry/kJSDispatchEntry/kApiWrapperFieldsData`

解析器按表取值并用 `tag_any` 兼容别名（见 `src/serializer.rs`）。

## 4. BytecodeArray 布局（objects/code.tq → objects/bytecode-array.tq）

| 版本 | header | 关键差异 |
|---|---|---|
| 9.4 | 54 | `osr_loop_nesting_level(int8)@52` + `bytecode_age(int8)@53` |
| 10.2 | 56 | `osr_urgency_and_install_target(uint16)@52` + `bytecode_age(uint16)@54` |
| 11.3 | 54 | 去掉 osr，`bytecode_age(uint16)@52` |
| 12.4 | 64 | **改为 extends ExposedTrustedObject**：多了 `wrapper`(BytecodeWrapper)@16，三个表变 `ProtectedPointer` 且**顺序变为 spt@24/ht@32/cp@40**，尾部 `optional_padding` |
| 13.6 | 64 | 同上，且 `parameter_size` 变为 **uint16 计数**（不再除 8） |

- 头偏移由 `codegen.py` 的 torque 布局推导器算出（含继承链、`@if` 条件、bitfield struct 尺寸、Struct 内联展开、`paddingN` 排除）。
- `parameter_count()` 语义：≤12 是"参数栈字节数"（>>3），13.x 起直接是计数（`src/disasm.rs` 按表分支）。

## 5. 解释器帧常量（execution/frame-constants.h）

`kRegisterFileStartOffset = -(StandardFrameConstants::kFixedFrameSizeFromFp/S + extra) - 1`

| 版本 | extra | reg_file_start | context | closure | first_param |
|---|---|---|---|---|---|
| 9.4–11.3 | 2 | -6 | -5 | -4 | -8 |
| 12.4+ | 3 | -7 | -6 | -5 | -9 |

（12.x 的 UnoptimizedFrameConstants 多了一个 feedback vector 槽）

## 6. SharedFunctionInfo（objects/shared-function-info.tq）

| 版本 | function_data | name_or_scope_info | script |
|---|---|---|---|
| 9.4–12.4 | @8 | @16 | @32（12.4 改名 `script`） |
| 13.6 | trusted@8 + untrusted@16 | **@24** | **@40** |

解析时对 `function_data` 的多个候选槽逐个尝试（取指向 BytecodeArray 的那个）。

## 7. ScopeInfo（objects/scope-info.tq）

| 版本 | flags | 变量区顺序 |
|---|---|---|
| ≤ 12.x | `SmiTagged<ScopeFlags>`（值在高 32 位） | names[n] → infos[n] → [saved] → [receiver] → function_variable_info |
| 13.6 | **裸 uint32** | position_info(2) → [module_count] → names[n]（n<75 内联，否则 hashtable）→ infos[n] → [saved] → function_variable_info |

flags 位域位置两代一致（saved=bit10、function_variable=bit12-13、inferred=bit14、receiver=bit7-8）。

## 8. roots 表

- 顺序 = `READ_ONLY_ROOT_LIST + MUTABLE_ROOT_LIST`，含 INTL internalized strings。
- `TORQUE_DEFINED_MAP_ROOT_LIST` 是构建期 torque 生成、源码里看不到 → 用"探针 jsc 的真实根索引"按版本解出**条数**：
  9.4/10.2/12.4/13.6 = 36，**11.3 = 34**。占位补齐后保留尾部列表（ALLOCATION_SITE / NAME_FOR_PROTECTOR / DATA_HANDLER），
  因为 `length`/`join` 这类保护器字符串根就在尾部。
  锚点（探针里受保护字符串被引用的根索引，与表逐一对齐）：11.3 `constructor=724 next=725 resolve=726 then=727`。
  条数算多两格的后果：`.next` 解析成 `AllocationSite`、for-of/生成器整类失效。
- 校准方法固化为 `scripts/build_ro_list.py`（候选串清单）+ `scripts/build_ro_map.sh`（探针 jsc → `(chunk/offset) → 名字`）。

## 8.1 只读堆引用与 ro-map

13.x 起函数名/属性名常以 `kReadOnlyHeapRef`（`(chunk, offset)` 地址）出现，`.jsc` 里没有名字。
解法的探针形态很关键：**只有 `o["<字面量>"]` 这种计算属性**才会把字面量编码成只读堆引用
（当 join 参数就不产生）→ 候选清单必须含非标识符串（标点、数字串等），并并入 fixtures 语料
（否则 `join("/")` 会留下 `<ro0_18016>` 占位）。
`--ro-map` 同时接给 `Disassembler`（`sfi_name`/`name_from_ref` 解 RoRef），否则 node24 的生成器函数名是空的、
调用点又用全局名 → 找不到函数。

## 9. 未完成/待办

- 行为矩阵（20 fixture × node 16/18/20/22/24）已**全部通过**；真实语料 408×3 份 0 语法错误。
- 老版本家族（V8 5.8–8.4，Node 8–14）仍未通过解析：7.8/8.4 主 payload 通了，卡 deferred 段条目形状
  与 space2 分配序列（1008 与 1136 之间缺一个 128 字节对象 → backref 命不中）；6.2/6.8 还需位打包 tag 解码。
- 观感：phi/临时寄存器成对噪声、迭代器关闭协议逐字发射（可读性，非正确性）。

### 9.1 逐版本形态差异（本轮补齐，改 parser/decompiler 前对照）

| 项目 | ≤11.x | 12.x | 13.x |
| --- | --- | --- | --- |
| ObjectBoilerplateDescription 头 | `[flags, key, val…]`（元素起 1） | 多 `BackingStoreSize/Flags` → 元素起 **2**，条目数 `Capacity/2` | 同 12.x |
| ClassBoilerplate | 7 格 FixedArray（含 args_count） | **ClassBoilerplateMap，6 格**（实例模板在元素 3） | 同 12.4 |
| 类属性模板 | DescriptorArray，条目 **`(key, details, value)`**；头一格是**打包计数**（非 Smi） | 同 | 类型名 `ClassBoilerplateMap` |
| for-in 判定 | `ForInContinue` + `JumpIfFalse` | 同 | **`JumpIfForInDone idx, len`**（判定在 ForInNext 之前） |
| for-in 步进 | `ForInStep <Reg>` | 同 | `ForInStep <RegInOut>`（新操作数类型） |
| handler 表 shift | 3 | 3 | 4（且 handler 落在区间终点之后几条指令处） |
| handler 表长度 | 高 32 位 | 高 32 位 | **低 32 位** |
| SFI 名字来源 | 池字符串/根 | 同 | 常为只读堆引用（需 ro-map） |

## 10. 旧族（V8 ≤ 8.x，Node 8–14）

Node 12/14 的 `version_hash` 已能精确/爆破识别，表也已提取（`tables/v7_8.json`、`v8_4.json`），
解码走独立路径（`LegacyAlloc` + `legacy` tag 表）。行为矩阵：node12 19/20 + 1（源码用了
7.8 不支持的 `?.`）、node14 20/20；真实语料 440 份 0 语法错误、0 解析失败。已查明的差异：

### 10.1 tag 编码：与空间位打包

| 版本 | 编码 | 关键值 |
|---|---|---|
| 6.2 / 6.8 | `static const int` + 位掩码 | `kNewObject=0x00`、`kBackref=0x08`、`kNop=0x2f`、`kSynchronize=0x1c`、`kVariableRepeat=0x1d`、`kVariableRawData=0x3a`、`kRootArrayConstants=0x80`、`kFixedRawData=0xc0`、`kFixedRepeat=0xe0`、`kHotObject=0xf0` |
| 7.8 / 8.4 | 现代式扁平枚举，但语义不同 | `kBackref=0x08`（**8 个空间** 0x00..0x07）、无 `kReadOnlyHeapRef`、`kNop=20`、`kSynchronize=26`；多出 chunk 类 tag：`kNextChunk=21`、`kDeferred=22`、`kAlignmentPrefix=23`、`kVariableRawCode=30`；8.4 有 `kStartupObjectCache=16`、7.8 该位是 `kPartialSnapshotCache=16` |
| 9.4+ | 现代扁平枚举 | 见正文各节 |

### 10.2 skip 变体（旧族专有）

旧族把"引用 + 跳过"合并编码：

```
kRootArrayConstants = 0x80, kRootArrayConstantsWithSkip = 0xa0, kRootArrayConstantsMask = 0x1f
kHotObject         = 0xf0, kHotObjectWithSkip         = 0xf8, kHotObjectMask         = 0x07
kFixedRawData = 0xc0, kFixedRepeat = 0xe0
kWhereMask = 0x1f, kHowToCodeMask = 0x20, kWhereToPointMask = 0x40
kSkip = 0x0f, kDeferred = 0x6f, kNextChunk = 0x4f, kAlignmentPrefix = 0x19
kNumberOfFixedRawData = 0x20, kNumberOfFixedRepeat = 0x10, kNumberOfHotObjects = 8
```

即 `0xa0..0xbf` = 根常量 0..31 **且跳过 1 槽**；`0xf8..0xff` = 热对象 + 跳过；
`kDeferred`(0x6f) 表示该引用被延迟（随后由 deferred 段补全）。

### 10.3 还需校准

- BytecodeArray 头偏移（6.x/7.x 的 `bytecode-array.h` 是 C++ 定义，非 .tq）
- 解释器帧常量（6.x/7.x 的 `frame-constants.h` 结构不同）
- SMI/指针宽度：Node 12 及更早在 x64 上无指针压缩（ts=8）；Node 8/10 同样

### 10.5 语义差异（本轮逐条对拍 V8 源码，都是"读错就整段错位/丢名字"的坑）

| 项目 | 7.8 / 8.4 实情 | 我们踩过的坑 |
| --- | --- | --- |
| backref 的 MAP/LO 空间 | `PutBackReference` 对 `kMap`/`kLargeObject` 只写**一个序数**（map_index / large_object_index），其余空间才写 (chunk_index, chunk_offset) | 一律读两段 varint → 第一个 MAP/LO 回引之后整条流错位（大 payload 上表现为若干 KB 后撞非法 tag 0x98 / 0xbc） |
| hot 环写入点 | 只有 `PutRoot`（根数组分支）与 `PutBackReference` 会 `hot_objects_.Add`；**新对象不入环** | 解析 kNewObject 时多记一笔 → 之后每个 hot 索引错位（ScopeInfo 被读成别的 SFI，函数名全丢） |
| 对象 size 单位 | `PutInt(size >> kObjectAlignmentBits)`，8.4 的 `kObjectAlignmentBits == kTaggedSizeLog2`（≠8） | 按 8 字节算 → 每个对象地址翻倍、backref 全不命中 |
| `ScopeInfo::HasFunctionName()` | 8.4→13.x 全是 `NONE != FunctionVariableBits`：UNUSED(3) 只是"函数变量没用到"，槽位保留、值为 `kNoSharedNameSentinel`（Smi → 序列化成 raw） | 按"必须 STACK/CONTEXT"过滤 → 真名一起丢（closure 的 `target`） |
| 数值域存储 | 7.8 及更早 **Smi**，8.x 裸 int32，9.x–12.x Smi，13.x 裸 int32 | 一律按老族=裸 int32 → 7.8 的 flags/param/clc 全读错 |
| `Context::MIN_CONTEXT_SLOTS` | ≤7.x = **4**（`[scope_info, previous, function, extension]`），8.x 起 = 2 | 写死 2 → 7.8 的 `StaCurrentContextSlot [4]`（局部 0）被写成 `__ctx.ctx4`，与闭包里的 `n` 对不上 |
| `GetIterator` | ≤7.x 的 `GetIteratorWithFeedback` 只 `LoadIC(receiver, @@iterator)`（**不调用**），调用是紧随其后的 `CallProperty0 <方法>, <receiver>`；8.x 起 builtin 连调用一起做 | 一律按"取方法并调用" → 7.8 把迭代器再当方法调一次（`rN.call is not a function`）。判据：GetIterator 的操作数 2 个=只取方法、3 个=会调用 |
| acc 隐式使用表 | ≤9.x 写 `AccumulatorUse::kRead/kWrite/kReadWrite`（短名），10.x+ 写 `ImplicitRegisterUse::kReadAccumulator…` | codegen 只认长名 → 老族 acc 表全空 → `flush_acc_before` 把 keyed 访问的键当死值丢掉（`a[0]` → `a[undefined]`） |
| TDZ 检查 | `LdaContextSlot …; ThrowReferenceErrorIfHole` 紧跟 context 读取 | context 变量已摊平成文件级 `var`（初值 undefined）→ 这条检查必然误报，需跳过 |

### 10.4 运行旧版 Node（本机实操）

```bash
# mise 的镜像很慢（~20 KB/s）；直连 nodejs.org 快 20 倍，且 ≤15 只有 x64 包（需 Rosetta）
curl -sL -o /tmp/node-v14.21.3-darwin-x64.tar.gz \
  https://nodejs.org/dist/v14.21.3/node-v14.21.3-darwin-x64.tar.gz
d=~/.local/share/mise/installs/node/14.21.3 && mkdir -p $d && \
  tar -xzf /tmp/node-v14.21.3-darwin-x64.tar.gz -C $d --strip-components=1
$d/bin/node --version   # 经 Rosetta 运行
```
