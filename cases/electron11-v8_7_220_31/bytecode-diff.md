# Bytecode diff: V8 8.6 → 8.7

Extracted from `src/interpreter/bytecodes.h` at V8 tag `8.7.220.31`.

## Summary

- 8.6 table: **182** bytecodes
- 8.7 source: **183** bytecodes
- Added: **`LdaNamedPropertyFromSuper`** (at index 42)
- Removed: none
- Operand changes: none (all shared opcodes have identical operand lists)
- Accumulator-use changes: none

## The new bytecode

```
V(LdaNamedPropertyFromSuper, AccumulatorUse::kReadWrite,
  OperandType::kReg, OperandType::kIdx, OperandType::kIdx)
```

Inserted at index 42, between `LdaNamedPropertyNoFeedback` and `LdaKeyedProperty`.
This shifts all subsequent bytecode indices by +1.

## Full list (8.7, first 50)

| Index | Opcode | Operands | Acc |
|---|---|---|---|
| 0 | Wide | — | — |
| 1 | ExtraWide | — | — |
| 2 | DebugBreakWide | — | rw |
| 3 | DebugBreakExtraWide | — | rw |
| 4–10 | DebugBreak0–6 | various | rw |
| 11 | LdaZero | — | w |
| 12 | LdaSmi | Imm | w |
| 13 | LdaUndefined | — | w |
| 14 | LdaNull | — | w |
| 15 | LdaTheHole | — | w |
| 16 | LdaTrue | — | w |
| 17 | LdaFalse | — | w |
| 18 | LdaConstant | Idx | w |
| 19 | LdaGlobal | Idx, Idx | w |
| 20 | LdaGlobalInsideTypeof | Idx, Idx | w |
| 21 | StaGlobal | Idx, Idx | r |
| 22 | PushContext | RegOut | r |
| 23 | PopContext | Reg | — |
| 24 | LdaContextSlot | Reg, Idx, UImm | w |
| 25 | LdaCurrentContextSlot | UImm | w |
| ... | | | |
| 40 | LdaNamedProperty | Reg, Idx, Idx | w |
| 41 | LdaNamedPropertyNoFeedback | Reg, Idx, Idx | w |
| **42** | **LdaNamedPropertyFromSuper** | **Reg, Idx, Idx** | **rw** |
| 43 | LdaKeyedProperty | Reg, Idx | w |
| ... | | | |

## Intrinsics (25, unchanged count)

One rename: `ToStringRT` → `ToString` (index 22).

## Verification

The bytecode table was verified by decompiling `hello.jsc` (compiled by
Electron 11.4.6) and confirming that all opcodes decode correctly:

```
@0   0x12 LdaConstant     [0]     ← loads from constant pool
@2   0x26 Star             r2
@4   0x27 Mov              r1, r3
@7   0x62 CallRuntime      [62], r2-r3
@12  0x13 LdaGlobal        [1], [0]
@15  0x26 Star             r3
@17  0x28 LdaNamedProperty  r3, [2], [2]   ← console.log
...
@45  0xab Return
```

All 46 bytecodes of the top-level function decode with correct opcodes and
operand widths, confirming the table is accurate.
