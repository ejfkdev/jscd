# Case Study: Decompiling Electron 11 (V8 8.7.220.31) `.jsc` files

This directory documents the effort to extend jscd to support V8 8.7.220.31 —
the V8 version shipped exclusively in **Electron 11 (Chromium 87)**, which was
never used by any Node.js release.

## The problem

A 12.7 MB `.jsc` file (`main.jsc`) was produced by an Electron 11 application
using `vm.Script.createCachedData()`. jscd's table coverage spans Node 8.0–26.10
(V8 5.8–14.6), but V8 8.7 was skipped because no Node release ever shipped it.
The file's version hash (`0x02f91115`) doesn't match any entry in
`tables/manifest.json`.

## What V8 8.7.220.31 is

Cross-checked against Electron's release index (`releases.electronjs.org`):

| V8 version | Electron | Chromium |
|---|---|---|
| 8.7.220.31 | **11.2.0 – 11.4.6** | 87.0.4280.141 |

Confirmed by running Electron 11.4.6 itself:
`process.versions.v8` → `8.7.220.31-electron.0`.

## Ground-truth artifacts

All in this directory (except `main.jsc` which is too large for git — see below):

- **`hello.js`** — 3-line test source
- **`hello.jsc`** — 472-byte code cache compiled by Electron 11's V8
- **`hello.disasm.txt`** — jscd's disassembly of `hello.jsc` (correct)
- **`hello.decompiled.js`** — jscd's decompiled output (correct with `JSCD_NO_SWC=1`)

## What we changed

### 1. New version table (`tables/v8_7_220_31.json`)

Built from the 8.6 table (Node 15, V8 8.6.395.17) with the following deltas
extracted from the V8 8.7.220.31 source tree
(`chromium.googlesource.com/v8/v8.git`, tag `8.7.220.31`):

| Field | 8.6 table (inherited) | 8.7 (corrected) | Source |
|---|---|---|---|
| `serialization.tags` | kDeferred@0x16, kAlignmentPrefix@0x17, kSynchronize@0x1a, ... | kDeferred removed, kAlignmentPrefix@0x16, kSynchronize@0x19, kRegisterPendingForwardRef@0x27, kResolvePendingForwardRef@0x28, kNewMetaMap@0x29 | `src/snapshot/serializer-deserializer.h` |
| `bytecodes` | 182 entries | 183 entries (+ `LdaNamedPropertyFromSuper` at index 42) | `src/interpreter/bytecodes.h` |
| `intrinsic_names` | `ToStringRT` (index 22) | `ToString` | `src/interpreter/interpreter-intrinsics.h` |
| `tagged_size` | 8 | 4 (pointer compression on in Chromium 87 desktop) | `src/common/globals.h` (`V8_COMPRESS_POINTERS`) |
| `header` | payload_length@16, checksum@20 (24-byte layout) | num_reservations@16, payload_length@20, checksum@24, header_size=32 | `src/snapshot/code-serializer.h` |

### 2. Serializer fix: `kRegisterPendingForwardRef` (`src/serializer.rs`)

**Bug:** jscd's handler for `kRegisterPendingForwardRef` read a `putint()` from
the stream, consuming 1–4 extra bytes per occurrence. V8 8.7's deserializer
(`src/snapshot/deserializer.cc`) does **not** read any data for this tag — it
just registers the current slot as a pending forward reference and returns
`current + 1`.

**Impact:** Without this fix, the parser desynchronizes at ~offset 977K in
`main.jsc` (object ~32745). With the fix, it parses to ~11.5 MB (99% of the
12.7 MB file).

**Fix:** Changed `SlotValue::PendingRef(self.putint()?)` to
`SlotValue::PendingRef(0)` — no stream data consumed.

### 3. Legacy trailing-byte tolerance (`src/serializer.rs`)

V8 8.7's deferred section ends with non-`kNop` bytes (observed byte `0x89`).
The 8.6 parser expected only `kNop` padding after `kSynchronize`. Added a
`legacy_tolerant` break instead of a hard error, allowing decompilation to
proceed with partially-parsed deferred objects.

### 4. SWC optimizer bypass (`JSCD_NO_SWC=1`)

The swc-based JS optimizer (`src/opt_js.rs`) incorrectly drops call arguments
when the bytecode contains `CallRuntime DeleteLookupSlot` (V8's module-wrapper
code). The text-level optimizer (`src/opt.rs`) handles this correctly. Use
`JSCD_NO_SWC=1` or `--runtime` to skip the swc pass.

### 5. Large-function skip (`src/decompile.rs`)

Functions with > 500 bytecodes cause infinite recursion in `emit_range_inner`
(the control-flow pattern detector). This is a pre-existing jscd bug, not
specific to V8 8.7 — it's just more likely to trigger on the large bundled
functions in an obfuscated Electron app. The skip emits a placeholder comment
and continues.

## What works

| Capability | hello.jsc (472 B) | main.jsc (12.7 MB) |
|---|---|---|
| Version recognition | ✅ exact | ✅ exact |
| Header parsing | ✅ | ✅ |
| Object parsing | ✅ all 17 objects | ✅ 224K objects (99% of payload) |
| Bytecode disassembly | ✅ | ✅ 7,253 functions, 1.4M lines |
| String extraction | ✅ | ✅ 163,810 strings |
| Decompilation | ✅ `console.log(greet("world"))` | ⚠️ partial (5 large functions skipped) |

## What doesn't work yet

1. **Deferred section** — The last ~1.2 MB of `main.jsc` (deferred objects)
   fails to parse because a backref resolves to an `RoRef` (read-only heap
   reference) instead of an object. This is because V8 8.7's ro-map (read-only
   heap name table) hasn't been generated. The ro-map requires a probe `.jsc`
   compiled on V8 8.7 and processed by `scripts/build_ro_map.sh`.

2. **SWC optimizer** — Drops arguments from calls preceded by
   `DeleteLookupSlot`. Workaround: `JSCD_NO_SWC=1`.

3. **Large functions** — The `emit_range_inner` recursion in jscd's control-flow
   detector doesn't terminate on some deeply-nested functions (673+ bytecodes).
   This is a pre-existing jscd bug, not V8 8.7 specific.

## How to reproduce

### Prerequisites

- Rust 1.96+ (`rustup update stable`)
- Electron 11.4.6 (for ground-truth `.jsc` files):
  ```bash
  curl -L https://github.com/electron/electron/releases/download/v11.4.6/electron-v11.4.6-darwin-x64.zip -o el11.zip
  unzip el11.zip
  ```

### Build

```bash
cargo build --release
```

### Test on ground truth

```bash
# Compile a test .jsc with Electron 11's V8
cat > hello.js <<'EOF'
function greet(name) { return "hello " + name; }
console.log(greet("world"));
EOF
ELECTRON_RUN_AS_NODE=1 ./Electron.app/Contents/MacOS/Electron -e '
  const vm = require("vm");
  const fs = require("fs");
  const s = new vm.Script(fs.readFileSync("hello.js","utf8"), {produceCachedData:true, filename:"hello.js"});
  fs.writeFileSync("hello.jsc", s.cachedData);
'

# Decompile (skip swc optimizer due to the DeleteLookupSlot bug)
JSCD_TS=4 JSCD_NO_SWC=1 ./target/release/jscd hello.jsc
```

Expected output:
```js
// ── 模块/脚本顶层代码 ─────────────────────────────
__runtime.DeleteLookupSlot([/* function greet */ greet, 0], __anonymous);
let r1 = console.log(greet("world"));
function greet() { /* 未编译（UncompiledData）：源码不在 code cache 中 */ }
```

### Decompile a large file

```bash
JSCD_TS=4 JSCD_NO_SWC=1 ./target/release/jscd main.jsc > main_out.js 2>main_err.txt
```

### Extract strings only (works on any size)

```bash
JSCD_TS=4 ./target/release/jscd strings main.jsc > strings.txt
```

### Disassemble all functions (works on any size)

```bash
JSCD_TS=4 ./target/release/jscd disasm main.jsc > disasm.txt
```

## Environment variables

| Variable | Effect |
|---|---|
| `JSCD_TS=4` | Force tagged size 4 (pointer compression). Required for Electron 11. |
| `JSCD_NO_SWC=1` | Skip the swc-based JS optimizer (works around the DeleteLookupSlot bug). |
| `JSCD_NO_OPT=1` | Skip ALL optimization (text + swc). Produces register-level output. |
| `JSCD_DBG_LEGACY=1` | Trace legacy-family object parsing. |
| `JSCD_DBG_OPT=1` | Trace the text optimizer passes. |

## V8 8.7 source tree

The V8 source at tag `8.7.220.31` is available from:

```bash
git clone --filter=blob:none --sparse --branch 8.7.220.31 \
  https://chromium.googlesource.com/v8/v8.git v8-8.7
cd v8-8.7
git sparse-checkout set \
  include/v8-version.h \
  src/snapshot/serializer-deserializer.h \
  src/snapshot/code-serializer.h \
  src/snapshot/deserializer.cc \
  src/snapshot/serializer.cc \
  src/interpreter/bytecodes.h \
  src/interpreter/bytecode-operands.h \
  src/interpreter/interpreter-intrinsics.h \
  src/objects/shared-function-info.h \
  src/execution/frame-constants.h \
  src/roots/roots.h \
  src/common/globals.h
```

Key files for extracting version-specific layout:

- `src/snapshot/serializer-deserializer.h` → serialization tag enum
- `src/snapshot/code-serializer.h` → header layout (field offsets)
- `src/snapshot/deserializer.cc` → `kRegisterPendingForwardRef` semantics
- `src/interpreter/bytecodes.h` → bytecode opcode list
- `src/common/globals.h` → `kTaggedSize`, `kObjectAlignmentBits`
