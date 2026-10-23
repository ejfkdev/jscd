# main.jsc — file profile

**Note**: `main.jsc` is not included in this repository (12.7 MB, potentially
proprietary). This file documents its structure for reproducibility.

## File header

```
magic:           0xC0DE03D2  (external refs: 0x3D2 = 978)
version_hash:    0x02F91115  → V8 8.7.220.31 (Electron 11)
source_hash:     0x0085962C  → source length: 8,754,732 bytes, module: no
flag_hash:       0x59649371
num_reservations: (5 entries, same layout as hello.jsc)
payload_length:  12,756,952 bytes
checksum:        0x4432E065
```

## Structure

| Metric | Value |
|---|---|
| File size | 12,769,864 bytes |
| Header | 32 bytes (aligned) |
| Reservations | 5 entries (20 bytes) |
| Payload | 12,756,952 bytes starting at offset 56 |
| Objects parsed | 224,126 |
| SharedFunctionInfos | 7,253 |
| Strings extracted | 163,810 |

## Content (from string analysis)

The file is an Electron 11 application's main-process code cache, produced by
`vm.Script.createCachedData()`. Key characteristics:

- **Obfuscated**: javascript-obfuscator with `splitStrings` (9–10 char chunks),
  hex identifier mangling (`_0x47f30c`, `a0_0x2d98`)
- **System fingerprinting**: CPU models (i5-9600K, E3-1260L, FX-8370E, etc.),
  GPU/VRAM, BIOS, serial numbers, MAC addresses, PCI vendor/device IDs
- **Cross-platform**: Windows (`wmic`, PowerShell, HKLM registry),
  Linux (`/proc/cpuinfo`, `/etc/os-release`, `ipconfig`),
  macOS (`system_profiler`, `ioreg`)
- **Electron-specific**: `ElectronJSONSettingsStoreMain`, Discord RPC
- **Compilation**: `evalmachine.<anonymous>` frames (vm.Script, not a module)
- **Not bytenode**: No `kAttachedReference 0` source placeholder;
  `__dirname` present but `__filename` absent

## Known issues with this file

1. **Deferred section**: Last ~1.2 MB fails to parse (backref → RoRef).
   Requires a V8 8.7 ro-map to fix.
2. **5 large functions**: 673–4861 bytecodes each, skipped due to
   `emit_range_inner` recursion bug in jscd.
3. **SWC optimizer**: Must use `JSCD_NO_SWC=1` due to `DeleteLookupSlot`
   interaction.
