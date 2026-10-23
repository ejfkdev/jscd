# jscd internal architecture: what trips up new V8 versions

This document explains the non-obvious machinery in jscd that caused
bugs when adding V8 8.7 (Electron 11) support. Each section is a trap
that cost significant debugging time.

## 1. The version table is not just "opcodes + tags"

A `tables/v*.json` file has 16 fields. The ones that matter most for
correct decompilation (in order of impact):

| Field | What it controls | When wrong |
|---|---|---|
| `serialization.tags` | Wire-format tag → byte value mapping | Parser desyncs at first unknown tag |
| `tagged_size` | Pointer compression (4 or 8) | ALL field reads off by 2x |
| `capture_tagged_size` | How the table was generated | `bca_rt_off` converts offsets wrong |
| `header` | SerializedCodeData field offsets | Payload starts at wrong offset |
| `bytecode_array` | BytecodeArray object layout | Wrong frame_size, param_count, bytecode_len |
| `bytecodes` | Opcode list + operand widths | Wrong instructions decoded |
| `frame` | Interpreter register frame base | Wrong register names (r0 vs a0) |
| `shared_function_info` | SFI field offsets | Can't find BytecodeArray |
| `scope_info` | ScopeInfo flag bit positions | Wrong variable names / scope analysis |
| `roots` | Root array index → name | Wrong builtin/property names |
| `runtime_names` | Runtime function index → name | Wrong CallRuntime names |

**Trap**: You can't just copy the 8.6 table and change the version string.
The `serialization.tags` field is version-specific because V8 adds/removes
tags between minor versions. For V8 8.7, the `kDeferred` tag was removed,
shifting everything from `kAlignmentPrefix` onward down by 1. Using 8.6's
tag values causes the parser to read garbage at the first shifted tag.

## 2. `kRegisterPendingForwardRef` does NOT read stream data

**Location**: `src/serializer.rs`, `parse_ref_inner()`

V8 8.7 introduced `kRegisterPendingForwardRef` (tag 0x27). The deserializer
in `src/snapshot/deserializer.cc` does:

```cpp
case kRegisterPendingForwardRef: {
    HeapObject obj = HeapObject::FromAddress(current_object_address);
    unresolved_forward_refs_.emplace_back(
        obj, current.address() - current_object_address, ref_type);
    num_unresolved_forward_refs_++;
    return current + 1;  // advances 1 slot, NO GetInt() call
}
```

jscd's original handler called `self.putint()`, consuming 1-4 bytes from the
stream. Each occurrence ate 4 extra bytes (because the next byte 0x27 has
low 2 bits = 3, triggering 4-byte putint decoding). After 32K+ objects, the
accumulated error desynced the parser completely.

**Fix**: `SlotValue::PendingRef(0)` — no stream data consumed.

**General lesson**: When adding a new tag, check the V8 deserializer source
(`src/snapshot/deserializer.cc`) for whether it reads `source_.GetInt()` or
`source_.Get()`. Tags that just manipulate internal state (pending refs,
alignment, nop) don't consume stream data beyond the tag byte itself.

## 3. `bca_rt_off`: the capture_tagged_size conversion

**Location**: `src/serializer.rs`, `bca_rt_off()`

BytecodeArray field offsets in the table are stored in "capture units"
(the tagged size when the table was generated). At runtime, if the actual
tagged size differs (e.g., table generated at ts=8, runtime ts=4 for
pointer compression), `bca_rt_off` converts:

```rust
pub fn bca_rt_off(&self, table, ts, id, capture_off) -> usize {
    let cap = table.slot_unit();
    if ts == cap { return capture_off; }  // no conversion needed
    // Find the last Raw slot (the boundary between tagged and raw data)
    let tail = self.objects[id].slots.iter().rev()
        .find_map(|s| matches!(s.value, SlotValue::Raw(_)).then_some(s.index));
    match tail {
        Some(tail) => {
            let tail_cap = tail * cap;
            if capture_off >= tail_cap {
                tail * ts + (capture_off - tail_cap)  // in the raw tail
            } else {
                capture_off / cap * ts  // in the tagged slots
            }
        }
        None => capture_off / cap * ts,
    }
}
```

**Trap**: If the table has `capture_tagged_size=8` but the V8 build uses
pointer compression (ts=4), the tagged slot portion of the BytecodeArray
is HALF the size the table assumes. The raw tail (frame_size, param_count,
bytecodes) starts at a different offset. `bca_rt_off` handles this by
finding the last Raw slot as an anchor, but ONLY if the parser correctly
identified the raw slots during deserialization.

If the table's tags are wrong (see §1), the parser mis-identifies which
slots are Raw vs tagged, and `bca_rt_off` computes the wrong byte offset
for frame_size/param_count → the decompiler reads garbage values.

## 4. The SWC optimizer vs `DeleteLookupSlot`

**Location**: `src/opt_js.rs`, `src/cli.rs` (line ~538)

jscd has two optimization passes:
1. **Text-level** (`src/opt.rs`): regex-based copy propagation, dead store
   elimination. Works on the raw text output. Always runs.
2. **AST-level** (`src/opt_js.rs`): uses swc's minifier for proper copy
   propagation and dead code elimination. Only runs when NOT `--runtime`.

The swc optimizer wraps the code in a function (to get function-scope
optimization), runs swc's minifier, then unwraps. The minifier does
copy propagation: `r5 = "world"; r4 = greet(r5)` → `r4 = greet("world")`.

**Trap**: When the bytecode contains `CallRuntime DeleteLookupSlot` (V8's
module-wrapper code for `delete` in sloppy mode), the swc optimizer's
control flow analysis goes wrong and drops call arguments. The text-level
optimizer handles this correctly.

**Fix**: `JSCD_NO_SWC=1` env var to skip the swc pass entirely.

**General lesson**: The swc optimizer is a black box that can silently
corrupt output when it encounters unusual code patterns (runtime calls,
unusual control flow). Always compare `JSCD_NO_OPT=1` output with the
optimized output to verify correctness.

## 5. `find_loop`: backward jump detection

**Location**: `src/decompile.rs`, `find_loop()` (line ~7097)

The loop detector scans forward from instruction `i` looking for a jump
that targets `i`'s offset (the loop header). The original condition:

```rust
if t == head && (ins.name.starts_with("JumpLoop") || t <= ins.offset) {
    back = Some(j);
}
```

The `t <= ins.offset` check is supposed to catch backward jumps that
aren't `JumpLoop` (e.g., `Jump` to an earlier offset). But `<=` instead
of `<` means a jump to the SAME offset (self-loop) also matches, which
can create a zero-iteration range `(i, i)` that `emit_range` recurses
on infinitely.

**Fix**: Changed to `t < ins.offset` (strictly backward).

**However**: This alone doesn't fix the stack overflow on large functions.
The recursion also comes from other pattern detectors (try/catch, switch,
for-in) that call `emit_range` on sub-ranges. If a sub-range overlaps
with the current range, the recursion doesn't terminate.

**General lesson**: `emit_range` needs a visited-set guard to prevent
re-processing the same `(start, end)` range. The existing `emit_depth > 80`
guard only limits nesting depth, not total recursion through different
code paths.

## 6. The deferred section: `RoRef` instead of `Object`

**Location**: `src/serializer.rs`, deferred object parsing (line ~1240)

V8's code cache has a "deferred" section after the main payload. Objects
that were too large to serialize inline are completed here. The deferred
section contains `kNewObject+space | backref | size | slots...` entries.

**Trap**: In V8 8.7, some backrefs in the deferred section resolve to
`RoRef(chunk, offset)` (read-only heap references) instead of `Object(id)`.
This happens because the ro-map (read-only heap name table) hasn't been
generated for V8 8.7. The parser expects `Object(id)` backrefs and fails
with "deferred backref is not an object reference".

**Workaround**: The `legacy_tolerant` flag makes this non-fatal: the
deferred section is skipped, and decompilation proceeds with the 99% of
objects that were parsed in the main payload. Functions whose bodies
were in the deferred section will appear as `/* uncompiled */`.

**General lesson**: The ro-map is essential for resolving read-only heap
references. Without it, many string/builtin references in the constant
pools and SFI name fields will be empty. Generate it with
`scripts/build_ro_map.sh` using a probe `.jsc` compiled on the target V8.

## 7. `vm.Script` only compiles the top-level function

**Location**: affects ground-truth testing

When creating a `.jsc` with `vm.Script(src, {produceCachedData: true})`,
only the top-level function is compiled. Inner functions (like `greet()`
inside `console.log(greet("world"))`) are lazily compiled and appear as
"uncompiled" (`UncompiledData`) in the code cache.

**Trap**: If you use `--print-bytecode` to get ground-truth bytecodes,
you must RUN the script first (which triggers lazy compilation). The
`.jsc` file only contains the top-level bytecodes. This means:
- `jscd disasm hello.jsc` shows only the top-level function's bytecodes
- `jscd hello.jsc` shows `function greet() { /* uncompiled */ }`
- V8's `--print-bytecode hello.js` shows greet's bytecodes (after running)

This is NOT a bug — it's how V8's code cache works. The `.jsc` only
contains what was compiled at cache-creation time.

## 8. `capture_tagged_size` vs `tagged_size`

**Location**: `src/tables.rs`, `VersionTable` struct

These are two separate fields that must be consistent:

- `tagged_size`: the V8 build's pointer size (4 for compressed, 8 for not)
- `capture_tagged_size`: the tagged size when the TABLE was generated

For tables generated from Node source (which uses non-compressed V8 on
macOS): `capture_tagged_size = 8`. For V8 8.7 (Electron 11, Chromium 87
desktop, pointer compression ON): `tagged_size = 4`.

When they differ, `bca_rt_off` (see §3) converts between the two. When
they're the same, offsets are used as-is.

**Trap**: Setting `capture_tagged_size = 4` on a table that was generated
at ts=8 BREAKS the parser because `bca_rt_off` returns offsets as-is
(assuming ts == cap), but the table's offsets are in 8-byte units. The
correct approach is: keep `capture_tagged_size = 8` (matching how the
table was built) and let `bca_rt_off` handle the conversion to ts=4.
