# V8 8.7.220.31 vs 8.6: Serialization Tag Enum Diff

Extracted from `src/snapshot/serializer-deserializer.h` at V8 tag `8.7.220.31`,
compared against jscd's 8.6 table (`tables/v8_6_15.14.0.json`).

## The shift

V8 8.7 **removed `kDeferred`** and **added `kRegisterPendingForwardRef`,
`kResolvePendingForwardRef`, `kNewMetaMap`**. Everything from `kAlignmentPrefix`
onward shifted down by 1.

## Full 8.7 tag enum

| Value | Tag | Notes |
|---|---|---|
| 0x00–0x04 | `kNewObject` | 5 spaces (kReadOnlyHeap, kOld, kCode, kMap, kNew) |
| 0x08–0x0c | `kBackref` | 5 spaces |
| 0x10 | `kStartupObjectCache` | |
| 0x11 | `kRootArray` | |
| 0x12 | `kAttachedReference` | index 0 = source string placeholder (bytenode) |
| 0x13 | `kReadOnlyObjectCache` | |
| 0x14 | `kNop` | |
| 0x15 | `kNextChunk` | |
| 0x16–0x18 | `kAlignmentPrefix` | 3 alignment values |
| 0x19 | `kSynchronize` | |
| 0x1a | `kVariableRepeat` | |
| 0x1b | `kOffHeapBackingStore` | |
| 0x1c | `kEmbedderFieldsData` | |
| 0x1d | `kVariableRawCode` | |
| 0x1e | `kVariableRawData` | |
| 0x1f | `kApiReference` | |
| 0x20 | `kExternalReference` | |
| 0x21 | `kSandboxedApiReference` | |
| 0x22 | `kSandboxedExternalReference` | |
| 0x23 | `kInternalReference` | |
| 0x24 | `kClearedWeakReference` | |
| 0x25 | `kWeakPrefix` | |
| 0x26 | `kOffHeapTarget` | |
| 0x27 | `kRegisterPendingForwardRef` | **NEW in 8.7** — does NOT read putint |
| 0x28 | `kResolvePendingForwardRef` | **NEW in 8.7** — reads putint (index) |
| 0x29 | `kNewMetaMap` | **NEW in 8.7** |
| 0x40–0x5f | `kRootArrayConstants` | 32 root shortcuts |
| 0x60–0x7f | `kFixedRawData` | 1–32 tagged slots |
| 0x80–0x8f | `kFixedRepeat` | 2–17 repeats |
| 0x90–0x97 | `kHotObject` | 8 hot ring entries |

## Critical bug: `kRegisterPendingForwardRef`

V8 8.7's deserializer (`src/snapshot/deserializer.cc`):

```cpp
case kRegisterPendingForwardRef: {
    DCHECK_NE(current_object_address, kNullAddress);
    HeapObject obj = HeapObject::FromAddress(current_object_address);
    HeapObjectReferenceType ref_type = GetAndResetNextReferenceType();
    unresolved_forward_refs_.emplace_back(
        obj, current.address() - current_object_address, ref_type);
    num_unresolved_forward_refs_++;
    return current + 1;  // advances 1 slot, NO stream data consumed
}
```

jscd's original handler read a `putint()` from the stream, consuming 1–4 extra
bytes. This caused the parser to desynchronize at ~offset 977K in `main.jsc`.

**Fix:** `SlotValue::PendingRef(0)` — no stream data consumed.
