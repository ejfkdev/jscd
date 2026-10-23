# Bug: infinite recursion in jscd's emit_range on V8 8.7 functions

## Location
`src/decompile.rs`, function `emit_range_inner` (line ~6329) and its recursive callees.

## Symptom
Decompiling `main.jsc` (12.7MB, 224K objects, 7253 SFIs) crashes with stack overflow
on SFI 60944 (673 bytecodes, 0 handler table entries, frame_size=104).

The crash happens inside `ctx.run()` → `emit_range(0, 673)` → `emit_range_inner`
which recursively calls `emit_range` for detected control-flow patterns (loops,
try/catch, switch/case). The recursion never terminates.

## Key facts
1. The function has **0 handler table entries** — try/catch/finally detectors
   should all return None immediately.
2. The crash is in the **loop detection** path: `find_loop(i, end)` at line ~7097
   detects a backward jump and calls `emit_range(i+1, back_idx)` for the loop body.
3. I added cycle detection (HashSet of (i, end) pairs in emit_range) but it
   didn't help — the recursion may go through a different path.

## The find_loop function (line ~7097)
```rust
fn find_loop(&self, i: usize, end: usize) -> Option<(usize, usize)> {
    let head = self.instrs[i].offset;
    let mut back = None;
    for j in i + 1..end.min(self.instrs.len()) {
        let ins = &self.instrs[j];
        let Some(t) = self.uncond_jump_target(ins)
            .or_else(|| self.cond_jump_target(ins))
        else { continue; };
        if t == head && (ins.name.starts_with("JumpLoop") || t < ins.offset) {
            back = Some(j);
        }
    }
    let back = back?;
    let exit = self.instrs.get(back + 1).map(|x| x.offset).unwrap_or(usize::MAX);
    Some((back, exit))
}
```

## The emit_range function (line ~6314)
```rust
fn emit_range(&mut self, i: usize, end: usize) -> Result<(), String> {
    if self.emit_depth > 80 {
        self.line("/* too deep */");
        return Ok(());
    }
    self.emit_depth += 1;
    let r = self.emit_range_inner(i, end);
    self.emit_depth -= 1;
    r
}
```

## emit_range_inner call chain (line ~6329)
The while loop in emit_range_inner tries pattern detectors in order:
1. `try_emit_try_finally(i, end)` → calls `emit_range` for try/catch/finally bodies
2. Handler-based try/catch (inline) → calls `emit_range` for body and catch
3. `try_emit_null_guard(i, end)` → may call emit_range
4. `find_loop(i, end)` → calls `emit_range(i+1, back_idx)` for loop body
5. `try_emit_for_in(i, end)` → calls emit_range
6. `try_case_chain(i, end)` → calls emit_range for case bodies
7. Conditional jump → may call emit_range for branches

Each detector returns `Option<usize>` (the next instruction to continue from).
If a detector returns `Some(next)` where `next <= i`, the loop doesn't advance.

## What I tried
1. Changed `t <= ins.offset` to `t < ins.offset` in find_loop → didn't fix
2. Added cycle detection (HashSet of (i,end)) in emit_range → didn't fix
3. Added emit_depth > 80 guard → still overflows 4GB stack
4. Added call_count > 100000 guard → still overflows

## What to investigate
1. Which pattern detector matches on the 673-bytecode function and creates the cycle?
2. Is the recursion through emit_range, or through a different function that
   calls emit_range without going through the cycle guard?
3. Does `try_emit_null_guard` or `try_case_chain` have their own recursive
   paths that bypass emit_range's depth/cycle guards?

## How to reproduce
```bash
cd /tmp/jscd
cargo build --release
JSCD_TS=4 JSCD_NO_SWC=1 ./target/release/jscd ~/Downloads/main.jsc
# Crashes with: thread '<unknown>' has overflowed its stack
```

The ground-truth hello.jsc (472 bytes) works fine. Only the large file crashes.
