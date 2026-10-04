# jscd

Reverse [bytenode](https://github.com/bytenode/bytenode)-compiled `.jsc` files back to JavaScript.

A `.jsc` is a V8 code cache (`v8::ScriptCompiler::CreateCodeCache`). `jscd` parses it statically:
pure Rust, one binary, no patched V8, no Node runtime, no network.

**English** (this file) · [中文](README.zh.md) · [Version-by-version notes](docs/VERSIONS.md) (Chinese)

[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Rust 1.96+](https://img.shields.io/badge/rust-1.96%2B-orange.svg)](Cargo.toml)
[![Node 8.0 – 26.10](https://img.shields.io/badge/Node-8.0%20%E2%80%93%2026.10-green.svg)](#supported-versions)
[![V8 5.8 – 14.6](https://img.shields.io/badge/V8-5.8%20%E2%80%93%2014.6-green.svg)](#supported-versions)
[![release](https://github.com/ejfkdev/jscd/actions/workflows/release.yml/badge.svg)](https://github.com/ejfkdev/jscd/actions/workflows/release.yml)
[![crates.io](https://img.shields.io/crates/v/jscd.svg)](https://crates.io/crates/jscd)

**Contents:** [Quick start](#quick-start) · [Install](#install) · [Usage](#usage) ·
[Supported versions](#supported-versions) · [How it works](#how-it-works) ·
[Verification](#verification) · [Repository layout](#repository-layout) · [Limitations](#limitations) ·
[Contributing](CONTRIBUTING.md)

## Quick start

```console
$ cat hello.js
function greet(name) {
  return "hello " + name;
}
console.log(greet("world"));

$ npm i -g bytenode && bytenode -c hello.js      # Node 20.20.2 in this transcript
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

The first bytes are the code-cache magic in little-endian (`cc 05 de c0` = `0xc0de05cc`);
`jscd info hello.jsc` decodes the whole header (sample in [Usage](#usage)).

The output above is what comes out *after* both optimization layers. `JSCD_NO_OPT=1 jscd hello.jsc`
shows the raw translation instead — V8's register shuffling, one instruction at a time.

On Node 22+ (V8 12.4+) some built-in property names live in the read-only heap. For the 29 V8
minors covered by the behavior matrix `jscd` ships a name table (`tables/ro_map_*`) and resolves
them automatically — **on macOS**, where those tables were extracted: read-only-heap indices are
platform-specific, so on other systems build your own with `jscd ro-map` and pass `--ro-map`
(`JSCD_NO_RO_MAP=1` turns the embedded table off). Without a table such names show as `<ro0_…>`
placeholders — never as the wrong name.

## Install

**macOS / Linux — Homebrew**

```sh
brew install ejfkdev/tap/jscd
```

**Windows — Scoop**

```powershell
scoop bucket add ejfkdev https://github.com/ejfkdev/scoop-bucket
scoop install jscd
```

Straight from the manifest URL, without adding the bucket first:

```powershell
scoop install https://raw.githubusercontent.com/ejfkdev/scoop-bucket/main/bucket/jscd.json
```

> `scoop install ejfkdev/scoop-bucket/jscd` does **not** work: Scoop resolves `bucket/app`
> against buckets you have already added, not against an `owner/repo` path.

**Prebuilt binaries** — every tagged release ships bare, ready-to-run executables
(Linux / macOS / Windows × x64 / arm64; Linux amd64 is a static musl build, Linux and Windows
are UPX-compressed):

Grab one straight from the release page (no archive, no installer — the file *is* the
executable):

```sh
curl -fLO https://github.com/ejfkdev/jscd/releases/download/v0.1.0/jscd-v0.1.0-linux-amd64
chmod +x jscd-v0.1.0-linux-amd64
./jscd-v0.1.0-linux-amd64 --version
```

**cargo** (any platform with Rust 1.96+)

```sh
cargo install jscd        # build from crates.io
cargo binstall jscd       # or fetch the release binary instead of compiling (cargo-binstall)
```

**From source**

```sh
cargo install --git https://github.com/ejfkdev/jscd     # latest main
git clone https://github.com/ejfkdev/jscd && cd jscd
cargo build --release        # -> target/release/jscd
cargo install --path .       # ...or install that build into ~/.cargo/bin
```

Requires Rust 1.96+ (swc, the JS optimizer, needs a recent rustc). No system dependencies.

<details>
<summary>Releasing (maintainers)</summary>

`scripts/release.sh vX.Y.Z` runs the release gates (`cargo test --release`,
`clippy --all-targets -- -D warnings`, and the end-to-end smoke `scripts/ci_smoke.sh` — fixtures
compiled to real `.jsc`, decompiled, syntax-gated and run-compared), then pushes an **annotated**
tag whose message becomes the GitHub Release description. The tag triggers
`.github/workflows/release.yml`, which rebuilds the six bare binaries listed above. Then
`cargo publish` to crates.io (`release.sh` keeps the tag and the `Cargo.toml` version in sync); the
[Homebrew tap](https://github.com/ejfkdev/homebrew-tap) and the
[Scoop bucket](https://github.com/ejfkdev/scoop-bucket) pick the new release up on their daily
auto-update run.

</details>

## Usage

```sh
jscd app.jsc                     # decompile one file to stdout
jscd app.jsc app.js              # ...to a file
jscd dist/                       # every .jsc under dist/ → dist-out/ (tree mirrored)
jscd dist/ out/                  # ...into out/ instead
jscd info app.jsc                # header fields, detected Node/V8 version
jscd disasm --filter main app.jsc
jscd ro-map probe.jsc > m.json   # build a read-only-heap name table
jscd --help                      # bilingual help (-h, `help`, `help <SUBCOMMAND>`)
```

- **Input** is one `.jsc` file, or a directory scanned recursively for `*.jsc`. **Output** defaults
  to stdout for a file, and to `<INPUT>-out/` (tree mirrored) for a directory; `-o` / the OUTPUT
  argument writes to a file or directory, `-` means stdout.
- A `.jsc` from a V8 outside the table range is **rejected with the detected version and the
  supported range** in the message, rather than silently emitting a runtime-only file;
  `jscd info` carries a `supported: yes|no` line for the same reason.
- Read-only-heap names (Node 22+) resolve automatically for the builds shipped in
  `tables/ro_map_*` — on macOS, the platform those tables were extracted on (RO indices depend on
  the platform, so an embedded table is applied only where it can be trusted). Elsewhere build one
  with `jscd ro-map` / `scripts/build_ro_map.sh` and pass `--ro-map`; `JSCD_NO_RO_MAP=1` disables
  the embedded table. A table that does not match is ignored — you get `<ro0_…>` placeholders
  rather than wrong names.
- The interface language follows `JSCD_LANG` (→ `LC_ALL` → `LC_MESSAGES` → `LANGUAGE` → `LANG` →
  `LC_CTYPE`): `zh*` is Chinese, anything else English. `JSCD_LANG=zh|en` forces one.

| Flag | Applies to | Effect |
| --- | --- | --- |
| `-o, --output <PATH>` | all forms | output file / directory / `-` (same as the OUTPUT argument) |
| `--quiet` | directory input | suppress per-file progress |
| `--json` | all subcommands | machine-readable output |
| `--filter <SUBSTR>` | `disasm` | only emit functions whose name contains `SUBSTR` |
| `--ro-map <PATH>` | `decompile` | read-only-heap name table produced by `jscd ro-map` |
| `--verify` | `decompile` | syntax gate: run the result through `node --check` (needs `node` on `PATH`) |
| `--runtime` | `decompile` | keep the runnable preamble (`__runtime` stubs, flattened `context` vars, name aliases). **Off by default** — add it when you want to run the product with `node` |
| `-h, --help` | all forms | print help (`jscd help <SUBCOMMAND>` for one command) |
| `-v, -V, --version` | all forms | print name, version and repository |

By default the output is **just the reconstructed code** — no runtime preamble and no comments
(the AST layer strips even the structural markers; `JSCD_NO_OPT=1` keeps them). `--runtime`
prepends the `__runtime` proxy (minimal stubs for the V8 builtins the bytecode calls), the
file-level declarations for flattened context variables, and the name-alias block, so the file
runs as-is with `node`.

<details>
<summary><code>jscd info</code> sample output</summary>

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

### Two optimization layers

`JSCD_NO_OPT=1` disables both, for diffing against the raw translation.

1. **Text layer** (`src/opt.rs`): folds away bytecode register shuffling — single-use values are
   inlined into their use, stores overwritten before any read are dropped, unused register
   declarations and pure bookkeeping such as `DeclareGlobals` are removed, `let r0; … r0 = E;` is
   lifted into a declarator initializer, the `CallProperty`-rendering leftover `rX = A.B;` is
   dropped (the call already reads the property at the call site, so keeping it would run a getter
   twice), and **expressions are rebuilt**: V8 hoists the receiver and every argument into
   registers, and a dedicated pass folds those single-use loads back into the call, in JavaScript
   evaluation order (receiver → property → arguments), so nothing is reordered. It refuses to
   cross blocks, loop headers, braceless control bodies, or any later use of the value (including
   the self-referencing write `r2 = new r2(a0)`).
2. **AST layer** (`src/opt_js.rs`): the text is handed to [swc](https://swc.rs) — the Rust port of
   terser — for **copy propagation**, unused-variable elimination and constant folding:

   ```js
   let r0, r1, r2, r3;        // as decompiled            // after both layers
   r0 = greet;                console.log(greet("world"));
   r2 = console;       →      function greet(a0) {
   r1 = r2.log;                 return "hello " + a0;
   r3 = r0("world");          }
   r2.log(r3);
   ```

   The pipeline is calibrated: `paren_remover → resolver → optimize → hygiene → fixer` (skipping
   `paren_remover` miscompiles), the snippet is wrapped in a function first (swc only propagates
   copies inside function scope, and the payload is a `Module.wrap` function body at runtime
   anyway) and the wrapper is stripped afterwards; no renaming, no statement merging, no arrow
   conversion, names and parameters kept. Unparseable or suspicious output → the text-layer result
   is kept instead.

## Supported versions

Version tables are generated from Node sources by `scripts/codegen.py` and embedded at build time.
Selection is keyed on the V8 version hash; every Node release from 8.0.0 on is listed in
`tables/manifest.json` (510 entries), and one table covers each V8 minor — identical tables are
stored once.

**Every Node release from 8.0.0 through 26.10.0 is identified** (510 releases, 90 V8 versions),
and **29 of the 36 V8 minors pass the full behavior matrix** — 31 Node lines, all green
(1,108 pass / 0 fail / 8 skip / 0 known-fail). The remaining 7 minors (5.8, 6.0, 6.1, 6.6, 6.7,
7.0, 7.4) are identified by `jscd info` but their payload deserialization **errors out loudly**
instead of emitting garbage.

| V8 minors | Node lines | Result |
| --- | --- | --- |
| 6.2 → 14.6, **29 of 36** | 31 lines, 8.17 → 26.10 | **all green** — 1,108 pass / 0 fail / 8 skip |
| 5.8, 6.0, 6.1, 6.6, 6.7, 7.0, 7.4 | 8.0–8.9, 10.0–10.8, 11.x, 12.0 (partial releases) | identified; payload **explicitly rejected** |

<details>
<summary>Full mapping: 36 version tables → 510 Node releases</summary>

| Node releases | V8 (newest) | table |
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
<summary>The 31 Node lines running the full matrix</summary>

`8.17` `10.24` `12.5` `12.9` `12.15` `12.22` `13.14` `14.0` `14.4` `14.5` `14.6` `14.21` `15.14`
`16.3` `16.5` `16.8` `16.9` `16.20` `17.1` `17.9` `18.2` `18.20` `19.1` `19.9` `20.20` `21.7`
`22.12` `23.11` `24.12` `25.9` `26.10`

</details>

Generation fetches **only the ~30 V8 source files it needs** per version, through a
blob-filtered partial clone plus an on-disk file cache (`workspace/node-src/`); no full Node
checkout is required:

```sh
python3 scripts/codegen.py --all-from 8.0.0 --donors tables/ --per-minor --keep-existing
```

## How it works

### What a `.jsc` holds

It contains no source text, and the header's `source_hash` field stores the source *length*, not a
hash. Recoverable data:

- header fields: `version_hash`, `flag_hash`, payload length, checksum, Brotli or raw payload
- `SharedFunctionInfo` tree and scope info (parameter, context and stack slots)
- string and literal constant pools, bytecode arrays, source-position and handler tables

`decompile` emits a syntactically valid pseudo-JS reconstruction. Names that exist only as scope
slots are printed as slot names (`rN`, `__ctx.ctxN`, `_anon_N`), never as identifiers that would
throw `ReferenceError`.

### Pipeline

1. **header** — parse `SerializedCodeData` (24 bytes for V8 ≤ 11, 32 bytes for V8 ≥ 12), detect
   Brotli, locate the payload.
2. **identify** — map `version_hash` to a V8 version with the embedded index; fall back to
   recomputing the hash when the index misses.
3. **deserialize** — walk the V8 serialization stream (tags, backrefs, the hot-object ring, plus
   6.x's `kBackrefWithSkip` and variable-length raw) into an object graph.
4. **disassemble** — decode bytecode against the per-version table; operand sizes, scaling and
   implicit-register tables come from `bytecodes.h`.
5. **decompile** — register IR → structured control flow (if, loops, switch, try) → AST → print;
   generator/async state machines, completion-code dispatch and class assembly are folded version
   by version.

## Verification

Four gates, each reproducible on its own:

1. **Behavior matrix** (`scripts/verify_behavior.sh`) — every fixture is compiled to `.jsc` with that
   Node version, decompiled by jscd (with the runnable prelude), gated on syntax, then the
   product and the original function are **compared case by case on their return values**. Two
   structural gates ride along: the default (non-`--runtime`) product may not lose top-level
   declarations (AST-layer DCE guard), and known-fails are counted separately so they cannot mask
   regressions.
2. **Whole-script diffing** (`scripts/script_diff.js`) — a complete script is run under `node` and
   stdout/exit code compared (covers "top level is statements only / `const` only / `class` only /
   `async`", which `target`-style fixtures cannot reach).
3. **Corpus sweeps** (`scripts/verify_samples.py`) — large sample sets × many versions: compile →
   decompile → `node --check` → load in a `vm` with a minimal test262 stub (a `ReferenceError`
   naming anything we generate — `__*`, `_anon_*`, `ctx*`, `phi*` — is a bug).
4. **Unit tests and static checks** — `cargo test` (56 tests) + `cargo clippy --all-targets`
   (0 warnings) + `cargo build` (0 warnings) + the dependency-free end-to-end smoke
   `scripts/ci_smoke.sh` (compile → decompile → syntax-gate → run the 5 whole-script fixtures).

```sh
cargo test                                              # 56 unit tests
bash scripts/verify_behavior.sh 8.17.0 10.24.1 12.22.12 14.21.3 16.20.2 18.20.8 20.20.2 22.12.0 24.12.0
python3 scripts/verify_samples.py 20.20.2 --limit 200    # corpus; also --offset, --samples
```

Fixtures are compiled the same way bytenode does it (same flags, `createCachedData()` — see
`scripts/mkcorpus.js`); the transcript at the top uses real bytenode 1.7.0.

**Numbers** (all reproducible with the scripts above; the matrix rows are freshly measured):

| Metric | Value |
| --- | --- |
| Supported Node releases | **510** (`v8.0.0` → `v26.10.0`), spanning **90 V8 versions** (`5.8.283.41` → `14.6.202.34`) |
| Version tables / read-only-heap name maps | **36** tables + **29** `ro-map` tables, indexed per release in `tables/manifest.json` |
| Behavior fixtures | **36** fixtures (**121 assertion cases**) + **5** whole-script fixtures — 41 files / 629 lines |
| One full matrix run | 31 Node lines × 36 fixtures = **1,116 cells** + 155 script cells ⇒ **3,751 case-by-case comparisons** and **1,271 structural gates** |
| Standard corpus sweep | **408** real samples × 6 lines (8.17 / 12.22 / 16.20 / 20.20 / 24.12 / 26.10) |
| Full corpus sweep (cumulative) | **2,446 samples × 10 lines ≈ 25,000** compile → decompile → syntax/load checks |
| Input JavaScript processed | **2.40 MB / 76,246 lines / 2.4 M characters** |
| Decompiled output produced | **25,029 files / 301 MB / 6.27 M lines** |
| Real-world files | Node's own `duplexpair.js`, `event_target.js`, `test-abortcontroller.js` × 9 lines |
| Result | **1,108 pass / 0 fail / 8 skip / 0 known-fail** + **155 script-pass / 0 fail** |

<details>
<summary>How the matrix stays honest</summary>

- `skip` (8 cells) marks fixtures whose source syntax needs a newer parser (`?.`/`??` on Node
  8/10/12.15, BigInt literals on 8.17), declared as `min_node` in `tests/fixtures/behav/cases.json`
  and counted separately from failures.
- `compile-fail` cells in the corpus are sample *sources* the version's own parser rejects (modern
  syntax — 64 of them on 8.17); the `0 syntax / 0 decompile` claim is about the products.
- **No known-fail remains.** `iter_no_extra_close` covers a case whose *native* semantics differ
  between V8 versions (`for (const v of [1,1,2]) { if (v === 2) continue; }` calls
  `iterator.return()` once on 6.2/6.8 and never on 7.8+): such fixtures are marked
  `same_node_original` in `cases.json`, so the original is run with **the same Node version** the
  artifact was compiled with — the product must reproduce *its own* version's behavior, which it
  does on all 31 lines.
- The IteratorClose epilogue that used to **swallow** loop-body exceptions, and both `try/finally`
  completion-code gaps, are fixed — `iter_close` and `finally_return` pass.

</details>

<details>
<summary>The 41 fixture sources</summary>

Behavior fixtures (each runs on all 31 lines): `arith` `branch` `loop_for` `loop_while` `strings`
`array_ops` `object_ops` `switch_case` `try_catch` `recursion` `closure` `class_basic`
`destructure` `template` `spread_rest` `for_of_in` `arrow_opt` `generator` `operators`
`string_regex` `yield_expr` `async_await` `async_try` `optional_chain` `twoclasses` `call_args3`
`call_once` `finally_return` `finally_solo` `delete_result` `num_literals` `try_catch_after`
`iter_close` `param_defaults` `typeof_flags` `iter_no_extra_close`

Whole-script fixtures (stdout + exit code compared): `top_stmt` `top_const` `top_var` `top_class`
`top_async`

One corpus line in one run: ≈ 2,800 products / 33.5 MB / ≈ 700 k lines of decompiled code.

`mise.toml` pins the Node versions the harness uses by default (10.24.1 → 24.12.0); the matrix
also uses older and newer lines — install the remaining 23 with:

```sh
mise install node@8.17.0 node@12.5.0 node@12.9.0 node@12.15.0 node@13.14.0 node@14.0.0 \
  node@14.4.0 node@14.5.0 node@14.6.0 node@15.14.0 node@16.3.0 node@16.5.0 node@16.8.0 \
  node@16.9.0 node@17.1.0 node@17.9.1 node@18.2.0 node@19.1.0 node@19.9.0 node@21.7.3 \
  node@23.11.1 node@25.9.0 node@26.10.0
```

</details>

## Repository layout

```
src/              header, serializer, bytecode, disasm, decompile, CLI (`src/help.rs` holds the help text)
tables/           per-V8 tables: opcode operands, roots, scope-info layout, runtime names,
                  read-only-heap name maps (`ro_map_*.json`) + manifest.json
scripts/          table codegen, corpus builders, verification harnesses, release.sh, ci_smoke.sh
tests/            unit + integration tests, and fixtures (compiled to .jsc on the fly)
docs/             VERSIONS.md (per-version engineering notes), ARCHITECTURE.md, DECOMPILE.md,
                  stream-format.md
.github/          tag-triggered release workflow: 6 platforms, bare binaries, described releases
CONTRIBUTING.md   build, the gates a change must pass, how to add a fixture
```

## Limitations

- No source text and no original identifiers; names are reconstructed.
- V8 builtins appear as `__runtime.*` / `__intrinsic.*` calls with minimal stubs.
- Scope slots that cannot be named print as `__ctx.ctxN`. Built-in names on Node 22+ need a
  read-only-heap name table: the embedded ones are macOS-only (see [Usage](#usage)), so on
  Linux/Windows build your own (`jscd ro-map`) or live with `<ro…>` placeholders.
- `decompile` targets readable, runnable output, not byte-identical round-tripping.

## License

MIT — see [LICENSE](LICENSE).

## Acknowledgements

[bytenode](https://github.com/bytenode/bytenode) for the packaging this tool targets;
[View8](https://github.com/suleram/View8) for the disassembly format conventions;
[swc](https://swc.rs) for the AST-level cleanup (a Rust port of terser's copy propagation).