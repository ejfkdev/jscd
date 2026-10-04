# jscd

把 [bytenode](https://github.com/bytenode/bytenode) 编译的 `.jsc` 还原成 JavaScript。

`.jsc` 是 V8 的代码缓存（`v8::ScriptCompiler::CreateCodeCache`）。`jscd` 纯静态解析：纯 Rust、
单二进制、不需要打过补丁的 V8、不需要 Node 运行时、不联网。

[English](README.md)（默认） · **中文**（本文件） · [逐版本工程笔记](docs/VERSIONS.md)

[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Rust 1.96+](https://img.shields.io/badge/rust-1.96%2B-orange.svg)](Cargo.toml)
[![Node 8.0 – 26.10](https://img.shields.io/badge/Node-8.0%20%E2%80%93%2026.10-green.svg)](#支持的版本)
[![V8 5.8 – 14.6](https://img.shields.io/badge/V8-5.8%20%E2%80%93%2014.6-green.svg)](#支持的版本)
[![release](https://github.com/ejfkdev/jscd/actions/workflows/release.yml/badge.svg)](https://github.com/ejfkdev/jscd/actions/workflows/release.yml)

**目录：**[快速上手](#快速上手) · [安装](#安装) · [用法](#用法) · [支持的版本](#支持的版本) ·
[实现](#实现) · [验证](#验证) · [仓库结构](#仓库结构) · [已知限制](#已知限制) ·
[参与贡献](CONTRIBUTING.md)

## 快速上手

```console
$ cat hello.js
function greet(name) {
  return "hello " + name;
}
console.log(greet("world"));

$ npm i -g bytenode && bytenode -c hello.js      # 本文用 Node 20.20.2
$ xxd -l 48 hello.jsc
00000000: cc05 dec0 0bc2 e400 9200 0000 e521 2eaa  .............!..
00000010: f002 0000 0000 0000 011c 5401 2006 a860  ..........T. ..`
00000020: 0000 0000 0600 0000 010c 4c60 0000 0000  ..........L`....

$ jscd hello.jsc
console.log(greet("world"));
function greet(a0) {
    return "hello " + a0;
}

$ jscd hello.jsc --runtime > hello.out.js && node hello.out.js
hello world
```

开头四个字节是小端的 code cache magic（`cc 05 de c0` 即 `0xc0de05cc`）；其余头部字段用
`jscd info hello.jsc` 能一并解出（样例见[用法](#用法)）。

上面的产物是**两层优化跑完**的样子。想看没优化的原样（V8 真实的寄存器搬运，一条一条），加
`JSCD_NO_OPT=1`。

Node 22+（V8 12.4+）上有些内建属性名存在只读堆里。行为矩阵覆盖到的 29 个 V8 minor，`jscd` 都
内置了名表（`tables/ro_map_*`）、自动还原 —— 但**只在 macOS 上**：只读堆的编号跟平台走，换系统
就得自己建（`jscd ro-map` 生成后 `--ro-map` 传入；`JSCD_NO_RO_MAP=1` 可关掉内嵌表）。
没有名表时这类名字落成 `<ro0_…>` 占位，绝不会给错名字。

## 安装

每个 tag 的 Release 里带有 Linux / macOS / Windows 的预编译产物（x64 与 arm64）。从源码装：

```sh
git clone https://github.com/ejfkdev/jscd && cd jscd
cargo build --release        # -> target/release/jscd
cargo install --path .       # 可选：装进 ~/.cargo/bin
```

需要 Rust 1.96+（swc 优化器要求较新的 rustc），无系统依赖。

<details>
<summary>发版（维护者）</summary>

`scripts/release.sh vX.Y.Z` 先过发版门禁（`cargo test --release`、
`clippy --all-targets -- -D warnings`，以及端到端冒烟 `scripts/ci_smoke.sh` —— fixture 真编译成
`.jsc` → 反编译 → 语法门禁 + 整脚本行为对拍），再推一个**带说明的 annotated tag**；tag 说明就是
GitHub Release 的描述。tag 一推即触发 `.github/workflows/release.yml`，在上面那六个平台重建裸
二进制。

</details>

## 用法

```sh
jscd app.jsc                     # 单文件反编译到 stdout
jscd app.jsc app.js              # ……写到文件
jscd dist/                       # dist/ 下所有 .jsc → dist-out/（保持层级）
jscd dist/ out/                  # ……改写到 out/
jscd info app.jsc                # 头部字段与识别出的 Node/V8 版本
jscd disasm --filter main app.jsc
jscd ro-map probe.jsc > m.json   # 生成只读堆名表
jscd --help                      # 双语帮助（-h、`help`、`help <子命令>`）
```

- **输入**是一个 `.jsc` 文件，或目录（递归找 `*.jsc`）。**输出**默认：文件输入 → stdout；
  目录输入 → 输入同级的 `<输入名>-out/`（保持层级）。`-o` / 位置参数 OUTPUT 可指定文件或目录，
  `-` 表示 stdout。
- V8 版本超出当前表范围时**直接报错**，报错里点名识别到的 V8 版本与当前支持范围，而不是静默吐一份
  只有运行时前导的文件；`jscd info` 的 `supported: yes|no` 也是为这件事准备的。
- 只读堆里的属性名（Node 22+）对 `tables/ro_map_*` 里收录的构建**自动解析** —— 只在 **macOS**
  上（那批表就是在 macOS 上提取的；只读堆编号跟平台走，只有信得过的表才自动套用）。换平台就用
  `scripts/build_ro_map.sh` / `jscd ro-map` 自己生成后 `--ro-map` 指定，`JSCD_NO_RO_MAP=1`
  可关掉内嵌表。**不匹配的表会被忽略**（宁可留 `<ro0_…>` 占位也不给错名字）。
- 命令行信息跟随语言：按 `JSCD_LANG`（→ `LC_ALL` → `LC_MESSAGES` → `LANGUAGE` → `LANG` →
  `LC_CTYPE`）识别——`zh*` 选中文、其余英文，`JSCD_LANG=zh|en` 可强制。

| 旗标 | 适用 | 作用 |
| --- | --- | --- |
| `-o, --output <路径>` | 全部形态 | 输出文件 / 目录 / `-`（与位置参数 OUTPUT 等价） |
| `--quiet` | 目录输入 | 不打印逐文件进度 |
| `--json` | 全部子命令 | 机器可读输出 |
| `--filter <子串>` | `disasm` | 只输出名字含该子串的函数 |
| `--ro-map <路径>` | `decompile` | 只读堆名表，由 `jscd ro-map` 生成 |
| `--verify` | `decompile` | 语法门禁：产物过一遍 `node --check`（需要 PATH 里有 `node`） |
| `--runtime` | `decompile` | 保留可运行前导（`__runtime` 占位实现 + 上下文变量提升 + 别名块）。**默认不带**；想让产物能 `node` 跑就加上 |
| `-h, --help` | 全部形态 | 打印帮助（`jscd help <子命令>` 看单个命令） |
| `-v, -V, --version` | 全部形态 | 打印名字、版本与仓库地址 |

默认输出**只有还原出来的代码本身**：没有运行时前导，也没有注释（连结构性分隔注释也会被 AST 层
清掉；`JSCD_NO_OPT=1` 才保留）。加 `--runtime` 才会补上 `__runtime` 代理（字节码调到的 V8 内建给
最小实现）、被摊平的上下文变量声明和名字别名块，这样产物可以直接用 `node` 运行。

<details>
<summary><code>jscd info</code> 输出样例</summary>

```console
$ jscd info hello.jsc
file:          hello.jsc
size:          776 bytes (raw) / 776 bytes (decompressed)
brotli:        no
magic:         0xc0de05cc (external refs: 0x5cc)
version_hash:  0x00e4c20b
source_hash:   0x00000092 (source length: 146 bytes, module: no)
flag_hash:     0xaa2e21e5
ro_checksum:   (absent)
payload:       752 bytes at offset 24
checksum:      0x00000000
node:          v20.20.2
v8:            11.3.244.8 (exact)
supported:     yes
```

</details>

### 两层优化

`JSCD_NO_OPT=1` 把两层都关掉，便于和原样对拍。

1. **文本层**（`src/opt.rs`）：删字节码特有的寄存器搬运 —— 只读一次的变量并进使用处、之后先被重写
   才被读的存储、没人再用的寄存器声明、`DeclareGlobals` 这类纯簿记；把"空声明 + 单次赋值"并成
   声明器初始化；删掉 `CallProperty` 渲染副产品的"方法加载中转"（`rX = A.B;` —— 调用那行已经把
   属性读搬到了调用处，留着会让 getter 多跑一次）；并且做**表达式重建** —— V8 会把接收者和每个
   实参都先算进寄存器，这一趟把"只读一次"的装载并回调用点，按 JS 的求值顺序（接收者 → 属性 →
   实参从左到右）摆放，所以不换序。判不准的一律不并：跨块、循环头、无括号控制体
   （`if (c) r0 = f();` 里 `f()` 可能不该执行）、以及"之后还要用这个值"（含 `r2 = new r2(a0)`
   这种自引用写）。
2. **AST 级**（`src/opt_js.rs`）：把文本交给 [swc](https://swc.rs)（terser 的 Rust 移植）做
   **复制传播**、无用变量删除与常量折叠：

   ```js
   let r0, r1, r2, r3;        // 反编译原样             // 两层跑完
   r0 = greet;                console.log(greet("world"));
   r2 = console;       →      function greet(a0) {
   r1 = r2.log;                 return "hello " + a0;
   r3 = r0("world");          }
   r2.log(r3);
   ```

   管线与取向都是校准过的：`paren_remover → resolver → optimize → hygiene → fixer`
   （少 `paren_remover` 会误编译），优化前包一层函数（swc 只在函数作用域做复制传播，而产物在真实
   运行时本来就是 `Module.wrap` 的函数体），压完剥壳；不改名、不合语句、不转箭头函数、保函数名。
   产物不是合法 JS 或解析失败 → 保持文本级结果。

## 支持的版本

版本表由 `scripts/codegen.py` 从 Node 源码生成、编译期嵌入；识别按 V8 版本哈希走。8.0.0 起的每个
Node 发布都在 `tables/manifest.json` 里（510 条），每个 V8 minor 一张表 —— 内容相同的表只存一份。

**8.0.0 到 26.10.0 的每个 Node 发布都能识别**（510 个发布、90 个 V8 版本），其中 **36 个 V8 minor
里的 29 个跑完整行为矩阵**（31 条 Node 线全绿：1,108 pass / 0 fail / 8 skip / 0 known-fail）。
剩下 7 个 minor（5.8、6.0、6.1、6.6、6.7、7.0、7.4）`jscd info` 能识别，但 payload 反序列化
**显式报错**，不会输出垃圾。

| V8 minor | Node 线 | 结果 |
| --- | --- | --- |
| 6.2 → 14.6，**36 个里的 29 个** | 31 条线，8.17 → 26.10 | **全绿** —— 1,108 pass / 0 fail / 8 skip |
| 5.8、6.0、6.1、6.6、6.7、7.0、7.4 | 8.0–8.9、10.0–10.8、11.x、12.0 的部分发布 | 能识别；payload **显式拒绝** |

<details>
<summary>完整映射：36 张版本表 → 510 个 Node 发布</summary>

| Node 发布 | V8（该线最新） | 表 |
| --- | --- | --- |
| 8.0.0–8.2.1 | 5.8.283.41 | `tables/v5_8.json` |
| 8.3.0–8.6.0 | 6.0.287.53 | `tables/v6_0_8.6.0.json` |
| 8.7.0–8.9.4 | 6.1.534.50 | `tables/v6_1_8.9.4.json` |
| 8.10.0–9.11.2 | 6.2.414.46 | `tables/v6_2.json` |
| 10.0.0–10.3.0 | 6.6.346.32 | `tables/v6_6_10.3.0.json` |
| 10.4.0–10.8.0 | 6.7.288.49 | `tables/v6_7_10.8.0.json` |
| 10.9.0–10.24.1 | 6.8.275.32 | `tables/v6_8.json` |
| 11.0.0–11.15.0 | 7.0.276.38 | `tables/v7_0_11.15.0.json` |
| 12.0.0–12.4.0 | 7.4.288.27 | `tables/v7_4_12.4.0.json` |
| 12.5.0–12.8.1 | 7.5.288.22 | `tables/v7_5.json` |
| 12.9.0–12.10.0 | 7.6.303.29 | `tables/v7_6.json` |
| 12.11.0–12.15.0 | 7.7.299.13 | `tables/v7_7_12.15.0.json` |
| 12.16.0–13.1.0 | 7.8.279.17 | `tables/v7_8.json` |
| 13.2.0–13.14.0 | 7.9.317.25 | `tables/v7_9_13.14.0.json` |
| 14.0.0–14.4.0 | 8.1.307.31 | `tables/v8_1_14.4.0.json` |
| 14.5.0 | 8.3.110.9 | `tables/v8_3.json` |
| 14.6.0–14.21.3 | 8.4.371.23 | `tables/v8_4.json` |
| 15.0.0–15.14.0 | 8.6.395.17 | `tables/v8_6_15.14.0.json` |
| 16.0.0–16.3.0 | 9.0.257.25 | `tables/v9_0_16.3.0.json` |
| 16.4.0–16.5.0 | 9.1.269.38 | `tables/v9_1_16.5.0.json` |
| 16.6.0–16.8.0 | 9.2.230.21 | `tables/v9_2.json` |
| 16.9.0–16.10.0 | 9.3.345.19 | `tables/v9_3_16.10.0.json` |
| 16.11.0–16.20.2 | 9.4.146.26 | `tables/v9_4.json` |
| 17.0.0–17.1.0 | 9.5.172.25 | `tables/v9_5_17.1.0.json` |
| 17.2.0–17.9.1 | 9.6.180.15 | `tables/v9_6_17.9.1.json` |
| 18.0.0–18.2.0 | 10.1.124.8 | `tables/v10_1.json` |
| 18.3.0–18.20.8 | 10.2.154.26 | `tables/v10_2.json` |
| 19.0.0–19.1.0 | 10.7.193.20 | `tables/v10_7_19.1.0.json` |
| 19.2.0–19.9.0 | 10.8.168.25 | `tables/v10_8_19.9.0.json` |
| 20.0.0–20.20.2 | 11.3.244.8 | `tables/v11_3.json` |
| 21.0.0–21.7.3 | 11.8.172.17 | `tables/v11_8_21.7.3.json` |
| 22.0.0–22.23.3 | 12.4.254.21 | `tables/v12_4.json` |
| 23.0.0–23.11.1 | 12.9.202.28 | `tables/v12_9_23.11.1.json` |
| 24.0.0–24.21.0 | 13.6.233.17 | `tables/v13_6.json` |
| 25.0.0–25.9.0 | 14.1.146.11 | `tables/v14_1.json` |
| 26.0.0–26.10.0 | 14.6.202.34 | `tables/v14_6_26.10.0.json` |

</details>

<details>
<summary>跑完整矩阵的 31 条 Node 线</summary>

`8.17` `10.24` `12.5` `12.9` `12.15` `12.22` `13.14` `14.0` `14.4` `14.5` `14.6` `14.21` `15.14`
`16.3` `16.5` `16.8` `16.9` `16.20` `17.1` `17.9` `18.2` `18.20` `19.1` `19.9` `20.20` `21.7`
`22.12` `23.11` `24.12` `25.9` `26.10`

</details>

生成时**只取它需要的那 ~30 个 V8 源文件**：经 blob 过滤的部分克隆 + 本地文件缓存
（`workspace/node-src/`），不需要整份 node 检出：

```sh
python3 scripts/codegen.py --all-from 8.0.0 --donors tables/ --per-minor --keep-existing
```

## 实现

### .jsc 里有什么

里面没有源码文本；头部的 `source_hash` 字段记录的是源码**长度**，不是哈希。可还原的数据：

- 头部字段：`version_hash`、`flag_hash`、payload 长度、校验和、Brotli 或原始 payload
- `SharedFunctionInfo` 树与作用域信息（形参、上下文、栈局部槽）
- 字符串与字面量常量池、字节码数组、源码位置表、handler 表

`decompile` 输出语法合法的伪 JS 重建结果。只以槽位形式存在的名字按槽名输出（`rN`、`__ctx.ctxN`、
`_anon_N`），不会写成会抛 `ReferenceError` 的标识符。

### 处理流程

1. **header** —— 解析 `SerializedCodeData`（V8 ≤ 11 为 24 字节，V8 ≥ 12 为 32 字节），识别
   Brotli，定位 payload。
2. **identify** —— 用内嵌索引把 `version_hash` 映射到 V8 版本；认不出就重算哈希。
3. **deserialize** —— 走 V8 序列化流（标签、回引、hot 对象环、6.x 的 `kBackrefWithSkip` 与变长
   raw）建成对象图。
4. **disassemble** —— 按版本表解码字节码，操作数尺寸、缩放与隐式寄存器表取自 `bytecodes.h`。
5. **decompile** —— 寄存器 IR → 结构化控制流（if、循环、switch、try）→ AST → 打印；生成器/async
   状态机、完成码分派、类装配按版本折叠。

## 验证

四层门禁，每一层都能单独复现：

1. **行为矩阵**（`scripts/verify_behavior.sh`）—— 每个 fixture 在**该版本的 Node**里编译成
   `.jsc` → jscd 反编译（带可运行前导）→ 语法门禁 → **跑产物与原始函数逐用例比对返回值**；另有
   两道结构门禁：默认产物不许丢顶层函数声明（防优化层 DCE）、known-fail 单独统计不混进回归信号。
2. **整脚本对拍**（`scripts/script_diff.js`）—— 完整脚本直接跑 `node`，比 stdout 与退出码（覆盖
   "顶层只有语句 / 只有 const / 只有 class / async"这些 `target` 型用例照不到的形态）。
3. **语料扫描**（`scripts/verify_samples.py`）—— 大规模样本 × 多版本：编译 → 反编译 →
   `node --check` → 在 vm 里以最小 test262 桩加载（我们自己生成的 `__`/`_anon_`/`ctx`/`phi` 名字
   若 ReferenceError 即判为 bug）。
4. **单元测试与静态检查** —— `cargo test`（56 条）+ `cargo clippy --all-targets`（0 告警）+
   `cargo build`（0 告警）+ 零依赖端到端冒烟 `scripts/ci_smoke.sh`（编译 → 反编译 → 语法门禁 →
   5 个整脚本 fixture 真跑对拍）。

```sh
cargo test                                              # 56 条单测
bash scripts/verify_behavior.sh 8.17.0 10.24.1 12.22.12 14.21.3 16.20.2 18.20.8 20.20.2 22.12.0 24.12.0
python3 scripts/verify_samples.py 20.20.2 --limit 200    # 语料；另有 --offset、--samples
```

fixture 的编译方式与 bytenode 完全同参（同样的旗标 + `createCachedData()`，见
`scripts/mkcorpus.js`）；快速上手那段用的是真 bytenode 1.7.0。

**数字**（都可用上面的脚本复现；矩阵那几行是当前二进制实测）：

| 指标 | 数量 |
| --- | --- |
| 支持的 Node 发布 | **510 个**（`v8.0.0` → `v26.10.0`），覆盖 **90 个 V8 版本**（`5.8.283.41` → `14.6.202.34`） |
| 版本表 / 只读堆名表 | **36 张**版本表 + **29 张** `ro-map` 表（`tables/manifest.json` 逐发布索引） |
| 行为 fixture | **36 个**（合计 **121 条断言用例**）+ **5 个**整脚本 fixture —— 41 份源码 / 629 行 |
| 一次全矩阵 | 31 条 Node 线 × 36 = **1,116 格** + 155 格整脚本 ⇒ **3,751 次逐用例比对** + **1,271 次结构门禁** |
| 标准语料扫描 | **408 份**真实样本 × 6 条线（8.17 / 12.22 / 16.20 / 20.20 / 24.12 / 26.10） |
| 全语料扫描（累计） | **2,446 份 × 10 条线 ≈ 2.5 万次**"编译 → 反编译 → 语法 / 加载"检查 |
| 处理过的输入代码 | **2.40 MB / 76,246 行 / 240 万字符** |
| 产出的反编译代码 | **25,029 份 / 301 MB / 627 万行** |
| 真实文件 | Node 自带 `duplexpair.js`、`event_target.js`、`test-abortcontroller.js` × 9 条线 |
| 结果 | **1,108 pass / 0 fail / 8 skip / 0 known-fail** + **155 script pass / 0 fail** |

<details>
<summary>矩阵为什么可信</summary>

- `skip`（8 个格子）表示 fixture 源码语法需要更新的解析器（Node 8/10/12.15 上的 `?.`/`??`，以及
  Node 8.17 上的 BigInt 字面量），在 `tests/fixtures/behav/cases.json` 里以 `min_node` 声明，与失败
  分开统计。
- 语料里 `compile-fail` 的格子是**该版本解析器**编译不了的样本源码（现代语法，如 8.17 的 64 份），
  不是产物问题；`0 语法 / 0 反编译`指的是产物侧。
- **已无 known-fail。** `iter_no_extra_close` 覆盖一个**原生语义随版本变**的用例
  （`for (const v of [1,1,2]) { if (v === 2) continue; }`：6.2/6.8 耗尽时会调一次
  `iterator.return()`，7.8+ 不调）。这类 fixture 在 `tests/fixtures/behav/cases.json` 里标 `same_node_original`：原函数
  改用**与被编译产物相同的 Node 版本**去跑 —— 产物要忠实复现**它自己那个版本**的行为，31 条线现在
  都做到了。
- 曾**吞掉**循环体异常的 IteratorClose 收尾、以及 try/finally 的两处完成码缺口均已修 ——
  `iter_close` 与 `finally_return` 通过。

</details>

<details>
<summary>41 份 fixture 源码</summary>

行为 fixture（每个都在 31 条线上跑）：`arith` `branch` `loop_for` `loop_while` `strings`
`array_ops` `object_ops` `switch_case` `try_catch` `recursion` `closure` `class_basic`
`destructure` `template` `spread_rest` `for_of_in` `arrow_opt` `generator` `operators`
`string_regex` `yield_expr` `async_await` `async_try` `optional_chain` `twoclasses` `call_args3`
`call_once` `finally_return` `finally_solo` `delete_result` `num_literals` `try_catch_after`
`iter_close` `param_defaults` `typeof_flags` `iter_no_extra_close`

整脚本 fixture（比 stdout 与退出码）：`top_stmt` `top_const` `top_var` `top_class` `top_async`

单条版本线一轮语料：≈ 2,800 份产物 / 33.5 MB / ≈ 70 万行反编译代码。

`mise.toml` 固定 harness 默认用的 Node 版本（10.24.1 → 24.12.0）；矩阵还要用更老/更新的线 ——
其余 23 个这样装：

```sh
mise install node@8.17.0 node@12.5.0 node@12.9.0 node@12.15.0 node@13.14.0 node@14.0.0 \
  node@14.4.0 node@14.5.0 node@14.6.0 node@15.14.0 node@16.3.0 node@16.5.0 node@16.8.0 \
  node@16.9.0 node@17.1.0 node@17.9.1 node@18.2.0 node@19.1.0 node@19.9.0 node@21.7.3 \
  node@23.11.1 node@25.9.0 node@26.10.0
```

</details>

## 仓库结构

```
src/              header、serializer、bytecode、disasm、decompile、CLI（帮助文本在 `src/help.rs`）
tables/           各 V8 版本的表：操作数、roots、scope-info 布局、运行时名、
                  只读堆名表（ro_map_*.json）+ manifest.json
scripts/          表生成、语料构造、验证 harness、release.sh、ci_smoke.sh
tests/            单元/集成测试 + fixture（现场编译成 .jsc）
docs/             VERSIONS.md（逐版本工程笔记）、ARCHITECTURE.md、DECOMPILE.md、stream-format.md
.github/          tag 触发的发版工作流：六个平台、裸二进制、带描述的 Release
CONTRIBUTING.md   构建、改动要过的门禁、怎么加 fixture
```

## 已知限制

- 拿不到源码文本与原始标识符，名字都是重建出来的。
- V8 内建显示为 `__runtime.*` / `__intrinsic.*` 调用（只有最小桩实现）。
- 解不出的作用域槽写成 `__ctx.ctxN`。Node 22+ 的内建名要靠只读堆名表：内嵌的那批只适用
  macOS（见[用法](#用法)），Linux/Windows 上自己用 `jscd ro-map` 建一张，或接受 `<ro…>` 占位。
- `decompile` 追求可读可跑，不做字节级往返一致。

## 许可

MIT，见 [LICENSE](LICENSE)。

## 致谢

[bytenode](https://github.com/bytenode/bytenode)（本工具针对的打包方式）；
[View8](https://github.com/suleram/View8)（反汇编格式沿用其约定）；
[swc](https://swc.rs)（AST 级清理：terser 复制传播的 Rust 移植）。