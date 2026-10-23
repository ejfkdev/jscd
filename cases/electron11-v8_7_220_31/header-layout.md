# Header layout: V8 8.7 SerializedCodeData

Extracted from `src/snapshot/code-serializer.h` at V8 tag `8.7.220.31`.

## Layout (pointer compression, kTaggedSize=4)

```
Offset  Field                Type     Notes
──────  ───────────────────  ──────── ──────────────────────────────────────
0       magic                uint32   0xC0DE0000 | ExternalReferenceTable::kSize
4       version_hash         uint32   Version::Hash() (right-fold for ≤11.x)
8       source_hash         uint32   source_length | (is_module << 31)
12      flag_hash           uint32   FlagList::Hash()
16      num_reservations    uint32   number of reservation entries
20      payload_length      uint32   serialized payload byte count
24      checksum            uint32   Adler-32 of payload
28      (alignment padding)           POINTER_SIZE_ALIGN(28) = 32
32      reservations[num]    uint32[] each = (space | chunk_size)
32+N    payload[payload_len] byte[]  the serialized object graph
```

## Key differences from 8.6 table

The 8.6 table (`v8_6_15.14.0.json`) was generated for Node 15 (V8 8.6.395.17,
non-compressed, kTaggedSize=8). Its header layout has `payload_length@16` and
`checksum@20` with `header_size=24`.

V8 8.7 (Electron 11, Chromium 87, pointer compression ON) has:
- `num_reservations@16` (NEW field — was absent or at different offset in 8.6)
- `payload_length@20` (shifted by +4)
- `checksum@24` (shifted by +4)
- `header_size=32` (POINTER_SIZE_ALIGN(28) = 32, was 24)

## Reservations

V8 8.7's `SerializedCodeData` includes a reservation table immediately after
the 28-byte (aligned to 32) header. Each entry is a uint32 encoding
`(SnapshotSpace | chunk_size_in_bytes)`.

For the ground-truth `hello.jsc` (472 bytes):
- `num_reservations = 5`
- Reservations: `[0x80000000, 0x80000094, 0x80000000, 0x80000000, 0x80000000]`
- Payload starts at: `32 + 5*4 = 52`, aligned to `56`
- Payload length: `416` bytes
- Total: `56 + 416 = 472` ✓

## Magic number

`0xC0DE0000 | ExternalReferenceTable::kSize`

For Electron 11: `0xC0DE03D2` → `ExternalReferenceTable::kSize = 0x3D2 = 978`.

This matches `main.jsc` exactly (first 4 bytes: `D2 03 DE C0` in little-endian).

## Version hash

`0x02F91115` → computed by V8's `Version::Hash()` using the right-fold
algorithm (`hash_combine(0, patch, build, minor, major)` with patch first).

jscd's `brute_force()` correctly identifies this as V8 `8.7.220.31`.
