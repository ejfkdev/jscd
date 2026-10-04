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

### 10.6 node8/10（6.2/6.8）现状：行为对拍 48/50 全绿（2 条 skip 是 fixture 语法要 node14+）

| 项目 | 状态 |
| --- | --- |
| 版本识别 | ✅ `version_hash` 精确命中（6.2.414.78 / 6.8.275.32） |
| 标签表 | ✅ `enum Where` 抓取（kNewObject=0x00、kBackref=0x08、kRootArray=**0x05**、kAttachedReference=0x0d、kHotObject=**0x38**、kRootArrayConstants=0x80、skip 变体 0x10/0x58/0xa0）；另补 `kNumberOfSpaces`/`kSpaceTagSize`/`kPageSizeBits`/`kMapSpace`/`kLoSpace` |
| 头/payload 起点 | ✅ 40 字节头 + 预留表 + 桩键，起点 `align8(40 + 4*(num_res+num_stub_keys))` |
| 对象解码 | ✅ 5/6 个空间、`*WithSkip` 变体、backref 位域（空间号在标签里、低 ValueIndex 位是 `chunk_index<<16\|offset>>3`）、LO 可执行字节、repeat="复制前一槽"、6.2 变长 raw **不推进**槽指针（随后的 kSkip 才推进） |
| 表 | ✅ SFI/BytecodeArray/roots/runtime_names/scope_info/字符串布局（length 是 Smi、字符区 24） |
| 行为对拍 | `bash scripts/verify_behavior.sh 8.17.0 10.24.1` → **48 pass / 0 fail / 0 compile-fail / 2 skip**（25 个 fixture；`skip` = `optional_chain` 用了 node8/10 解析器不支持的 `?.`/`??`，cases.json 里标 `min_node: 14` → 低版本**显式跳过**而不是混进 compile-fail）。九版本合计 **222 pass / 0 fail / 0 cfail / 3 skip**；**node8/10 的 async 也已重建（含 async 函数体内的用户 try/catch）** |
| 真实语料 | ✅ node 自带的 net/url/events/fs/stream/inspect/path/errors/… 10 份源码在 8.17.0/10.24.1 上反编译全部成功且 `node --check` 通过 |
| 样本回归 | ✅ 标准语料（`work/samples` 408 份 test262/TheAlgorithms 抽样）：node8 344 跑、node10 374 跑，**syntax-fail=0 / decompile-fail=0 / ref-error(ours)=0**；大语料抽样（`--samples work/samples-all --limit 800`）：node8 736 跑、node10 766 跑，同样全 0（两轮合计 ran=2220，失败只有"该版本解析器不支持该样本语法"的 compile-fail）。直扫两轮全部产物（1080 + 1140 份）：`_ro` 占位 0 处、`node --check` 0 失败 |
| 6.x ro-map | ⚠️ 6.2 的属性名一律内联在 payload（探针解出 **0** 条 RO 引用名，不需要表）；6.8 有 12 条（`0/21808 → "g"` 这类短名/内建名）→ 建表后 node10 的 `_ro` 占位 12 → **0**。清单与建表都可脚本化：`build_ro_list.py --no-punct work/samples work/samples-all tests/fixtures` + `build_ro_map.sh <ver> <list> <out>` |

#### 10.6.0 async/await 重建（九版本全部通过；用户 try/catch 见 §10.6.0.1 / §10.6.0.2）

V8 把 `async function` 编成与生成器**同构**的状态机：`_AsyncFunctionEnter` 建状态对象 →
每个 await 点 `_AsyncFunctionAwait(state, 值)` + `SuspendGenerator` → `ResumeGenerator` +
`GeneratorGetResumeMode` 分派 → `_AsyncFunctionResolve/Reject` 结算 Promise。
重建时不另造驱动器，直接发 `async function` + `await`：

| 环节 | 做法 |
| --- | --- |
| 识别 | `plan_async`：扫 `InvokeIntrinsic/CallJSRuntime/CallRuntime` 的名字 —— `AsyncFunctionEnter`/`AsyncFunctionAwait*`/`AsyncGenerator*`，以及 6.x 唯一可用的 **`ResolvePromise`/`RejectPromise`**（普通生成器不会有结算调用） |
| 机器码折叠 | 状态对象创建、`Mov <closure>/<this>/<context>`、state→实参搬运、await 调用、`SuspendGenerator`/`ResumeGenerator`/恢复分派、`SetPendingMessage` 全标 `__gskip`；**序言里算 await 表达式的指令要留**（不能像普通生成器那样整段跳过） |
| await 点 | 挂起点改名 `__gawait`，值取 await 调用的**第二个**寄存器（12.x+ 的 reglist；6.x 用"挂起点前最近一条调用的第 2 个寄存器"兜底）→ 发 `rK = await <值>;` |
| 结算 | `AsyncFunctionResolve/ResolvePromise` → `return <值>;`、`AsyncFunctionReject/RejectPromise` → `throw <错误>;`（值都取**第 2 个**实参：7.8 还多一个"是否已捕获"的布尔尾巴，取 last 会发 `return false`） |
| 函数头 | `async function` / `async function*`（与生成器正交：有 SuspendGenerator 也不再发 `*`） |
| 恢复分派（13.x） | `LdaZero; TestReferenceEqual rMode; JumpIfTrue <kNext>` 这种比较链形态也要认（以前只认 `SwitchOnSmiNoFeedback`，于是 `GeneratorGetResumeMode` 残留进产物） |

行为验证（`/tmp/as2.js` 探针 + 新 fixture `tests/fixtures/behav/async_await.js`）：
node12.22.12 / 14.21.3 / 16.20.2 / 18.20.8 / 20.20.2 / 22.12.0 / 24.12.0 上
`noAwait(1)=2`、`withAwait(21)=42`、`multi(2,3)=5`、`loopAwait` 累加、拒绝传播都与原始一致，
fixture 的 3 组用例全过。`behav_diff.js` 也升级成"结果是 Promise 就先 await 再比"。

**6.x 的包装器折叠（6.2/6.8 都已通；用户 try/catch 见 §10.6.0.1 / §10.6.0.2）**：6.x 的 async 外面包了一层
`LdaZero; Star rC; Jump <dispatch>`（正常出口）+ catch 处理器（`RejectPromise`）+
`SwitchOnSmiNoFeedback (rC) {0: resolve, 1: …, 2: ReThrow}` 分派，分派块在函数尾部、
且被 catch 区间覆盖。折叠做法（`plan_async` 里的 6.x 分支）：
- 找"case 0 里含 ResolvePromise"的那个 switch = 分派；
- 分派之前最后一个 `CreateCatchContext` 起、到分派为止整段是机器码（reject 路径）→ 标 `__gskip`；
- 那两个 handler（外层=完成码设置、内层=catch）登记进 `used_handler_starts`，否则 ① 规则
  会把分派整块当成 catch 体（正常路径不可达）；
- 主体出口那条"目标正好是分派"的 `Jump` 也标 `__gskip`，让控制流线性落入分派；
- 分派里的 `ResolvePromise` 由结算处理器变成 `return <值>`、`RejectPromise` 变成 `throw <错误>`。

实测：**node8.17.0（6.2）与 node10.24.1（6.8）的 `async_await`、`async_try` fixture 都已通过**
（`noAwait(1)=2`、`withAwait(21)=42`、`multi(2,3)=5`、`loopAwait` 累加、`withTry(rej)=caught:…` 与原文一致）；
node12.22.12–24.12.0 同样通过。九版本合计 204 pass / 0 fail / 3 cfail（3 个 cfail 都是 `arrow_opt` 的语法越界）。

**6.2（node8）的重建**（本轮打通，与 6.8 用同一套完成码折叠）：6.2 的 async 机器码全是
native-context 槽号形式的 `CallJSRuntime`（名字解不出），并且**函数体在"序言"之后、首个挂起之前**
（生成器的首挂起是"未启动"挂起、体在续体里；async 没有那个挂起 —— 体直接跑）。
所以不能像生成器那样把 `0..=首挂起` 整段跳过，序言右端要取 **`_AsyncFunctionEnter` 之后那条 Star**
（noAwait 这种没有 `CreateJSGeneratorObject` 的形态，判据是"尾部 `SwitchOnSmiNoFeedback` 的
case 0 块里有数字名调用"= ResolvePromise）。首挂起之后的恢复垫片
（`RestoreGeneratorRegisters … GetInputOrDebugPos → Star rK … GetResumeMode; LdaZero;
TestEqualStrictNoFeedback rM; JumpIfTrue <续体>`）也要跳过 —— 否则恢复值寄存器被
`__intrinsic.GeneratorGetInputOrDebugPos` 的返回值覆盖（node8 的 `loopAwait` 因此算出 NaN）。

**node8.17.0（6.2）已重建**（见上文的"6.2 的重建"）：识别靠形态（
`_CreateJSGeneratorObject` 后紧跟数字名调用；没有它的形态靠尾部 switch 的 case 0 里有数字名结算），
await 值取挂起点之前最近一条调用寄存器列表的第 2 个，序言右端取 Enter 之后那条 Star，
首挂起之后的恢复垫片整段跳过；结算/完成码（rC/rV）与 6.8 同一套就地折叠。

`behav_diff.js` 顺带加固：反编译产物的 Promise 可能永不 settle → 现在用 `Promise.race` 加
2 秒上限（超时记 `ERR Timeout`），对拍不会再被挂住。

另有一个与 async 相关的小缺口（各版本都在）：**async 函数体内的用户 `try/catch`** 与
V8 的 reject 包装器交织时只还原成"抛出"（`withTry(rej)` 应返回 `caught:…`）——fixture 没覆盖这一形态。
**12.x+ 见 §10.6.0.1、6.8 见 §10.6.0.2（都已修好）。**

#### 10.6.0.1 async 函数体内的用户 try/catch（12.x+ 已通）

探针 `/tmp/as2.js` 的 `withTry(p)`（`try { const v = await p; return v } catch(e) { return "caught:" + e.message }`）
在 12–24 上原来被摊成直线代码：try 体、用户 catch 体、机器 reject 块顺序排列，
所以 `withTry(Promise.reject(...))` 返回 `undefined`。V8 11.x 的字节码（node20，123 字节）长这样：

```
@0  SwitchOnGeneratorState r0, [0], [1] { 0: @36 }   ← 生成器状态机外壳（机器码）
@10 _AsyncFunctionEnter → @14 Star0
@18 Mov <context>, r3        ← handler A 起点（被折叠成 __gskip）
@21 Mov r0, r4               ← handler B 起点（state 搬运，也是 __gskip）
@24 Mov a0, r5
@27 _AsyncFunctionAwaitCaught r4-r5 ; @31 SuspendGenerator
@36 ResumeGenerator … Ldar r4; ReThrow          ← 恢复垫片（await 被拒 → ReThrow）
@54 Mov r4, r1 ; … ; @63 _AsyncFunctionResolve ; @67 Return
@68 Star4; CreateCatchContext r4,[1]; …; @96 _AsyncFunctionResolve ; @100 Return   ← 用户 catch（值 = "caught:"+e.message）
@101 Star3; CreateCatchContext r3,[4]; …; @118 _AsyncFunctionReject ; @122 Return  ← 隐式 reject 块
Handler Table（磁盘 words）= [18,101,811,2, 21,68,545,3]
  → A=(18,101,101) 隐式包装器（range 覆盖 try 体 + 用户 catch 体）
  → B=(21,68,68)   用户 try/catch
```

两处修好它（`plan_async` + ① 规则，只对 12.x+ 生效）：

| 环节 | 做法 |
| --- | --- |
| 认隐式包装器 | `disp=None`（没有完成码分派 switch = 现代形态）时扫每个 handler 的 target 块：块里若调 `AsyncFunctionReject/RejectPromise` 就是隐式包装器 —— **用户代码的 `throw` 走 `Throw` 字节码**（探针 `rethrow` 证明），不会调这两个内建。消费掉该 handler（① 不再包它），整块标 `__gskip` 丢弃。重建后的 JS 里"异常自然外抛"就等于 promise reject，语义等价 |
| handler 起点落在机器码上 | 两个 handler 的 start(@18/@21) 都落在被折叠的 `Mov <context>`/state 搬运上 → ① 规则按 offset 精确匹配**永远匹配不到**。现在起点向后移到第一条真正会发射的指令，多个 handler 落到同一条时取 range 最小的（最内层）先包 |
| 结果 | `try { r5 = a0; r4 = await r5; r1 = r4; r5 = r1; return r5 } catch (e) { …; r6 = e; r6 = "caught:" + r6.message; return r6 }` —— 恢复垫片的 `ReThrow` 早被折叠掉，重建后的 `await` 被拒时天然在 try 体内抛，正好被用户 catch 接住 |

新 fixture `tests/fixtures/behav/async_try.js` 固化了三种形态：`withTry`（catch 里 return）、
`withRethrow`（catch 里 `throw e` → `Throw` 字节码）、`retry`（catch 体里再 await 一次，包装器与挂起点叠加）。
**node12.22.12 / 14.21.3 / 16.20.2 / 18.20.8 / 20.20.2 / 22.12.0 / 24.12.0 七个版本 3/3 用例全过**。

#### 10.6.0.2 node10（6.8）的完成码分派折叠：同样的 fixture 也全过

6.8 的 async 外面包一层"**完成码 + 尾部 `SwitchOnSmiNoFeedback` 分派**"：
每条出口（try 体正常完成、catch 体完成、隐式 reject）都先写"完成码寄存器 rC + 值寄存器 rV"
再 `Jump <dispatch>`，值由分派 case 0 里的 `ResolvePromise` 结算：

```
@83 LdaZero; @84 Star r4      ← 完成码 0
@86 Mov r0, r5                ← 值
@89 Jump [..] (@203)          ← 出口
...
@203 LdaTheHole; SetPendingMessage; …; @214 Ldar r4   ← 分派 preamble（出口指向这里）
@216 SwitchOnSmiNoFeedback { 0: @222, 1: @235, 2: @238 }
@222 Mov r2,r7; Mov r5,r8; ResolvePromise r7-r8; Ldar r2; Return   ← case 0：resolve(rV)
@235 Ldar r5; Return                        ← case 1
@238 Ldar r5; ReThrow                       ← case 2
```

线性发射只能渲染一份分派，做不到"每条出口各折一次"（try/catch 体里的 return 折不到，
其后死尾声块还会把 catch 结果覆盖成 `return undefined`）。做法（`plan_async` 的 6.8 分支）：

| 环节 | 做法 |
| --- | --- |
| 认 rC / rV | rC = 分派 switch 之前最后一条 `Ldar rX`（`@214 Ldar r4`）；rV = case 1 的 `Ldar rV; Return`（并可用 case 0 的 ResolvePromise 第二实参交叉验证） |
| 折出口 | 扫所有"指向分派（switch 或它前面那段 preamble）"、且**没被别的规则判成机器码**的 `Jump`，往前 4 条里找 `LdaZero/LdaSmi 0; Star rC` → 就地改名 `__greturn`，发射成 `return <rV>;` |
| 丢掉分派 | 折到出口后，`disp..函数尾` 整段标 `__gskip`（隐式 reject 块此前已被 6.8 包装器折叠跳过） |
| catch 体右端 | 6.x 的 catch 内联在代码中间，右端原先取"区间末尾"→ 现在还要截在**续接块**之前：`catch_start` 之前的跳转落进 catch 体（`early_jumps`，在改名之前记好）→ 那一段是 try 体与 catch 体共享的后续代码，必须落在 try/catch 之外。否则 `try{r=await p}catch(e){r="c"} return "r="+r` 的成功路径返回 undefined |
| 修好的形态 | `withTry`（catch 里 return）、`withRethrow`、`retry`（catch 里再 await）、`contAfter`（try/catch 之后还有代码，两路合并）、`maybeRet`（`try { if (await p) return 1 } catch { return 2 }`，try 体可能不 return）——**node10 全部逐用例与原文一致** |

顺带修掉一个 6.x 上下文读取的真语义坑（各版本都受益）：**`PushContext <reg>` 的 RegOut 写进寄存器的是「旧」上下文**
（源码 `interpreter-generator.cc`：`StoreRegisterAtOperandIndex(old_context, 0)`，所以 `PopContext <reg>` 才能靠它恢复）。
于是 catch 体里 `LdaImmutableContextSlot r8, [slot], [0]` 读的是**外层函数上下文**的槽 —— 原来我们的
catch 别名（当前上下文槽 = 抛出对象 → `e`）会把它也读成 `e`：
node10 的 `retry` 里 `await q`（外层参数）于是变成 `await e`（`retry:Error: …`）。
现在 `context_name(slot, via_current)` 区分两类读取：`*CurrentContextSlot` 才用 catch 别名与最内层作用域，
显式上下文读取跳过最内层作用域（读的是外层）。

**node8.17.0（6.2）同样走这套折叠**（见上），产物与原文逐用例一致（`async_await`/`async_try` 两个 fixture 3/3 全过）。

#### 10.6.1 本阶段修掉的 6.x 专有语义（按发现顺序）

1. **roots 表按 `RootListIndex` 枚举重建**（`heap.h` 的 `ROOT_INDEX_DECLARATION` 顺序：
   `STRONG_ROOT_LIST(159) + INTERNALIZED_STRING_LIST(197) + 私有/公开/众所周知符号 +
   STRUCT_LIST(32) + StringTable + SMI_ROOT_LIST`）。旧表的字符串段用正则抽取，
   **漏掉了字面量里带括号的条目**（`"(anonymous function)"`/`"(closure)"`/`"(?:)"`），
   于是整段错位（`length` 读成 `line` 之类）。现由探针实测校验：
   6.2 156/157、6.8 173/173 逐名命中（唯一"不一致"是 `dayperiod`/`dayPeriod` 的大小写）。
   - 众所周知的符号按 heap-symbols.h 的**描述**存（`Symbol.iterator` 等）→ 反编译时
     按计算键渲染 `obj[Symbol.iterator]`（点属性会取到 undefined）。
2. **对象/数组字面量的常量**：6.2/6.8 的数组常量是 `ConstantElementsPair`（一个 Tuple2：
   `{elements_kind, constant_values}`，`array_elem(o,0)` 的偏移正好落在 value2）；
   对象常量是 `BoilerplateDescription`（6.2 共用 FixedArrayMap → 只在字面量调用点
   `lit_ctx` 才敢按形状解；6.8 有专属 BoilerplateDescriptionMap）。计数格可能在**尾部**
   （`["a", hole, 3]`，count 是含展开属性的总数）。
   - `CreateObjectLiteral` 的 `RegOut` 语义：结果只进寄存器、**不写 acc**（写 acc 会把
     `LdaConstant "x"` 的键顶掉）；`StaKeyedPropertySloppy/Strict`、`StaNamedPropertySloppy/Strict`、
     `StaGlobalSloppy/Strict` 是 6.2 的独立字节码名。
3. **acc 活跃表要收全**：`ImplicitRegisterUse` 为空串是"完全不碰累加器"，早先被过滤掉 →
   `preserves_acc` 落到手工名单 → `LdaConstant "x"; CreateObjectLiteral …` 中间把 acc 当死值
   flush（`"x" in {x:1}` 直接算错）。
4. **ScopeInfo**：数值域比 7.8+ 多一格 StackLocalCount（`ContextLocalCount@5`、变量区@6），
   变量区顺序 = 形参名 + 栈局部首槽 + 栈局部名 + context 名 + infos；
   6.2 的 ScopeInfo 在 SFI 的**独立槽** `scope_info`（槽 3），名字在 `name_or_scope_info`（槽 2）。
5. **HandlerTable**：6.2 的表长是**槽数**、6.8/9.x+ 是**字节数**（一律按对象字节数取槽更稳）；
   6.2 的数据**每槽一个值**（低半恒 0），6.8+ 是紧密排布的 int32（一槽两个值）→
   按"偶数位是否恒 0"选视图；handler 偏移的移位 6.x 也是 `>>3`。
   - 6.x 的处理块**内联在代码中间**，所以 catch 体右端不能取"下一个 handler 的 target"，
     而要取到"重抛守卫"（`JumpIfFalse <join>; Ldar rX; ReThrow`）之后。
6. **try/catch/finally**：6.x 的 handler target 不等于区间末尾（入口在 end 之后几条指令，
   中间夹着另一条完成码设置）→ 判定放宽成"target ≥ end 且入口形态像 handler"；
   完成码 0 在 6.x 用 `LdaZero`（不是 `LdaSmi`）；try 体末尾留在 acc 里的副作用表达式
   要在 `} catch` 之前落地。
7. **ToObject 守卫**：`Ldar X; JumpIfUndefined <cold>; Ldar X; JumpIfNotNull <after>` 是
   "X 为 null/undefined 就抛"，单跳转规则会把冷块发两遍（第二遍成线性代码 → 后续不可达）→
   新增 `try_emit_null_guard` 合成 `if (X == null) { … }`。
8. **`TestUndetectable`**：6.x 带寄存器操作数，9.x+ 只测累加器（按 arg(0) 取会解出空串 →
   `return === undefined` 恒真 → IteratorClose 守卫失效，直接调 `undefined.return`）。
9. **for-in**：6.2 没有 `ForInEnumerate`，形态是 `ToObject rX; ForInPrepare rX, rA-rC`
   （对象寄存器在 ForInPrepare 的第一个操作数上）。
10. **生成器**：6.2 是另一套状态机（`Ldar rG; JumpIfUndefined <prologue>; …RestoreGeneratorState…;
    SwitchOnSmiNoFeedback {0:续体0, 1:续体1}`，挂起点是 `SuspendGenerator [k]; Return`，
    各续体在函数**开头**分派）→ 新增 `six_x_generator`：跳过序言/恢复分派/状态测试 shim，
    只留挂起点，并把"跳向已跳过指令"的跳转**按相对量重算**重定向到其后第一条活指令
    （相对量当绝对偏移用会把回边指错）。序言 `Ldar aK; StaCurrentContextSlot [S]` 记成
    槽→参数名别名，否则函数体里读的是源码名 `n` 而形参叫 `a0`。
11. **6.2 的 `r = yield v`**：恢复值在续体 shim 里经 `InvokeIntrinsic [_GeneratorGetInputOrDebugPos]`
    落到某个寄存器（`Star rK`）—— 该 shim 已被折叠掉，所以挂起点自己要发射 `rK = yield v;`
    （只发 `yield v;` 会让 `const a = yield n` 之后读到的是**上一个**寄存器的旧值：
    `target(1)` 原本 `1|11|30|true` 会算成别的数）。
    新增 fixture `tests/fixtures/behav/yield_expr.js` 覆盖这一形态（`const a = yield n` 链）。
12. **运行时前言**：6.x 用到一批 9.x 不需要的运行时/内建 —— `AppendElement`（数组字面量展开）、
    `ToFastProperties`/`InstallClassNameAccessor`/`DefineGetterPropertyUnchecked`（类装配）、
    `IsJSReceiver`/`Call`/`ToString`/`CreateIterResultObject`（迭代协议）、
    `NewScriptContext`/`NewTypeError`/`ReThrow`/`Abort`/`StackCheck`/`RestoreGeneratorState`、
    生成器版的 `GeneratorGet*`。缺了它们 Proxy 一律给"返回 undefined 的空函数"：
    `Counter = ToFastProperties(r3)` 会把类绑定写成 undefined。
13. **6.2 的 `DefineClass`** 签名是 `(extends, ctor, startPosition, endPosition)`，**返回原型**
    （9.x+ 返回构造函数、实参是 boilerplate/methods）→ 按"第 3/4 实参是不是数字"分流。
14. **语法门禁**：手写配平检查不识别正则字面量，会把 `node --check` 通过的产物判成
    "unbalanced bracket"而不输出 → 现在配平只是快速通道，不通过时用真引擎复核；
    失败信息也带上了上下文窗口（原先 `tail()` 返回空串）。

### 10.4 运行旧版 Node（本机实操）

```bash
# mise 的镜像很慢（~20 KB/s）；直连 nodejs.org 快 20 倍，且 ≤15 只有 x64 包（需 Rosetta）
curl -sL -o /tmp/node-v14.21.3-darwin-x64.tar.gz \
  https://nodejs.org/dist/v14.21.3/node-v14.21.3-darwin-x64.tar.gz
d=~/.local/share/mise/installs/node/14.21.3 && mkdir -p $d && \
  tar -xzf /tmp/node-v14.21.3-darwin-x64.tar.gz -C $d --strip-components=1
$d/bin/node --version   # 经 Rosetta 运行
```

#### 10.7 本轮（大语料扫出来的四个真 bug + fixture 门控）

把 `work/samples-all`（2446 份 test262/TheAlgorithms）分块扫完（`--offset` 是本轮给
`verify_samples.py` 加的口子）时，扫出四个**产物层面的真错误**（都是"语法判据"抓不到、
只有跑起来才看得见的那类）：

| 症状 | 根因 | 修法 |
| --- | --- | --- |
| node8 产物出现 `undefined /* hole */(<this>-<this>, …)`（语法错误） | `CallAnyReceiver` **所有版本**都是 `<callee Reg>, <reglist 接收者+实参>, <count>, [slot]` —— 被调在**第一个寄存器操作数**、不在累加器。旧实现按"被调在 acc"渲染，寄存器组被当实参文本吐出来（6.x 的 `super.m()` 就是这个形态） | 按 `CallProperty` 同样的语义渲染成 `callee.call(receiver, …)`；`Call`/`CallNoFeedback` 同形，一并修 |
| `obj?.raw ?? -1` 的兜底值变成 `0` | `LdaSmi` 的立即数用**无符号**解析（`[-1]` → parse 失败 → `unwrap_or(0)`）→ 负数 Smi 静默变 0 | 直接读操作数（`Imm/Idx` → i64） |
| 老族（6.2/6.8/7.8）闭包捕获的槽只剩 `__ctx.ctx4`，与外层写的名字对不上（`nest(2)(3)` → NaN） | `ScopeInfo` 的 `outer_scope_info` 读不到：`HasOuterScopeInfo` 的 flag 位随版本变（6.2 的 `FunctionKind` 是 **10 位** → 在 25；6.8 在 20；7.8 在 21；8.x+ 在 22），且老族变量区里 ReceiverInfo/PositionInfo 的有无也不同 | 位改成按家族派生（表可覆盖 `has_outer_bit`）；再补一层**兜底**：变量区里扫一个 ScopeInfo 引用（唯一、且只用于命名、有类型校验） |
| 内外两层箭头同名（V8 推断名撞车：都叫 `nest`）→ 两条 `function nest()` 互相覆盖，引用指到错的那个 | 声明名、`CreateClosure` 引用、常量池 SFI 引用三处各自按"原始名"渲染 | 新增 `Decompiler::function_emit_name`（按对象顺序去重：`nest` / `nest_2`…），三处统一走它 |

配套的测试改进：
- fixture 按"语法所需版本"拆开 —— `arrow_opt`（箭头/默认参数/访问器，九版本都能编译）+
  `optional_chain`（`?.`/`??`，cases.json 标 `min_node: 14`）；`behav_diff.js` 支持 `min_node`，
  低版本报 `skip` 并**单独统计**（`verify_behavior.sh` 的汇总里 `skip=`），不再与"产物有问题"混在
  一起 —— 之前 `arrow_opt` 的 cfail 一直掩盖着上面第 3、4 条真 bug：**它把 fixture 里最简单的
  闭包捕获一起挡住了**。
- 拆开后立刻暴露并修好了老族闭包捕获（第 3、4 条）与 `LdaSmi` 负数（第 2 条）。

回归证据（都是修后重跑）：
- 行为矩阵：九版本 × 25 fixture = **222 pass / 0 fail / 0 compile-fail / 3 skip**（skip 全是 `optional_chain` 在 8/10/12）。
- 标准语料 408 份 × 九版本：**syntax-fail=0 / decompile-fail=0 / ref-error(ours)=0**。
- 大语料 2446 份**逐块扫完九个版本**（`--offset` 分块，21 块共 22014 次跑，全部用**最终二进制**重跑）：
  node8 2066 / node10 2250 / node12 2348 / node14 2396 / node16 2402 / node18 2400 /
  node20 2406 / node22 2406 / node24 2423 跑，三项（syntax-fail / decompile-fail /
  ref-error(ours)）**全 0**；其余格子是该版本解析器编译不了样本源码（compile-env，
  随版本单调下降：380→196→98→50→44→46→40→40→23，正是"老版本解析器落后"的形状）。
- 真实复杂语料（`duplexpair.js`/`event_target.js`/`test-abortcontroller.js`，9 版本 × 3）：产物语法全 OK，
  其余格子是**该版本解析器编译不了源码**（`?.`/`??=`，node12/14/8/10）。
- `workspace/real/big.jsc`（485KB → 37662 行）node16 `node --check` 通过；`cargo test` 13 项通过。

**教训（写给下一次直扫产物的人）**：`work/out/*.js` 是**逐次扫描的缓存**，用旧二进制看它会得出
过时结论（本轮先看到两条 `Illegal return statement`，重生成后都 OK）。直扫前要么重生成、
要么以 harness 的即时 `--check` 为准。

#### 10.8 同名方法跨类（`twoclasses`）再扫出三个 bug

新 fixture `tests/fixtures/behav/twoclasses.js`（两个类各带同名实例方法 `bump` 与同名静态方法 `tag`）暴露了
"重名"这条线上更深的三件事：

| 症状 | 根因 | 修法 |
| --- | --- | --- |
| `B.tag is not a function`（九版本几乎全错） | 函数名去重后（`tag` / `tag_2`），`DefineClass` 的**按名兜底**（boilerplate 没给键、9.4 及更早的实例方法/静态方法都走它）拿 `f.name` 当键 → 挂成了 `tag_2` | 去重过的函数带一份**原始 V8 名**（`tag_2.__v8name = "tag"`），兜底按 `__v8name || name` 挂键 |
| 同上（首次修完仍错） | `__v8name` 的赋值排在函数声明之后 ✗ —— 而顶层类装配在**加载时**就跑（函数声明有 hoisting、赋值没有） | 改成在**前言之后**集中发一整块别名赋值 |
| 别名块的潜在雷 | 类构造器在产物里是 `var X = class {...}`（**不提升**）→ 别名块里的 `X.__v8name = …` 会对 `undefined` 赋值 | 别名行加 `typeof X === "function"` 守卫（构造器不需要别名：兜底挂键只处理 `dyn[3..]` 的方法）；fixture 里两个**同名类**（不同作用域）覆盖这条路径 |
| node24：`A.bump()` 返回 B 的值（绑定互换） | 13.x 的 `Lda/Sta*ScriptContextSlot` 读的是**脚本上下文**，它比普通上下文多一个 extension 槽（V8 `MIN_CONTEXT_EXTENDED_SLOTS = MIN_CONTEXT_SLOTS + 1`）→ 变量基准是 `min+1`；用 2 去减会把槽 3(A) 读成 `context_locals[1]`(B) | 槽基准按**作用域类型**取：脚本作用域用 `script_ctx_base()`（12.4 起 = min+1，之前 = min），其余用 min；`ScopeType` 的数值**随版本变**（8.4–12.4：`SCRIPT_SCOPE=4`；13.x 枚举重排后 `SCRIPT_SCOPE=0`），按 major 判定 |
| node22 同上一格 | 12.4 的**读**用 `LdaCurrentContextSlot [3]`（不是 Script 变体），而槽仍是脚本槽 → 光看指令名分不出来 | 改判据：链式查名时**按每个作用域自己的基准**取（`context_name_in_chain_base` 内 per-scope），脚本作用域用脚本基准 |

顺带修掉一个一直在的读错：**`ParameterCount` 的偏移**在 7.8–12.x 上少减了一格
（`(clc_slot - 2)` 读到了 `Flags`：node12/16/22 的 param 读成 47554/94658 这种"旗标值"）；
6.x 因为数值域多一格 `StackLocalCount` 才是 -2。现在九版本都读出正确的参数个数。

回归（全部用最终二进制）：矩阵 **222 / 0 / 0 / 3**（九版本 × 25 fixture）；标准语料 408 × 九版本三项全 0；
大语料 2446 份在 node8/10/12/14/16/18/20/22/24 **九版本**全扫（`--offset` 分块，21 块 22014 跑）三项全 0。

#### 10.9 工程收尾：零告警 + 双语 README

- **编译告警清零**：`cargo build --release`、`cargo clippy --release --all-targets` 都是 **0 告警**。
  清掉的东西：10 处 `matches!` 里的重复分支（同一个名字写两遍 → unreachable pattern）、
  2 处多余的 `mut`、以及一批死代码（`const_depth`/`temps`/`FnCtx.name`/`Handler.depth` 字段、
  `temp`/`fn_name`/`ldar_reg_text`/`call_args`/`sfi_script_slot`/`array_elem`/`decode_smi`/`hex`
  方法与常量 `SFI_SCRIPT`）。clippy 侧另有 28 条（`needless_range_loop`、多余括号、`same_item_push`、
  doc 缩进…）一并处理。清理后回归：矩阵 **222 / 0 / 0 / 3**、单测 13 项、样本 805 跑全绿。
- **README 双语**：`README.md`（英文）与 `README.zh.md`（中文）重写为 GitHub 风格并互链 ——
  现实说明（源码文本不在 .jsc 里）、特性、安装、用法（含 `info` 真实输出）、版本表（九组 Node↔V8）、
  实现原理、验证门禁与数字、仓库结构、已知限制、许可与致谢。README 里出现的每条命令都实跑过。

#### 10.10 发版流水线（.github/workflows/release.yml）

打 tag 即发版：tag 名就是版本号（`build.rs` 用 `git describe --tags --exact-match` 注入
`JSCD_VERSION`，`jscd --version` / `jscd version` 直接打印它 —— 实测 `git tag v0.0.0-test`
后重建，两个命令都输出 `jscd v0.0.0-test`）。

- **六个产物**：linux-amd64（`x86_64-unknown-linux-musl`，musl-tools 当链接器）、
  linux-arm64（`aarch64-unknown-linux-gnu`，`gcc-aarch64-linux-gnu` 交叉）、macos-arm64（原生）、
  macos-amd64（在 arm64 runner 上交叉，Apple SDK 双向支持）、windows-amd64、
  windows-arm64（在 x64 runner 上交叉到 MSVC ARM64）。
- **依赖全纯 Rust**（`brotli` / `serde` / `clap`，无 `-sys`、无 `cc`）→ 交叉编译只需要链接器。
- **UPX**：linux/amd64、linux/arm64、win64/amd64、win64/arm64 压（格式支持取自 upx 5.2.1 自带的
  `--help` 清单：`amd64-linux.elf` / `arm64-linux.elf` / `amd64-win64.pe` / `arm64-win64.pe`）；
  **macOS 不压** —— 本机实测 upx 对 Mach-O 直接拒绝（`macOS is currently not supported
  (try --force-macos)`），产物字节不变且仍可运行。UPX 步骤是"尽力而为"：失败只打印一行、
  保留未压缩产物，不让发版失败。
- **不做二次压缩**：产物以裸文件挂到 Release（走 `action-gh-release`），刻意**不用**
  `upload-artifact`（它一定会打成 zip）；只有手动演练（`workflow_dispatch`，无 tag）才走
  workflow artifact，且把 `compression-level` 设为 0 并在步骤名里标明仅演练。
- 发版前先过 `cargo test --release`；`permissions: contents: write` 只给到 Release 所需。

#### 10.11 命令行重做（对齐 ddc 的形态 + 双语）

形态：

- `jscd [选项] <输入> [输出]` —— 不写子命令就是**反编译**；输入是目录时递归处理，
  保持层级写到输出根目录。输出缺省：文件 → stdout；目录 → 输入同级的 `<输入名>-out/`
  （`dist` → `dist-out`，`.` 先 canonicalize 成真实目录名）。
- `jscd <子命令> [参数…]` —— info/strings/functions/disasm/decompile/ro-map。
- 空参数、`-h/--help`、`help [子命令]` → 帮助（含名字/版本/仓库地址/语言说明/Usage 三行/
  INPUT-OUTPUT 语义/选项/子命令/示例）；`-v/-V/--version`、`version` → 名字+版本+仓库。
  这些旗标由 `main.rs` 先拦截，clap 自带的英文 help/version 关掉，避免两套文案。

双语（`src/lang.rs`）：`JSCD_LANG` → `LC_ALL` → `LC_MESSAGES` → `LANG` → 系统区域设置
（Windows 走 `GetUserDefaultLocaleName`，经 `windows-sys`；macOS 读 `defaults read -g AppleLocale`）
→ 英文。任何 `zh*`（含 `zh_TW`/`zh_HK`/`zh_MO`/`zh_SG`）都算中文；`C`/`POSIX` 视为"没说"，
继续往下找。实测：`JSCD_LANG=zh` → 中文、`JSCD_LANG=en` 压住 `LANG=zh_CN`、`LC_ALL=zh_TW.UTF-8`
→ 中文、`LANG=fr_FR` → 英文、清空全部环境变量后 macOS 读到 `AppleLocale=zh_CN` → 中文。
报错也本地化：clap 的 `ErrorKind` 映射成"未知选项 '--nope' —— 用法见 `jscd --help`"这种句子。

产物名与仓库地址都换成 `https://github.com/ejfkdev/jscd`（`Cargo.toml` 的 repository/homepage、
两份 README、CI 的 clone 行）。

验收：`tests/cli.rs` 11 项（空参 help、-h/--help/help/子命令 help、-v/-V/--version/version、
六种语言取值、未知选项退出码 2 + 中英文案、-o 与位置参数冲突、目录+-、空目录提示、
文件→stdout/文件/`-`、目录→`<名>-out` 且保层级、显式输出目录、非 .jsc 不处理）——
需要 `.jsc` 的用例现场用 `node scripts/mkcorpus.js` 编译，环境里没有 node 时跳过。
全量回归仍是矩阵 **222 / 0 / 0 / 3**、clippy 零告警、`cargo test` 26 项全过。

#### 10.12 bytenode 的 CommonJS 包装函数必须摊平（hello world 例子扫出来的）

bytenode 的 `compileFile` 对 `.js` 会先 `Module.wrap` 再编译：

```js
(function (exports, require, module, __filename, __dirname) { <源码> });
```

于是 `.jsc` 的顶层是**闭包工厂**（脚本体只有 `CreateClosure [k]; Star0; Return`），而
bytenode 运行时（`Module._extensions['.jsc']` → `runBytecode`）真正**调用的是那个包装函数**。
我们原来只把含 `DeclareGlobals` 的函数摊平 → 真实 bytenode 产物里的顶层语句被包成
`function _anon_3(a0…a4)` **从不执行**：`jscd hello.jsc` 出来的文件跑起来什么都不打印。

修法（`FnCtx`）：

- 顶层 SFI（`cache.top_sfi`）若形如"闭包工厂"（只允许 `CreateClosure/LdaConstant`、
  `Star*`、`Mov`、`Ldar`、`Return` 等壳指令）→ 记下那个子 SFI 为**包装函数**；
- 工厂自身不输出（它是 bytenode 的胶水，没有可执行语句）；
- 包装函数按顶层摊平（`inline_body`），参数名按 Node 的 CJS 约定给回
  `exports, require, module, __filename, __dirname` —— 摊平后的 `require(...)`/`module.exports`
  既好读，也能在 `node x.js` 下直接跑。

实测（`tests/cli.rs::cjs_wrapped_jsc_runs_top_level_code`）：`mkcorpus --module`（= `Module.wrap`）
编译 hello world → 产物顶层被摊平 → 在 vm 里跑起来读到 `hello world`；九个版本
（8.17.0–24.12.0）用 `--module` 编译同一份 hello.js，**产物都能打印 hello world**
（22.12.0/24.12.0 需带 `--ro-map`：`log` 这类内建名在 V8 12.4+ 存于只读堆）。

另：`lang.rs` 去掉了 macOS 上 `defaults read -g AppleLocale` 的子进程调用 —— 取语言不再
fork 任何进程（Windows 那条本来就是进程内 API）；macOS 只认 `JSCD_LANG/LC_ALL/LC_MESSAGES/LANG`，
环境变量全空时落回英文。

#### 10.13 语言检测：命令行环境变量优先，系统 API 只作兜底且防崩

Windows 的 cmd / PowerShell 默认不导出任何语言变量，所以 10.12 末尾那条"Windows 用进程内 API"
仍是必要回退；但按"少碰系统 API、失败也不能崩"的要求重排了顺序：

- **先问环境变量**（不碰 API、不起子进程）：`JSCD_LANG` → `LC_ALL` → `LC_MESSAGES` →
  `LANGUAGE` → `LANG` → `LC_CTYPE`。后两个是为 Windows 上的 Git Bash / MSYS / Cygwin 准备的 ——
  它们只设 `LC_CTYPE`/`LANGUAGE` 也能被认出来；`C`/`POSIX`/空值一律当"没说"，继续往下问。
- **只有全都没答上来**才在 Windows 上调一次 `GetUserDefaultLocaleName`（进程内、每进程一次）。
- 那次调用整段裹 `catch_unwind`，且长度做夹取：返回 0、返回越界长度、UTF-16 解码失败、
  甚至真出 panic，都退化成 `None` → 英文，**不会崩**；macOS / 其它 Unix 的 `os_locale()`
  直接返回 `None`。
- 新增 `src/out.rs`：`jscd x.jsc | head -3` 这种下游提前关管道（EPIPE）的用法，
  原来走 `print!` 会 panic（Rust 默认忽略 SIGPIPE，写失败就 panic）；现在 stdout/stderr
  全部经 `out::stdout`/`out::stderr_line`，断管按 Unix 惯例安静收场（退出码 0），
  其它写错误也只报一行、不 panic。

验证：`cargo check`/`clippy --all-targets` 针对 `x86_64-pc-windows-msvc` 交叉编译零告警
（本机 PATH 上 Homebrew rustc 在前、`kache` shim 抢了裸 `rustc`，需显式
`PATH=<rustup 工具链>/bin:$PATH` 才能查到 windows std —— CI 用 rustup action 不受影响）；
环境变量优先级有单测（`env_priority_and_fallthrough`）；`--help | head -1` 退出码 0、无 panic。

#### 10.14 不支持的 V8 版本必须报错，不能"读通就算成功"（用户报的 Node 26 空产物）

用户在 Node 26 上编了份 hello.jsc（`a9 06 de c0` / V8 14.6.202.34），`jscd` 输出的
除运行时前导外**什么都没有**，退出码还是 0。原因不是"没识别出版本"——`identify`
爆破出了 14.6.202.34，只是没有 14.6 的表，于是走了"逐个试全部内嵌表"的兜底，
而 14.6 的 payload 撞上 13.6 的表时**反序列化一路不报错**：对象图读出来了（能看到
`Script`/各种 Map），但 SFI 一个都没有、字节码为空 —— 渲染出来就只剩前导。

修法（`src/cli.rs`）：

- 候选表解析成功后加**结构门禁** `cache_has_code`：必须至少有一个挂着
  `BytecodeArray` 的 SFI。空源码也会编出 `StackCheck; Return`，所以"一个都没有"
  只可能是表与 payload 不是一路货 → 该候选判失败，继续试下一张表。
- 全灭时的报错分两种口吻（`unsupported_msg`）：版本**在支持范围外** → 只报
  "由 V8 x.y.z 生成（version hash 0x…）；当前支持 Node 8.17.0–24.12.0（V8 6.2–13.6）"，
  不再把 18 张候选表的失败记录甩给用户；版本**在范围内**（截断/损坏）才附前 4 条候选错误。
- `info` 增加 `supported:` 行（`Identified::supported()`），JSON 同步加 `supported` 字段：
  "识别得出"与"能反编译"从此分开说 —— 14.6 会显示 `supported: no`。

回归：`tests/cli.rs::unsupported_v8_version_fails_loudly_instead_of_emitting_an_empty_prelude`
用 `mise exec node@26.10.0` 现编一份真产物（环境里没有 node26 就跳过，CI 不受影响）断言
非零退出 + 报错点名版本 + stdout 不再吐半份产物 + `info` 标 `supported: no`；
`unsupported_msg` 两种口吻各有单测（CI 里也能跑）。

另：**同族但不同 build 的文件照旧能解** —— 例如 node 24.21.0（V8 `13.6.233.17-node.53`，
hash 与 24.12.0 不同、爆破只认到 13.6.233.17）走兜底 + 门禁，decompile 正常。

## 11. 全版本覆盖：Node 8.0.0 → 26.10.0（本轮）

目标：**Node 8 起到最新版的每个发布都要能解**；取数只许碰"需要的那几个文件"，不许拉整份源码；
相同配置合并、不同配置按版本存。

**覆盖面**（`tables/manifest.json`，510 条）：dist/index.json 里 ≥ 8.0.0 的全部发布 —— 36 个
V8 minor（5.8 – 14.6）、90 个 V8 版本（4 段）。每个发布都有自己的 version_hash 条目，所以
`jscd info` 从"爆破猜版本"升级为**精确命中**（`exact`）。

**取数改造**（`scripts/codegen.py`）：

- 不再需要 `--node <完整克隆>`。默认用一个 **blob 过滤的部分克隆**
  （`git clone --filter=blob:none --no-checkout --depth 1`，2.3 MB）＋ 每个 tag 只 `fetch` 树，
  再用**一次** `git cat-file --batch` 把它要的那 ~30 个 V8 源文件的 blob 合并成一轮网络往返取回；
  取回的文件写进 `workspace/node-src/<tag>/…` 缓存，之后离线可重复生成。
- 本地 git 能看到 tag 时，查不到的文件就确定是"这版没有"（`.404` 哨兵），不再去打
  raw.githubusercontent（某些网络里是黑洞）；传输错误一律抛，不会把网络故障伪装成"缺文件"。
- `--all-from 8.0.0`：从 index.json 按 **V8 4 段版本**挑每版最新的 Node 发布当 tag，自动枚举。
- `--donors tables/ --per-minor --keep-existing`：新表缺的字段从最近的已知表**继承**
  （老族里 string 布局、ScopeInfo 位域、legacy 标签这些源码里造不出来）；每个 minor 只留一张表，
  已有的人工调过的表**不覆盖**（逐字节保持）。
- 去重仍是按内容哈希：同内容的表共用一份文件；不同内容才另存 —— 最终 36 张表 2.0 MB。

**顺手修掉的真 bug**：`hash.algorithm` 的折叠探测。V8 ≥ 12 用 `base::Hasher`（左折叠），
但 `Hasher` 从 `src/base/functional.h` 搬到了 `src/base/hashing.h`，旧探测在 22/24/26 上全都
判成右折叠 —— manifest 里那些哈希因此对不上真文件（只能靠爆破兜底）。实测校验：
node 22/24/26 产物头里的 version_hash 与**左折叠**计算值逐位相同；已修正 12.4/13.6 两张表并让
生成器把 hashing.h 也读上。

**逐版本验证（已铺开）**：每个 minor 的代表版本装好后跑冒烟（arith/class_basic/closure/try_catch）：

| V8 | Node | 结果 |
| --- | --- | --- |
| 6.2 – 13.6 | 8.17 / 10.24 / 12.22 / 14.21 / 16.20 / 17.9 / 18.2 / 18.20 / 19.1 / 19.9 / 20.20 / 21.7 / 22.12 / 23.11 / 24.12 | **全过**（9 条原线另跑满矩阵 222/0/3）|
| 9.5 / 9.6 / 10.1 / 10.7 / 10.8 / 11.8 / 12.9 | 17.1 / 17.9 / 18.2 / 19.1 / 19.9 / 21.7 / 23.11 | **新表直接全过** |
| 14.1 | 25.9 | 3/4 →（下述两个修复后）class_basic 也过 |
| 14.6 | 26.0 / 26.10 | 1/4 → 3/4；满 25 fixture 矩阵：25.9 与 26.10 合计 pass 19 / partial 7 / fail 24 |
| 9.0 – 9.3 | 16.3 / 16.5 / 16.8 | payload 起点与 9.4 不同（unexpected end of payload）→ 待做 |
| 5.8 / 6.0 / 6.1 / 6.6 / 6.7 / 7.0 / 7.6 / 7.7 / 7.9 / 8.1 / 8.6 | 8.2.1 / 8.6.0 / 8.9.4 / 10.3.0 / 10.8.0 / 11.15.0 / 12.10.0 / 12.15.0 / 13.14.0 / 14.4.0 / 15.14.0 | 老族头部/位置布局差异 → 待做 |

结论：**"能识别"已经全版本打通**（510 个发布精确命中）；**反编译质量**在 V8 9.4 – 13.6 全段达标
（含 Node 17/18/19/21/23 这些新纳入的奇数 major 线），V8 14.x 与 V8 ≤ 9.3（含 5.8–7.9 老族）
还需按家族补差异 —— 这几支的表已生成、设备与脚本已就绪，剩下的是逐家族对差异。


### 11.1 V8 14.x 的两处根因（已修）

1. **操作数种类改名**：V8 14 把 `bytecode-operands.h` 里的操作数类型从"编码形态"改成"语义名"
   —— `kIdx` 变 `kConstantPoolIndex`，另加 `kFeedbackSlot`/`kContextSlot`/`kCoverageSlot`/
   `kAbortReason`/`kEmbeddedFeedback`。解码器只认历史那套名字，于是 14.x 的池索引被当成未知
   种类：**顶层闭包工厂的识别拿不到子 SFI**（`child=None`）→ bytenode 的 CommonJS 包装不再摊平，
   常量也跟着解成 `undefined`。修法：生成器加 `normalize_operand_types()`，把新名按
   size/scalable 逐项对齐折回老种类（`ConstantPoolIndex/ContextSlot/FeedbackSlot/CoverageSlot → Idx`，
   `AbortReason → Flag8`，`EmbeddedFeedback → Flag16`），且在 donor 继承**之后**执行（否则 donor 会把新名带回来）。

2. **脚本上下文不再用 cell**：`LdaCurrentScriptContextSlot`、`LdaScriptContextSlot`、
   `LdaLookupScriptContextSlot(InsideTypeof)`、`StaScriptContextSlot`、`StaCurrentScriptContextSlot`
   六条被等价的 `*ContextSlotNoCell` 取代（操作数形状逐条一致）。修法：`decompile::canonical_name()`
   在解码后折回老名字（`CreateFunctionContextWithCells → CreateFunctionContext` 同理），
   下游按名字分派的那一大片逻辑不用逐处加分支；`jscd disasm` 仍按表中真名输出。

修完：26.10 的 class_basic 恢复成 `r0 = Counter; return new r0(r1);`（与 24.12 一致），
冒烟 3/4，满矩阵 19/50。**剩下的 14.x 差异**（string_regex / generator / async / twoclasses /
operators 等）多半与新指令有关：`Add_StringConstant_Internalize`（字符串常量内化加法）、
`ForOfNext`（for-of 快路径）、`SetPrototypeProperties` —— 表里都已经有它们的操作数形状，待逐个补渲染。

### 11.2 V8 14.x 全绿（Node 25/26 = 50/50）与其余版本的校准

昨天 14.x 还停在"冒烟 3/4、满矩阵 19/50"，现在 **25.9.0 + 26.10.0 的 25 fixture 矩阵 50 pass / 0 fail**，
原 9 条线 + 25/26 合计 **272 pass / 0 fail / 3 skip**。新增的根因（都是"表里一条不影响识别的常量错"）：

1. **运行时函数表（Runtime::FunctionId）错位 1**：14.x 的 `runtime.h` 用 `#ifdef V8_DUMPLING` 多包了两条
   （PrintDumpedFrame / DumpExecutionFrame），Node 不发这个开关 —— 留着会让 364 以后的 id 整体 +1，
   于是顶层 `CallRuntime [DeclareGlobals]` 被解成 `DeclareEvalVar`，**顶层代码不再摊平**（class_basic 挂在这）。
   另外两条"直接条目"的 `IF_` 宏（`IF_SPARKPLUG_PLUS(F, MaybePatchBinaryBaselineCode, …)` 这类）
   老写法只认 `FOR_EACH_` 开头的参数，会把整条漏掉。定标方法：让 **V8 自己打印字节码**
   （`node --print-bytecode --no-lazy <源文件>` → `CallRuntime [真名]`），拿真名与我表逐个对。
2. **roots 表的 torque 段条数**：14.x 比 13.6 少 4 条（`TORQUE_MAP_COUNT` 从 36 改 32）。
   锚点：twoclasses 的类 boilerplate 键是 `Root(1027) = String:constructor`，用 36 时算成 1031。
   同一方法顺带校准了 **node19（10.7/10.8 → 25）** 与 **node21（11.8 → 34）** —— 这两条线的
   `constructor` 根分别是 693/707/735。校准脚本后来固化成一句话：拿 class_basic 产物里
   boilerplate 第二格的 `Root(n)` 与表里 `String:constructor` 的下标求差。
3. **脚本上下文的 extension 槽在 14.x 没了**：`LdaCurrentScriptContextSlot` 一族被
   `LdaCurrentContextSlot` 取代且槽号 −1（实测 13.6 `[3]/[4]` ↔ 14.6 `[2]/[3]`），
   `script_ctx_base()` 的 +1 只对 12.4–13.6 生效，否则 A/B 两个类变量都算成第一个。
4. **类模板（DescriptorArray）起点随版本变**：≤13.6 从元素 1 起、14.x 从 2 起（首格变成裸数据）。
   改成按"第一组能不能解出键"试起点 1..3，与版本无关。
5. **老族判定不能看 `kSpaceMask` 是否存在**：9.0–9.3 的 serializer 源码里那些常量还在，
   但 .jsc 已是新格式（16.3 与 16.20 的 payload 起点字节逐位相同）→ 改判 **V8 major ≤ 8**；
   同理 `serialization.legacy` 段只对 ≤8 输出（继承之后要再清一遍，否则 donor 会带回来）。

**逐版本校验通道（可复用）**：`node --print-bytecode --no-lazy` 打印真名 ↔ 我们的 disasm 打印原始下标；
两者一配就是"id → 真名"。比翻 V8 源码猜宏展开顺序可靠得多（`IF_*`、`#ifdef`、`IF_WASM_DRUMBRAKE`
这些开关的取值最终由真构建决定）。

**仍然欠着**：9.0（16.3 能解析、函数名/形参个数还差一点；9.1/9.2 已全过）、5.8–8.6 老族（头部/位置布局）、
ro-map 内嵌（现在 22+ 还要 `--ro-map`）。

### 11.3 逐线满矩阵复核（纠正"冒烟=全绿"的误判）

冒烟只跑 4 个 fixture，会把"14/25 通过"的线看成全绿。按**完整 25 fixture 矩阵**复核后的真实分档：

| V8 | Node 线 | 满矩阵 |
| --- | --- | --- |
| 6.2 – 6.8、7.8、8.4 – 13.6 | 8.17 / 10.24 / 12.22 / 14.21 / 16.20 / 18.20 / 20.20 / 22.12 / 24.12 | **全过**（222 + 50）|
| 9.4 / 10.8 / 11.8 / 14.1 / 14.6 | 16.20 / 19.9 / 21.7 / 25.9 / 26.10 | **25/25 全过** |
| 12.9 | 23.11 | 24/25 |
| 8.6 / 9.0 / 9.1 / 9.2 / 9.6 | 15.14 / 16.3 / 16.5 / 16.8 / 17.9 | 14/25 |
| 7.9 / 8.1 | 13.14 / 14.4 | 21–23/25 |
| 5.8 / 6.0 / 6.1 / 6.6 / 6.7 / 7.0 / 7.7 | 8.0–8.9 / 10.0–10.8 / 11.x / 12.x | 解析阶段就失败 |

本轮顺带修掉的两个"只在某些行暴露"的问题：

- **老族 acc 拼写**：≤8.6 的源码写 `AccumulatorUse::kWrite/kReadWrite`，提取器只把长名
  （`kWriteAccumulator`）算作"写"→ 整张老族表把 `Add`/`Call…` 标成只读 → 调用结果被当死值丢掉
  （node15 的 class_basic 少两行语句、算错数）。补上短名后 node15 全绿。
- **ScopeInfo 数值域起点**：9.0（node16.0–16.3）的 ScopeInfo 比 9.1+ 多一格，按槽 1 读会把函数名
  和变量名全丢（产物里函数叫 `_anon_4`、`n` 变 `__ctx.ctx2`）。改成"两个起点都试、按解出的
  名字数量挑"，与版本无关。

**8.6 / 9.0–9.2 / 9.6 这五条线剩下的 9 个失败**都卡在同一处：这些 fixture 需要**只读堆里的属性名**
（`join`/`toUpperCase` 之类），而它们的 ro-map 只有个位数条目 —— 探针法在这几版上产不出足够的
`<roN_off>` 占位（1997 个名字的探针只留 14 个占位，13MB 全量探针反编译还会超时）。下一步要么
让老族的只读堆引用统一走 `<ro…>` 占位，要么给这几版换一种建表方式。

### 11.4 老族打通到 19 条线（本轮）

这轮把 **7.9 / 8.1 / 8.6 / 9.0 / 9.1 / 9.2 / 9.6 / 12.9** 全部或几乎打通，合计 **19 条 Node 线**
通过完整 25 fixture 矩阵（原来 9 条 + 19.9/21.7/25.9/26.10 + 13.14/15.14/16.3/16.5/16.8/17.9/23.11）。
五处根因：

1. **ro-map 是拿旧二进制生成的**：这几条线的 ro-map 只有 4–14 条（`join` 之类全缺），换成修完
   的二进制重跑就恢复到 39 条 → 五条线一起变绿。教训：**ro-map 要跟二进制一起重建**。
2. **老族 acc 拼写**：≤8.6 源码写 `AccumulatorUse::kWrite/kReadWrite`，提取器只认长名 → 老族整张表
   把 `Add`/`Call…` 标成只读，调用结果被当死值丢掉（node15 少两行语句）。
3. **ScopeInfo 数值域起点自适应**：9.0 比 9.1+ 多一格 → 函数名/变量名全丢（`_anon_4`、`__ctx.ctx2`）；
   改成两个起点都试、按解出的名字数量挑。
4. **ScopeType 顺序的版本边界是 12.9 不是 13.0**：`scope-info.tq` 里 `SCRIPT_SCOPE` 从 node23 起排第一
   —— 12.9 的存储还是 Script 变体、读取已换成普通变体（"半改"状态），判错时脚本作用域当函数作用域，
   槽基准少 1 → 两个类变量全读成 B（`r2 is not a constructor`）。边界改成 `major>=13 || 12.x && minor>=9`。
5. **donor 继承把 null 当缺失**：7.9 的 `scope_info` 抽不出来（源码里不是 .tq）→ 生成 `scope_info: null`，
   只补"缺键"时 donor 配置永远补不上 → node13 的闭包变量名全丢。顺带把"只许借更低版本"的规则
   限定到 major ≤ 7（8.1 借 8.4 而不是 7.8，否则 `min_context_slots=4` 这类老值会把类变量解析错）。

**还剩**：14.4（8.1）的 generator/yield_expr；5.8/6.0/6.1/6.6/6.7/7.0/7.7 的 payload 格式（解析阶段）。

### 11.5 24 条线全绿 + 老族的边界梳理

**新增全绿**：14.4（8.1，靠"生成器序言允许 `StackCheck` 前缀"）、13.14（7.9）、15.14（8.6）、
23.11（12.9）、17.1（9.5）、18.2（10.1）、19.1（10.7），加上此前的 19 条 —— 共 **24 条线**
通过完整 25 fixture 矩阵（另有 7.5/7.6/9.3 已过 arith，正在补满矩阵）。8.1 那条值得记一笔：
V8 8.1 的生成器开头比 8.4 多一条 `StackCheck`，而 `plan_generator` 死认"第 0 条是
`SwitchOnGeneratorState`"，于是整个状态机重建被跳过（输出空串）。改成在前 8 条里找分派点、
把前缀当机器码跳过。

**老族现在只剩这几族**（都是 payload 格式级，需要各自移植）：

| 族 | 代表 | 现状 |
| --- | --- | --- |
| 7.5 / 7.6 | 12.5 / 12.9 | arith 已验证通过，待满矩阵 |
| 7.4 | 12.0 | 解析失败 |
| 7.0 / 7.7 | 11.15 / 12.15 | 解析失败（7.0：unknown tag 0x41；7.7：unexpected end）|
| 6.6 / 6.7 | 10.3 / 10.8 | 6.7 能"解析但不含函数"；6.6 更早失败 |
| 6.0 / 6.1 | 8.6 / 8.9 | header 的 version_hash 读成 0（头部布局差异）|
| 5.8 | 8.2 | payload 起点不对 |

**一条被证伪的捷径**（记下来免得再走）：用"格式别名"把 6.6/6.7 指到 6.2/6.8 的表 ——
`JSCD_TABLE` 调试路径会**绕过结构门禁**，于是"退出码 0"被误判成可用；真上满矩阵，
6.7 除 arith 外 12 个 fixture 照挂。别名机制留在 `TABLE_ALIAS`（当前为空），有证据再填。

### 11.6 26 条线全绿 + 内嵌 ro-map + 老族的最后清单

**新增全绿**：8.1（14.4，生成器序言允许 `StackCheck` 前缀）、8.3（14.5）、9.3（16.9）、9.5（17.1）、
10.1（18.2）、10.7（19.1）、12.9（23.11）、7.9（13.14）—— 连同此前的一共 **26 条线**（覆盖 36 个
V8 minor 中的 30 个）。老族只剩 7.5/7.6（各 20/25）与八条解析失败的分支（5.8/6.0/6.1/6.6/6.7/7.0/7.4/7.7）。

**两个新修**：

1. **7.x 的众所周知的符号根**：7.5/7.6 的 roots 表里存的是内部拼写（`Symbol:iterator_symbol`），
   渲染器只认 `Symbol.iterator` → 输出 `obj[undefined]` → destructure / spread_rest / for_of_in /
   generator 一起挂。加一张内部名 → JS 名（13 个众所周知的符号，ECMAScript 固定）的映射后三个恢复。
2. **生成器序言前缀**：V8 8.1 的生成器比 8.4 多一条开头 `StackCheck`，而 `plan_generator` 死认
   第 0 条是 `SwitchOnGeneratorState` → 状态机重建被整段跳过（输出空串）。

**内嵌 ro-map（尽力而为）**：`--ro-maps` 让生成器把 `workspace/ro/ro-map-<node>.json` 按
**精确 V8 版本**收进 `tables/ro_map_*.json` 并内嵌（29 个版本）→ Node 22+ 开箱即用。注意
只读堆地址 (chunk/offset) 是**按构建**编号的：同一 V8 版本的不同 `-node.NN` 补丁也会错开，
所以内嵌表**精确匹配**，不匹配就当没有（宁可 `<ro0_…>` 占位也不给错名字）；要绝对准确
仍可用 `scripts/build_ro_map.sh` 按自己的构建生成再 `--ro-map`。

**仍未做**：5.8/6.0/6.1/6.6/6.7/7.0/7.4/7.7 的 payload 格式（解析阶段就失败）；7.5/7.6 的
`destructure`（DeclareGlobals 数组的编码差异）。

### 11.7 默认剥离运行时前导（`--runtime` 反向开关）

用户反馈：反编译结果里很大一部分是 `__runtime`/`__intrinsic` 占位实现那 200 行，读代码时噪声太大。
改成：

- **默认只输出还原出来的代码本身** —— 开头两行说明（"想直接 `node` 跑就加 `--runtime`"），
  然后是顶层代码与各函数；被摊平的上下文变量声明（`var a, b;`，很短且只在真有闭包变量时出现）保留，
  因为摊平后的代码靠它才说得通。名字别名块（`X.__v8name = "raw"`）只在 `--runtime` 时才发。
- `--runtime`（`decompile` / 默认形态都支持）保留原来的可运行前导：`__runtime` 代理
  （V8 内建最小实现）、上下文变量声明、别名块 —— 产物可直接 `node` 跑。

配套：对拍/语料脚本（`scripts/behav_diff.js`、`scripts/verify_samples.py`）与 CLI 测试改成显式
`--runtime`（它们测的正是"产物能不能跑"），并新增一条用例断言默认输出没有前导定义、`--runtime`
明显更长。README 双语同步：快速上手改用真实输出（node 20.20.2 的 20 行默认结果 + `--runtime`
跑出 `hello world`），旗标表与"前导"段落改写。

### 11.8 默认去掉样板注释 + 产物保守优化（用户要求）

用户要求：① 去掉产物里多余的注释；② 能把冗余代码优化掉（典型是"某个变量只用一次就不用定义"）。

**注释**：默认输出不再发任何样板横幅（顶层那段 `// @generated by jscd …` 与每个函数前的
`// @generated by jscd — 源码文本不在 code cache 中…` 都没了），只留 `// ── 模块/脚本顶层代码`
这类**结构性分隔**。`--runtime` 时仍保留（那是可运行前导的一部分）。

**优化（`src/opt.rs`，纯文本后处理，输出形态很规整：一行一条语句）**：

1. `DeclareGlobals` 行直接删 —— V8 的簿记，源码里没有对应写法，前导里它本来就是空实现；
   参数随之没人读，交给第 3 条。
2. `let r0, r1, …;` 只留还被读/写的寄存器（连"真没出现"的才删，否则赋值会变成隐式全局）。
3. 死存储：反向扫每个寄存器"下一个动作" —— 之后**先被重写**才被读、或之后再没出现，
   且右值是稳定值（字面量/寄存器/纯数组）→ 删掉这次写。
4. 单次使用前送：`rX = E;` 与唯一的读**紧邻**、`E` 是字面量/寄存器/紧邻的裸标识符、
   这次写的值之后没人读、且该寄存器在整个**函数区域**里只被读一次 → 把 `E` 并进读处、删掉赋值。

**为什么这么保守**（这一轮踩过的坑，都记在代码注释里）：文本级分析没有控制流信息，
跨行/跨分支的搬运会踩到**寄存器复用**与 `phi` 汇合点 —— 实测 `operators`/`async`/`try_catch`
一批 fixture 就是这么挂的；多趟迭代更糟（后一趟看到的是"已经删过的行"）。现在只做**能证明
等价**的形态，且只跑**一趟**。行为矩阵是最终裁判：24.12 从 25/25 掉到 14/25、再一条条收紧回
25/25；随后 8 个版本（8.17→26.10）合计 **200 pass / 0 fail / 2 skip**。

`JSCD_NO_OPT=1` 可关掉这轮优化，便于对拍。

### 11.9 AST 级优化：接 swc（terser 的 Rust 移植）

用户要求：产物里像 `r0 = greet; … r3 = r0("world")` 这种"只用一次的变量"该有**算法**消掉，
（"是不是有些 JS 优化工具能自动处理这种问题？有 Rust 实现更好"）。

**选型过程**：先试了 [oxc](https://oxc.rs)（Rust、快）。它的压缩器做常量折叠/DCE 很好，但
**没有通用复制传播** —— 源码里 `substitute_single_use_symbol_*` 的替换来源只有"**声明器
初始化器且值为编译期常量**"（`symbol_value` 来自 `evaluate_value`），而 V8 产物里到处是
`let r0; r0 = greet;` 这种"空声明 + 后赋值"，且 `greet`/`console` 这类标识符不是常量。
换 **swc**（`swc_core`，Next.js 生产在用）：`reduce_vars`/`collapse_vars` 就是 terser 那套，
正是要的算法。

**两个必须踩准的点（都真栽过）**：

1. **管线顺序**：`paren_remover` → `resolver` → `optimize` → `hygiene` → `fixer`。
   少 `paren_remover` 会**误编译**：压缩器把 `(a = console).log` 这类"括号保护"当成普通取属性，
   实测把 `a = console; b = a.log; a.log(1)` 改写成 `a = console.log; a.log(1)`（语义全错，
   运行时 TypeError）。swc 官方 `minify()` 就是这个顺序。
2. **包一层函数再优化**：压缩器只在**函数作用域**里做复制传播 —— 脚本顶层它把顶层绑定当
   全局对象属性（可能被别的脚本改），一律不碰。而产物在真实运行时**本来就是** `Module.wrap`
   的函数体 → 优化前包 `(function(){ … })();`，压完把壳剥掉（AST 层剥，不做文本手术）。
   顺带：`--runtime` 的前导是手写基础设施，**跳过** AST 层（压缩器只会帮倒忙）。

**取向是可读性而非最小体积**：`mangle: None`；`sequences: 0`（不合逗号表达式）；
`join_vars: false`（不合声明）；`arrows: false`（不转箭头函数）；`negate_iife: false`；
`keep_fnames/keep_classnames/keep_fargs/keep_infinity: true`（保名、保形参、别写 `1/0`）；
`reduce_vars: true`（**默认是 false**，不开就没有跨语句复制传播）；`pure_getters` 保持默认
（属性读可能触发 getter → 保守保留，实测 `a.b` 这种读不会被删）。

效果（用户那个例子）：

```js
// 之前（文本级优化后）
let r0, r2, r3;
r0 = greet;
r2 = console;
r3 = r0("world");
r2.log(r3);
// 现在（AST 级优化后）
let r2 = console;
let r3 = greet("world");
r2.log(r3);
```

`r2 = console` 留着是**正当保守**：中间的调用理论上能改全局 `console`，swc 不肯把全局引用
前送。解析失败 / 结构不认识 / 产物为空 → `opt_js::optimize` 返回 `None`，调用方保持文本级
结果（宁可少优化，不可改语义）。

### 11.10 文本层新增两条规则 + 一个既有 bug

⑤ **空声明 + 单次赋值 → 声明器初始化**：`let r0; … r0 = E;` → `let r0 = E;`。**原地**提升、
不搬运、不换序；条件是"该寄存器在这个函数区域里只有一处声明、只有这一次写，且这次写之前
没有任何出现"（否则原来读到 `undefined`，改完变 TDZ 报错），还要同块、是块内直接语句
（`if (c)` 这类无括号控制头后面不能直接跟 `let`）。**为的是喂 AST 层** —— swc 的复制传播
只认声明器初始化。

⑥ **方法加载中转删除**：`rX = A.B; … A.B(args)` 是 `CallProperty` 渲染的副产品 ——
调用那行已经渲染成 `A.B(args)`（带接收者、属性只读一次），这条加载成了"值为死、却让 getter
多跑一次"的残留。删它才与原始源码一致（原始源码里属性只读一次）。判据：这次装载的值到
下一次写该寄存器（或出了函数区域）之前没人读，且后面确实有同一个成员表达式的调用、中间
`A` 没被改写。

**链式前送的冲突（既有 bug，这次才暴露）**：`r0 = r4; r1 = r0; new r1(a0);` 里两条前送改写
互相抵消 —— 前一条删 `r0 = r4` 并把下一行改成 `r1 = r4`，后一条删 `r1 = r0` 并把 new 改成
`new r0`，结果 **r0 的值凭空消失**（twoclasses fixture 在 10.24.1 上就是这么挂的）。
修法：剪掉"目标本身会被删"的前送（保留来源）。

### 11.11 RegList 的精确个数（3+ 实参的调用/构造全错，既有 bug）

新 fixture `call_args3`（3 实参函数调用、3+ 实参方法调用、0 实参 `new`）一上来就红。

- **`CallProperty`（变参形态）**：tables 里所有版本都是 `(Reg, RegList, RegCount, Idx)` ——
  寄存器组**第一格是接收者**，其余是实参。旧代码按"接收者单独一格"解，`CallProperty r4, r5-r8`
  （= `r5.m(r6,r7,r8)`）被渲染成 `r4.call(r5-r8)` ✗（区间当成一个参数、`.call` 路径）。
- **`CallUndefinedReceiver`（变参形态）**：寄存器组**全是实参**（接收者是 undefined，不在表里），
  旧代码同样错。
- **`Construct`**：寄存器组就是实参（没有 new.target 在表里）；`count == 0` 时 V8 仍打印
  `r0-r0`，文本分不出"0 个"和"1 个" → `new O()` 凭空多一个实参。
  修法：`reglist_of()` —— **名字从文本取**（≤8.4 的参数寄存器在文本里是 `a0` 这种参数相对
  命名，用原始下标硬拼会得到 `a-7`，class_basic 在 8.17 上就是这么挂的），
  **个数用解析后的 RegCount**（只用来判 `count == 0`）。

### 11.12 类模板的散装键：`template_key` 漏了 ro-map

类方法名只存在于 boilerplate 的键里（`template_key`）。键可能是**只读堆字符串**
（单字符名 `m`、`a`、`v` 全在 RO 堆！），只有经 ro-map 才能解出 —— 而 `template_key` 用的是
自由函数 `name_of_ref`（拿不到表），于是这类键解不出、扫描停在那里，方法名全丢：
`DefineClass` 把方法挂成 `_anon_23`，`c.m is not a function`。修法与 `elem_key` 一致：
RoRef 先走 `ro_map`。

（顺带确认：单字符名在 RO 堆、2 字符起在普通堆 —— 所以 ro-map 覆盖单字符就够了。）

### 11.13 本轮的验证数字（全部在最终二进制上复跑）

- **行为矩阵**：**26 版本 × 26 fixture = 672 pass / 0 fail / 4 skip**（skip = `optional_chain`
  在 8.17.0 / 10.24.1 / 12.22.12 / 13.14.0 —— 源码语法要 node14+）。新增 fixture
  `call_args3`（3 实参函数调用、3+ 实参方法调用、0 实参 `new`）—— 之前 25 个 fixture
  **一条都没覆盖变参调用**，所以 §11.11 的 bug 一直没被矩阵发现。
- **语料**：408 份样本 × Node 8.17 / 12.22 / 16.20 / 20.20 / 24.12 / 26.10 →
  **0 语法错、0 反编译错、0 我们侧引用错**（`compile-fail` 格子是该版本解析器编译不了的
  样本源码：64/17/6/5/4/4，与产物无关）。
- `cargo test` 全绿（库 34 + CLI 14 + 探针 3）；`cargo clippy --all-targets` 零告警。
- 用户那个例子（`console.log(greet("world"))` 经 bytenode 编译）：
  默认输出 `let r2 = console; let r3 = greet("world"); r2.log(r3); function greet(a0){…}`。

### 11.14 第三轮：表达式重建（用户点名 `r2` 也该省掉）

用户看到的是两层跑完的
`let r2 = console; let r3 = greet("world"); r2.log(r3);`，问"r2 只用一次，似乎也可以省略"。
确实可以，但 swc 不肯：中间的调用（`greet("world")`）原则上能改全局 `console`，把全局读往后
搬不在它的安全模型里（terser 同款保守）。于是加第三条规则：

**⑦ 表达式重建**（`src/opt.rs`）：从调用行往前扫**同块内连续**的语句，把"装载进寄存器、
随后只读一次"的赋值并回调用（接收者 / 实参位置），删掉这些赋值：

```js
r2 = console;            console.log("sum", f(1, 2, 3));
r3 = "sum";
r4 = f;
r5 = 1; r6 = 2; r7 = 3;   →
r4 = r4(r5, r6, r7);     // 方法/函数调用、返回值写回同一寄存器（自引用）也照样重建
r2.log(r3, r4);
```

**为什么不换序**：整段一并搬进调用表达式，而 JS 求值顺序是"接收者 → 属性 → 实参从左到右"，
与字节码里这些寄存器赋值的先后**一致**；段内每句的相对次序因此不变。判据（遇到就停）：

- 同函数区域、同块；`starts_statement` 为真（无括号控制体两端都不行 ——
  `if (c) r0 = f();` 里 `f()` 可能不该执行）；
- 循环头不当并进目标（每轮重新求值）；
- 活跃性：**到下一次写这个寄存器之前**不能再有人读它 —— 包括那句写自己的"自引用读"
  （`r2 = new r2(a0)` 就是靠这条挡住的：`r2 = _anon_66;` 不能并走，否则 `new r2(…)` 拿到
  undefined，twoclasses 在 19–22 全挂过）；
- 值依赖的寄存器/标识符不能被段内更靠后的句子改写；
- 替换点形态：`new <reg>(…)` 的被调位置加括号（`new f(x)(a)` 会被解析成"先 new 再调用"，
  8.17 的 twoclasses 栽过）、`<reg> = …` / `<reg>++` 这类左值位置直接放弃；
- 已被别的调用吸收走的语句，只有当吸收它的调用也在本趟链上时才能跳过。

括号是保守加的（非原子表达式一律加），多余的由 swc 重新打印时清掉，所以文本层单独跑也不会乱。

效果（用户那个例子）：`console.log(greet("world"));` + 函数声明。args3 的顶层代码现在
和原始源码几乎一字不差（`console.log("sum", f(1, 2, 3)); console.log("sub", o.m(9, 4, 2)); …`）。

**回归**：26 版本 × 26 fixture = 672 pass / 0 fail / 4 skip；语料 408 份 × 多版本 0 语法/0 反编译错。

### 11.15 第四轮：逐份"人读"对拍发现的 8 个真 bug（全部已修）

用户要求：从测试用例里挑一批编译成 jsc、反编译，**自己读**原文与产物对比找问题（不用对拍工具）。
读了 class_basic / twoclasses / closure / generator / async_await / async_try / try_catch /
destructure / spread_rest / for_of_in / switch_case / template / string_regex / object_ops /
array_ops / operators / arrow_opt / yield_expr / branch / loop_for / strings / recursion /
call_args3，外加按形态构造的探针源码。结论：**矩阵全绿并不等于产物正确** —— 这一轮挖出的
8 个 bug 里没有一个被原有验证覆盖：

1. **优化层删掉整段代码**（默认输出）：swc 在 IIFE 里把"载荷内没人引用"的顶层声明当私有死代码，
   `function target` 与 `Counter = ctor` 直接消失。矩阵跑 `--runtime`（跳过 AST 层）正好绕过。
   修：不再包壳（swc 直接吃脚本，顶层声明它一律保留），并给 `behav_diff.js` 加**结构门禁**
   （未优化产物里的每个顶层函数声明，优化后必须还在）。
2. **调用被求值两次**：`r3[i] = f();` 后多一条 `f();` —— `StaInArrayLiteral` 渲染后漏了
   `acc_consumed()`（兄弟操作码都有），下一条指令的 flush 把同一表达式又发了一遍。
   纯函数 fixture 永远测不出来 → 新增 `call_once` fixture（模块级计数器 + 数组/对象字面量/
   算术三种形态），现在 26 个版本全过。
3. **`obj["c" + n]` 取到 undefined**（V8 14.6）：字符串拼接走 `Add_StringConstant_Internalize`
   特化指令，我们不认识 → 渲染成 TODO + `__runtime.Add_StringConstant_Internalize`。
   修：按 bytecodes.h 的语义（kReg 是 lhs、acc 读写）渲染成 `reg + acc`。
4. **`__uncompiled.NAME` 占位符换不掉**：`link_flat_functions` 收集"已定义名"时只认
   `function `，`async function` 顶格时收不到 → 占位永远留着，且元素是"成员读"不算纯，
   整张死的 DeclareGlobals 表也留下来了。修：认全 `async function* / async function /
   function* / class` 前缀。
5. **0 形参函数写成 `f(a0)`**：`function inc()`（闭包）、`static zero()` 都多一个形参 ——
   ScopeInfo 读出 0 时回退到 BCA 口径（含 `this`）。修：非老族回退时 −1。
6. **`r13 is not defined`**（修 5 之后的回归，被矩阵抓到）：phi 声明行插在**行首无缩进**处 →
   `regions()` 当成新的文件级声明 → 区域化的声明裁剪把整条 `let r0…r15;` 删了。
   修：phi 声明带缩进，且 `regions()` 忽略 `let phi…`。
7. **死 phi 噪声**：每条 `if` 都带 `phiN = <条件值>;` 与 `let phiN;`，而且挡着 ⑦ 的扫描。
   修：规则⑧（没人读的 phi → 删赋值，右值含调用/成员读就留成表达式语句；声明里摘名）。
8. ⑦ 不再把控制流头（`if (…)`/`while (…)`）当并进目标（对象字面量并进条件既难读、
   又踩条件求值次序）。

**读到但未改的（语义正确、可读性欠佳，按影响排序）**：
① for-of / 解构 / 展开仍是**迭代器协议展开**（destructure、for_of_in、generator 的产物
30–60 行 vs 原 10 行）—— 最值得做的下一个特性（把 `[Symbol.iterator]()/next()/.done/.value`
+ try/finally 协议折回 `for (const v of xs)` / `const [a, b] = …`）；
② 类方法名没利用 boilerplate 的键（`_anon_24` 本该叫 `bump`）；
③ swc 会把语句合成逗号序列（`return a.bump(1), x + a.value;`，`sequences: 0` 对该形态无效）；
④ `void 0` 而非 `undefined`、`throw RangeError(…)` 丢 `new`（swc 对 Error 家族的等价转换）；
⑤ 数组"占位 + 逐个赋值"（`r4 = [0,0,0]; r4[0] = f(); …`）可折成字面量。

**这一轮的验证**：26 版本 × 27 fixture = **698 pass / 0 fail / 4 skip**（含结构门禁）；
语料 408 份 × 8.17/20.20 两版 0 语法/0 反编译错；`cargo test` 39+14+3 全绿；clippy 零告警。

### 11.16 第五轮：5 个子 agent 分片"人读对拍"（≈40 组 fixture/版本 + 样本）

用户要求：再挑一批编译→反编译，**自己读**原文与产物对比，并允许派子 agent 多覆盖。
做法：3 个 Node 版本（8.17 / 16.20 / 26.10）× 27 fixture，每组出"默认产物 + `--runtime` 文本层产物"
两份；再加 9 份真实样本 × 2 版本。分 5 片派给子 agent 精读，**结论我一律自己复核**（下面标注）。

**复核为真并已修（2 条）**

1. **`?.()` 短路分支里多出一次调用** ✗✗（3 个 agent 独立撞到）。`obj?.raw?.toString?.()` 在
   `toString` 为 nullish 时，产物 `else { r6.toString(); phi7 = void 0; }` 仍调了一次 →
   必然 TypeError。根因：**if/else 发射时 else 路径继承了 then 分支跑完的 acc**，于是 then 里
   pending 的调用被当"待求值"在 else 又发一遍。修：进 else 前把 acc/acc_stored 恢复成**分支前**
   状态（V8 里 acc 跨跳转保留，then 的求值不发生在 else 路径上）。
2. **参数槽的写回落到了死临时** ✗（`try { … } finally { x = x + 100 }`，x 是形参）。
   字节码是 `Star [Reg(-9)]`（参数槽用**负下标**），而写侧按文本 `a0` 解析成下标 0 → `r0 = a0 + 100`，
   AST 优化层再把它当死代码删掉 → **整句消失**。修：`aN` 形态的参数槽**按名字写**（`store_named`）。

**复核为真、尚未修（进 known-fail，有复现）**

3. **try/finally 完成码机制两个缺口** ✗✗（我扩展 fixture 时撞出来的，node 16/26 同样错）：
   ① `try { return v; } finally { … }`（**无 catch**）→ finally 主体被塞进凭空的 `catch (e)`、
   完成码 `switch (r0)` 机器泄漏、`return` 丢失；
   ② finally 之前算出的值要在 finally 之后用 → 正常路径返回 `undefined`。
   已固化成 `tests/fixtures/behav/finally_return.js`，cases.json 标 `known_fail`；矩阵把它单列
   （不会污染 pass/fail 信号），修好前一直红着。
4. **数组解构默认值丢分支** ✗✗：`[a, b = 9] = [x, undefined]` → 产物 `if (r16 === undefined) { }`
   + 无条件 `r1 = 9;`（值那条边整段丢失）。字节码 `Ldar r16 / JumpIfNotUndefined / LdaSmi 9 / Star1`；
   发射轨迹（`JSCD_DBG_IF`）显示走了 if/else 分支、then 的 acc-only 指令被吞、acc 泄漏到续行。
5. **for-of / 解构收尾的 `ReThrow` 是死守卫** ✗✗（3 个 agent）：产物 `if (__ctx.ctx0 !== undefined) throw __ctx.ctx0;`
   而 `ctx0` 全文只读不写 → 循环体/委托迭代器抛异常时被**吞掉**、函数继续返回残值。
   正解是抛"当前挂起异常"（catch 参数/寄存器里那个）。
6. `JumpIfJSReceiver` 渲染成 `x !== undefined` ✗（应判"是不是对象"，null/原始值会漏检；窄触发）。
7. 对象展开渲染成 `Object.assign({}, src)` ✗（自有 `__proto__` 键语义与原生的 `{...src}` 不同）。

**复核后判为"不是 bug"**：标签模板实参在默认产物里少一个（子 agent 报 SEMANTIC）——
被调 `(s) => s.toUpperCase()` 根本不看第 2 个实参，实参本身又是纯变量读，swc 丢掉它是**语义等价**的
（agent 的验证方式是把被调函数换掉，那等于改了程序）。同理 `new Error()` → `Error()` 对 Error 家族等价。

**已知设计限制（非新 bug）**：闭包捕获变量摊平成文件级 `var`（共享绑定；重入/交错调用会串）、
重复名去重后默认产物没有 `__v8name` 别名行（静态方法按名兜底需要它，默认产物本来不带运行时）、
swc 的语句→逗号序列合并与效果重排（`calls += 1` 被挪到 `2 * a0` 之后，数值入参不可见、抛异常的
入参可见）。

**验证**：26 版本 × 28 fixture = **698 pass / 0 fail / 4 skip / 26 known-fail**（后者就是
`finally_return` 在 26 条线上的已知未修）；语料 408 份 × 20.20/26.10 0 语法/0 反编译错；
`cargo test` 39+14+3 全绿；clippy 零告警。

### 11.17 第六轮：子 agent 扩测 → 逐条复核（try/finally 全版本、数字字面量、上下文槽名）

用户要求"扩大测试例子、用子 agent 并行测试、AI 自己读产物对拍"。四个分片（3 版本 × 全 fixture、
4 版本 × 全 fixture、真实语料 15 样本 × 2 版本、3 版本深度潜在问题）共读了 ≈200 组产物；
**每条结论都在当前二进制上重新复现过**（子 agent 的材料常比修复滞后，报告里的 high 过半是旧状态）。

**修好的（每条都有 fixture 或探针钉住）**

1. **无 catch 的 `try{…}finally{…}` 全版本打通**（26.10 比较链形态 + 23.11"恢复对在 case 分支里"）。
   检测改成双形态：switch（≤20）与比较链（26），`SetPendingMessage` 在白名单里（判别子仍是
   `CallProperty`）。`finally_solo` 26 条线全绿。
2. **`try/catch` 之后的代码被吞进 catch** ✗✗：函数里只有这一个 handler 时（V8 不发完成码），
   `catch_end` 的"下一个 handler"策略退化成函数尾 → `try{out=x}catch{out=-1} return out;` 的成功路径
   返回 undefined（16/20/26 全中）。补结构判据：try 体末尾那条 `Jump` 的目标就是续接点。
   连带把**最后一个 known-fail**（`finally_return`，catch+finally 里 finally 之前的值）修绿 —— 根因同源
   （区间划分 + 下面第 4 条的死存储误删）。
3. **双精度数组字面量**：FixedDoubleArray 的元素是**原始 f64**，而 Smi 解码的判定
   （高半为零）与"低半为零的 double"（1.5、-0、0.5…）撞车 → `[1.5,-0,2.25]` 变成
   `[1073217536,-2147483648,1073872896]`。按 holder 的 map 名走 8 字节 f64 解码
   （偏移 `2*ts + i*8`，压缩/非压缩通用）；`Expr::Num` 补 `-0`。
4. **BigInt 字面量**：`BigIntMap` 不在 `ty.is("BigInt")` 的匹配里（落进未知常量 → `undefined`）；
   `bigint_value` 把 bit_field 当 Smi 读高半（恒 0）。按 `bigint.h` 重写：`SignBits = bit0`、
   长度从 bit1 起、digit 从 `2*ts` 起每 ts 字节、digit 存**绝对值**（`FromInt64` 先取 absolute）。
   **语料扫描又抓出一处**：4 位 digit（256 位）的值塞进 `i128` 组装时移位按 128 取模、最高位落到
   符号位 → 正数解成负数、渲染出 `--5981482…n`（语法错）。改成反复除 1e9 的十进制大数转换
   （`arithmetic-66c885` 的 `0x761f…n` 现在与源码 hex 逐位相等；语料 596 份 0 语法错）。
5. **脚本没有任何声明时顶层代码丢失**：`console.log("hi")` 这类脚本没有 `DeclareGlobals`，
   于是整个脚本被当匿名函数输出成没人调用的 `function _anon_0()`。载荷根 SFI 就是脚本顶层 →
   纳入 `inline_body` 判据。新增**整脚本对拍器** `scripts/script_diff.js`（跑原脚本与产物比 stdout/退出码）
   + `tests/fixtures/scripts/` 5 个用例（只有语句 / 只有 const / var / class / async），并入矩阵统计。
6. **文本层：跨分支的"死存储"误删** ✗✗：`try { r0 = x } catch { r0 = -1 } return r0` 里，
   try 里的赋值被 catch 里的同寄存器写当成"已被覆盖"删掉（线性模型对互斥分支无效）。
   按**块路径签名**比较（`try {` 与 `} catch (e) {` 是不同块），并把 `next` 状态改成按 (区域, 寄存器) 键。
7. **phi 预初始化多余求值**：`phi0 = a0 instanceof Object;` 在两条分支都无条件赋值时是多余的，
   且 `instanceof` 会调 `Symbol.hasInstance`（可观察的双求值）。加"安全省略"判据（分支顶层、
   赋值不看旧值）。
8. **旧操作码名**：`LdaNamedPropertyNoFeedback` / `StaNamedPropertyNoFeedback`（7.7–9.x）没接 →
   `console.log(...)` 变成 `__runtime.LdaNamedPropertyNoFeedback.call(...)` 的桩调用（node12/13 脚本一行都不输出）；
   `CallRuntime [CreateArrayLiteralWithoutAllocationSite]`（7.8/7.9 的字面量）被当普通运行时调用包住。
9. **上下文槽名（本轮最大的一条）**：
   - `V8 14 的 *NoCell` 被误映射成 `*ScriptContextSlot`（脚本上下文，槽基准 +1）——
     实际是**普通**上下文槽的 V8 14 拼写（头文件里与 `LdaContextSlot` 并存、形状相同）→ 整体错一槽；
   - `LdaContextSlot rN, [slot], [depth]` 只看最内层作用域 → 不同 block context 的**同号槽**串名
     （`for (const ctor of …) { const rab = … }`：`ctor.BYTES_PER_ELEMENT` 读成 `rab.BYTES_PER_ELEMENT`，
     `new (rab = CreateResizableArrayBuffer(...))(rab,…)` → TypeError）；
   - 实现**上下文寄存器跟踪**：`Create*Context` 记 pending、`Star/Mov` 落成 `reg_scope[rN]`、
     `PushContext rN` 存的是**旧上下文**（interpreter-generator.cc 核对）→ 登记上一个作用域；
     显式读取按 `reg_scope + depth` 沿外层链解析（**跳过没有 context 的作用域**——闭包自己的
     ScopeInfo 常常 context_locals 为空）；`<context>` 操作数从当前作用域起走 depth 跳；
     形参拷贝槽优先用 `aK`（async/生成器序言把形参搬进上下文，摊平产物里形参是 `aK` 不是源码名）。
10. **签名 arity（老族 `.length`）**：≤8.4 的 BCA 形参口径含 `this`（+1），而 ScopeInfo 只数形参。
    两口径**交叉验证一致**（`scope.param_count + 1 == bca_param_count()`）时才用 ScopeInfo ——
    8.17 的 `function bump(a0,a1)` → `bump(a0)`、getter/0 参函数 → `()`。
    无脑 −1 曾让老族寄存器名算成 `a-2`（node14 产物语法错），所以必须带这个证明。
11. **运行时桩补缺**：`eval_`（非直接 eval 的调用点，不声明会 ReferenceError，
    SyntaxError 类测试永远看不到真错）、`DisposableStack` 系列（`using` 的释放语义）。

**已知未修（已固化成 known-fail / 记录在案）**

- `iter_close`（known_fail，26 条线）：for-of **IteratorClose 收尾吞异常** —— 机器 handler 区被线性发射
  （没折进 catch），重抛渲染成引用 catch context 槽 0 的守卫（那个槽在这些形态里没人写）→
  循环体抛出的异常不传播、函数返回残值。正解是折 handler + 接异常寄存器（试过"直接抛累加器"：
  线性残留块会让**正常路径**抛垃圾，比原来更糟，故回退）。
- 函数参数默认值族（生成器/async）：默认值被内联进体（求值时机从"调用时"挪到"开始执行时"，
  生成器里抛错的默认值不再同步抛）、`x = y` 这种默认值整段丢、`.length` 计错。
- 数组解构默认值在 16.20 上退化成无条件赋值（fixture 的 RHS 恒 undefined 掩盖了它）。
- `optional_chain` 里 `toString` 被读两次（死读 + 真调用）。
- 类形状（方法可枚举、可无 new 调用、箭头变普通函数）、rest 用 `[].slice.call(arguments)`、
  模板对象的 `SetCode` 桩、8.17 for-of 正常结束多调一次 `iterator.return()`。

**这一轮的验证**：26 版本 × 33 fixture = **827 pass / 0 fail / 5 skip / 26 known-fail**
（后者就是 `iter_close` 在 26 条线上）+ 5 个整脚本用例 × 26 版本 = **130 pass / 0 fail**；
`cargo test`、clippy 全绿。

### 11.18 第七轮：把最后两个 known-fail 清掉（IteratorClose 收尾 + 参数默认值）

用户要求"继续，彻底修复"。这一轮把上一轮钉住的两条 known-fail 都修了，并顺带修掉扩测发现的
若干条（含一条 6.x 侧的 `.length`/async 缺口）。

**1. for-of 的 IteratorClose 收尾（`iter_close` 由 known_fail 转绿，26 条线全过）**

根因有两层，都在"无 catch 的 try/finally 形态"的检测上：

- **完成码分派的锚点**：收尾协议 handler 里也有形态相同的 `TestReferenceEqual <码寄存器>`
  （`JumpIfTrue <跳过重抛>`），早先的扫描把它当候选、一看下一跳不是 `JumpIfFalse` 就
  **整体放弃** → 外层交给 ① 规则当普通 catch → 异常被吞、函数返回残值。
  现在按"**恢复挂起消息**"锚定：分派比较的前三条指令里有 `SetPendingMessage`
  （9.4–22.x 版式），**或**它的负分支里有 `SetPendingMessage`（V8 14 版式）才算分派；
  两条都不满足就继续往后找（不再放弃）。分派一旦认出，整段机器重抛**丢弃** ——
  JS 的 `try/finally` 天然会把异常传下去，根本不需要渲染机器重抛。
- **6.x 三处**：`TestEqualStrictNoFeedback` 根本没实现（落成桩调用 → 分派永远认不出，
  补上别名与 `===` 渲染）；catch+finally 路径在"找不到完成码设置"时直接放弃
  （6.x 的 handler 配对会挑中内层 try/catch）→ 改为**回退试无 catch 形态**（四个失败点）。

**2. `try/catch` 之后还有代码（`try_catch_after`）**：函数里只有一个 handler 时
`catch_end` 退化成函数尾、把后续语句吞进 catch（成功路径返回 undefined）；
续接点的结构性锚点是 try 体末尾那条 `Jump` 的目标 —— 现在**始终参与取 min**
（顺带修掉"两个顺序 try/catch"：第一个 catch 的右端曾截到**第二个 catch 处理器**，
第二条 try 被吞进第一条的 catch）。

**3. catch 体尾部的 acc 落地**：`try { A } catch { out.push("E1") } try { B }` 里
catch 最后那个调用被当成待求值带出 catch、在**下一条语句的位置**无条件执行 ——
catch 体结束前补 `materialize_acc_stmt()`。

**4. 参数默认值搬进签名（`param_defaults` 新 fixture，11 版本探针逐字相等）**

V8 把 `function f(a, b = 2)` 的默认值编译在 prologue（`Ldar aK; JumpIfNotUndefined <else>;
<默认值…>; Jump <join>; <else: Ldar aK>; <join: Star rV>`，6.2→14.x 形态一致；6.x 的
生成器/async 走 `Lda*CurrentContextSlot [S]` 变体）。早先把它内联成体内三元式，两个语义偏差：

- 求值时机：源码在**调用时**（参数绑定阶段）求值 —— 生成器/async 生成器也一样
  （体要等第一次 `next()`）→ 抛错的默认值不再同步抛；
- `.length`：形参表不带默认值 → 多算。

做法：识别整段机器码 → 用**试渲染**（把该段发射到临时缓冲、取 acc 当表达式；纯寄存器临时
赋值按名替换；结果里不许残留未解析的 `rN`/`tN`）→ 生成签名默认值 `a1 = <expr>` →
原 prologue 整段跳过，只留一条 `rV = aK;` 的寄存器回写（`__pregs`）。

配套细节：`plan_param_slots`（形参下落槽 `Ldar aK; StaContextSlot [S]` → 槽 S 就是参数 K，
与机器形态/版本无关；生成器/async 的计划里那两处因条件不同会漏，6.2 async 的形参拷贝槽
因此曾被按脚本作用域解名、**覆盖脚本级变量**）；prologue 里 `Mov aK, rN` 与
`Lda…Slot [S]; Star rN` 的拷贝预置成参数名（生成器/async 的 prologue 会被计划整段丢弃）；
普通 async 函数**也**搬（原生语义：async 的形参绑定在 async 机制内部，抛错 → rejected promise）。

**5. 6.x async 的两处缺口**

- **没有 `await` 的 async 函数被判成普通函数**（调用返回 undefined 而不是 promise）：
  6.2 的 ScopeInfo `function_kind` 恒 0、也没有 `Await` 操作码。判据补齐：
  `looks_async_6x()`（具名结算调用 / `CreateJSGeneratorObject` 后的数字名调用 /
  尾部 Switch 的数字名结算）+ plan_async 的"无 await"出口也把函数标记成 async。
- **catch 体里的数字名结算**（`RejectPromise(state, err)`）渲染成 `throw err;` ——
  JS 的 `async` 关键字把它变成 reject（否则落在未实现的桩上、体里的异常被吞、promise 反而 resolve）。

**6. `TestTypeOf` 的 flag 枚举随版本差一位**：BigInt 支持（V8 6.7）往枚举里插了 `kIsBigInt`，
`undefined/function/object` 从 4/5/6 挪到 5/6/7（用"`typeof x === "…"`"探针**逐版本实测**钉的）。
认错位会把"是不是函数"判成"是不是 undefined" → 8.17 的收尾协议对着活函数抛假 TypeError。

**7. 上下文槽名（三条收尾）**：`slot < 基准` 时 `saturating_sub` 会索引到作用域第一个变量
（catch context 的槽 0 = 抛出对象 → 22.12/24.12 的收尾守卫写成 `if (returnCalls !== undefined)
throw returnCalls`，凭空抛一个数字）→ 两处加下界检查；**写侧**也用精确解析
（闭包里的 `const real = it.return` 曾写成脚本级 `orig = …`，嵌套函数读 `real` 是 undefined）；
`Mov <context>, rN` 不能被当成"寄存器 rN"去传播作用域（`reg_of("<context>")` 落到 0）。

**8. 常量池两处**：`EmptyArrayBoilerplateDescription` 根 → `[]`（`const out = []` 曾变 undefined
→ `out.push` TypeError）；6.x 的空数组 `Tuple2{kind, EmptyFixedArray}`（内层是**根**不是对象）
→ `[]`（`[...gen()]` 靠它先建空数组）。

**9. 空数组/`instanceof` 之外的杂项**：`is_generator`/`is_async` **不再**用 ScopeInfo 的
function_kind 兜底（版本间不一致：箭头函数被判成生成器 → 产物返回 `[object Generator]`，
26 条线的 `arrow_opt`/`class_basic` 全红）；BigInt 的十进制化改大数转换（4 位 digit 塞 i128
会移位取模、正数变负 → `--5981482…n` 语法错）。

**这一轮的验证**

- 26 版本 × 36 fixture：见文末矩阵；三个新 fixture（`param_defaults`、`typeof_flags`、
  `iter_no_extra_close`）+ 两条旧 known-fail 转绿。
- 探针（不进矩阵，逐版本与原脚本逐字对拍）：参数默认值 11 版本全等、for-of 收尾 7 版本全等、
  `typeof` 10 项对拍 5 版本全等。
- `iter_no_extra_close` 钉住 6.x **唯一**剩余缺口：正常结束多调一次 `iterator.return()`
  （6.2/6.8 把循环体的完成码 continue=2 当成"需要 close"；7.8+ 语义不同、产物正确）。

**收尾（同轮追加）：最后那条"known-fail"其实是 V8 自身的版本差异，不是产物缺陷**

`iter_no_extra_close` 的判别形态是"末元素触发 `continue` 后迭代器耗尽"：
`for (const v of [1,1,2]) { if (v === 2) continue; }`。用**同版本**的 node 跑原函数实测：

- 6.2 / 6.8：耗尽时**会**调一次 `iterator.return()`（`calls=1`）；
- 7.8+（16.20 起）：不调（`calls=0`）。

产物两边都**忠实复现**（同版本对拍逐字相等）—— 早先矩阵报"差异"是因为对拍脚本用**宿主 V8**
（新）跑原函数、却拿它跟 6.2 字节码重建的产物比，属于**苹果比橘子**。

因此：给对拍脚本加了 `same_node_original` 开关（`cases.json` 里按 fixture 声明）——
勾了它的 fixture，原函数改用**目标版本的 node** 跑（spawn 一次，`-e` 里打印 `__JSCD__` 前缀的
JSON 结果）。`iter_no_extra_close` 现在两条语义都钉住、26 条线全绿。**known-fail 归零。**

### 11.19 第八轮：把 7.5–8.4 的三条经典线拉进矩阵（31 条线全绿）

README 里"数字体现质量"的要求（用户）顺带暴露了两处**测试材料**与一处**真 bug**：

1. **真 bug（V8 7.5/7.6）**：对象解构的 ToObject 守卫写成
   `Ldar X; JumpIfNull <cold>; JumpIfNotUndefined <after>` —— **第一条跳转后没有重复 `Ldar`**
   （acc 没被破坏）。`try_emit_null_guard` 只认"两跳之间夹 `Ldar`"的 6.x 版式 → 漏 →
   冷块（`CallRuntime [ThrowPatternAssignmentNonCoercible]`）被当**线性代码**发射、
   函数**必然抛错**（12.5/12.9 的 `{x, ...rest} = obj` 就是这样挂的）。
   另外那条冷块**没有显式 `Throw`**（运行时自己抛、不返回）→ `block_terminates` 也太严，
   放宽为"单条 `CallRuntime` 且名字以 `Throw` 开头或含 `NonCoercible`"。
2. **测试材料（ro-map 覆盖）**：12.5/12.9/12.15/14.0/14.6 五条经典线此前没有 ro-map
   （或列表太窄，`","` `":"` `.then` 一类名字没进表）→ 产物里出现 `<ro0_25784>` 这种占位符
   （`join("<ro0_25784>")`），看着像语义 bug、其实是只读堆名表缺名字。
   现在用**超集列表**（所有现存 map 的取值 + fixture 名单 + 语料标识符，12586 条）统一生成。
3. **矩阵扩到 31 条线**：12.5.0、12.9.0（V8 7.5/7.6）与 12.15.0、14.0.0、14.6.0
   （V8 7.7 / 8.1 / 8.4.371.19）由"只能跑单文件"变成**全绿**。
   尚未移植的仍是：V8 5.8 / 6.0–6.1 / 6.6–6.7 / 7.0 / 7.4 的 payload 族
   （8.0–8.9 / 10.0–10.8 / 11.x / 12.0 的部分发布）：`jscd info` 能识别（或明确报
   `unsupported`），payload 反序列化会**显式报错**而不是输出垃圾。
