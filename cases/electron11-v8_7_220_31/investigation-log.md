# Investigation log: V8 8.7.220.31 (Electron 11)

Chronological record of the investigation, including dead ends and breakthroughs.

## Phase 1: Identification

1. **File**: `main.jsc` (12,769,864 bytes) in `~/Downloads/`
2. **Magic**: `D2 03 DE C0` → `0xC0DE03D2` (V8 code cache magic)
3. **Version hash**: `0x02F91115` → jscd brute-forces to `8.7.220.31`
4. **Node mapping**: No Node release uses V8 8.7 → "unknown (no matching Node release)"
5. **Electron mapping**: `releases.electronjs.org` → `8.7.220.31 = Electron 11.2.0–11.4.6`
6. **Confirmed**: Electron 11.4.6 `process.versions.v8 = 8.7.220.31-electron.0`

## Phase 2: Table creation

1. Cloned V8 source at tag `8.7.220.31` from `chromium.googlesource.com`
2. Extracted serialization tag enum from `serializer-deserializer.h`
3. Extracted bytecode list from `bytecodes.h` (183 entries, +1 vs 8.6)
4. Extracted intrinsics from `interpreter-intrinsics.h` (25, 1 rename)
5. Created `tables/v8_7_220_31.json` based on 8.6 table + deltas
6. Registered in `manifest.json` + `tables_embed.rs`

**Dead end**: Tried to use `scripts/codegen.py` to auto-generate the table.
The script expects a Node repo layout (`deps/v8/...`) and doesn't support
standalone V8 checkouts. Hand-building the JSON was faster.

## Phase 3: Ground truth

1. Downloaded Electron 11.4.6 (`electron-v11.4.6-darwin-x64.zip`)
2. Compiled `hello.js` → `hello.jsc` (472 bytes) using `ELECTRON_RUN_AS_NODE=1`
3. Ran `--print-bytecode` to get the real bytecode dump
4. Header matches `main.jsc` exactly (same magic, same version hash)

**Key insight**: `vm.Script` only compiles the top-level function. Inner
functions like `greet()` are lazily compiled and appear as "uncompiled" in
the `.jsc`. This is expected — the code cache only contains what was compiled
at cache-creation time.

## Phase 4: Debugging the serializer

### Bug 1: Tag enum off-by-one

Initial table used 8.6 tag values. The `kDeferred` tag was removed in 8.7,
shifting everything from `kAlignmentPrefix` onward down by 1. Fixed by
reading the exact enum from `serializer-deserializer.h`.

### Bug 2: `kRegisterPendingForwardRef` reads a spurious putint

**Symptom**: Parser desync at offset 977,605 (object ~32745) with
"unknown serialization tag 0xC1".

**Root cause**: jscd's handler for `kRegisterPendingForwardRef` called
`self.putint()`, consuming 1–4 bytes from the stream. But V8 8.7's deserializer
does NOT read any data for this tag — it just registers the current slot as
pending and returns `current + 1`.

**Impact**: Each occurrence consumed 4 extra bytes (the putint read a 4-byte
value because the next byte `0x27` has low 2 bits = 3, meaning 4-byte encoding).
After ~32K objects, the accumulated error caused a complete desync.

**Fix**: `SlotValue::PendingRef(0)` — no stream data consumed.

After this fix, parsing goes from 977K → 11.5M (99% of the 12.7M file).

### Bug 3: Trailing non-Nop bytes after deferred section

**Symptom**: "unexpected byte 0x89 after kSynchronize at 11519216"

**Root cause**: V8 8.7's deferred section ends with non-`kNop` bytes. The 8.6
parser expected only `kNop` padding.

**Fix**: Made the trailing-byte check tolerant for legacy family (break
instead of error).

### Bug 4: Deferred backref resolves to RoRef

**Symptom**: "deferred backref is not an object reference: Ref(RoRef(4, 2597))"

**Root cause**: A backref in the deferred section resolves to a read-only heap
reference instead of an object. This happens because V8 8.7's ro-map (read-only
heap name table) hasn't been generated for this version.

**Status**: Unfixed. The deferred section is skipped (tolerant mode), and
decompilation proceeds with the ~99% of objects that were parsed successfully.

## Phase 5: Decompilation

### Bug 5: SWC optimizer drops call arguments

**Symptom**: `greet("world")` → `greet()` in the optimized output.

**Root cause**: The swc-based JS optimizer (`src/opt_js.rs`) incorrectly
eliminates call arguments when the bytecode contains `CallRuntime DeleteLookupSlot`
(V8's module-wrapper code). The text-level optimizer handles this correctly.

**Fix**: `JSCD_NO_SWC=1` env var to skip the swc pass. Added in `src/cli.rs`.

### Bug 6: Stack overflow on large functions

**Symptom**: Stack overflow (SIGSEGV) during `decompile_function` for SFI 60944
(673 bytecodes, 26 registers).

**Root cause**: Infinite recursion in `emit_range_inner` (the control-flow
pattern detector for try/catch/for/switch). The `emit_depth > 80` guard
doesn't catch all recursive paths (specifically `try_emit_try_finally_no_catch`
has its own recursion that bypasses the depth check).

**Workaround**: Skip functions with > 500 bytecodes (5 functions in `main.jsc`).

**Status**: Unfixed. This is a pre-existing jscd bug, not V8 8.7 specific.

## Phase 6: Results

| Metric | Value |
|---|---|
| Objects parsed | 224,126 |
| Functions found | 7,253 |
| Strings extracted | 163,810 |
| Functions decompiled | 7,248 (5 skipped) |
| Disasm lines | 1,435,172 |
| `hello.jsc` round-trip | ✅ correct |

## What the file contains

The `main.jsc` file is an Electron 11 application's main-process code cache,
produced by `vm.Script.createCachedData()`. Content includes:

- **Obfuscated JavaScript** (javascript-obfuscator with `splitStrings` option,
  9–10 char chunks, `_0x` hex identifiers)
- **System information collection**: CPU models, GPU/VRAM, BIOS, serial numbers,
  MAC addresses, PCI vendor/device IDs, WMI/`dmidecode` commands
- **Platform coverage**: Windows (`wmic`, PowerShell, HKLM registry), Linux
  (`/proc/cpuinfo`, `/etc/*`, `ipconfig`), macOS (`system_profiler`)
- **Electron integration**: `ElectronJSONSettingsStoreMain`, Discord RPC
- **Compilation**: `evalmachine.<anonymous>` (vm.Script), not a module
