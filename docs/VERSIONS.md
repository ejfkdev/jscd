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

- 顺序 = `READ_ONLY_ROOT_LIST + MUTABLE_ROOT_LIST`，含 INTL internalized strings（122 条，Node 默认 full-icu）。
- `TORQUE_DEFINED_MAP_ROOT_LIST` 是构建期生成、顺序难静态复现 → **在表里截断**，之后的 root 引用走结构指纹分类，避免错分类。

## 9. 未完成/待办

- 13.6 有 1 个函数名取值偏差（roots 索引与真实构建的字符串段仍有个别错位；反汇编本体完全一致）。
- 老版本家族（V8 5.8–8.4，Node 8–14）尚未做代码页校准（表结构已就位）。
## 10. 旧族（V8 ≤ 8.x，Node 8–14）——待实现

Node 12/14 的 `version_hash` 已能精确/爆破识别，表也已提取（`tables/v7_8.json`、`v8_4.json`），
但**解码路径需要独立实现**（现代路径不适用）。已查明的差异：

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

### 10.4 运行旧版 Node（本机实操）

```bash
# mise 的镜像很慢（~20 KB/s）；直连 nodejs.org 快 20 倍，且 ≤15 只有 x64 包（需 Rosetta）
curl -sL -o /tmp/node-v14.21.3-darwin-x64.tar.gz \
  https://nodejs.org/dist/v14.21.3/node-v14.21.3-darwin-x64.tar.gz
d=~/.local/share/mise/installs/node/14.21.3 && mkdir -p $d && \
  tar -xzf /tmp/node-v14.21.3-darwin-x64.tar.gz -C $d --strip-components=1
$d/bin/node --version   # 经 Rosetta 运行
```
