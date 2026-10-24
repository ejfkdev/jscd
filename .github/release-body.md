**jscd @VERSION@** — reverse bytenode-compiled `.jsc` (V8 code cache) back to JavaScript.
Static parsing: no patched V8, no Node runtime, no subprocesses.

### What's new in 0.2.0

- **Electron 19 → 45** (V8 10.2 – 15.6) is now covered, including pointer-compressed and
  V8-sandboxed builds. Every major from 20 to 45 has a table calibrated against that build's
  own `--print-bytecode` / `--trace-serializer` output and passes the 9-fixture behavior
  check 9/9.
- Newer Node releases through **26.12.1** (V8 14.6) are covered; version hashes that collide
  between Node and Electron builds (14.6.202.34, 13.6.233.17, 12.4.254.20, …) are resolved
  by trying every table for the V8 minor and scoring the parsed bytecode.
- Robustness: malformed or mismatched caches now fail with an error instead of looping —
  array/scope name counts are bounded by the payload size and context chains detect cycles.
- 543 manifest entries (517 Node + 26 Electron), 61 version tables.

### Install

```sh
brew install ejfkdev/tap/jscd                                        # macOS / Linux
scoop bucket add ejfkdev https://github.com/ejfkdev/scoop-bucket && scoop install jscd   # Windows
cargo binstall jscd                                                  # anywhere with Rust (cargo-binstall)
```

Or take a bare binary straight from this release (no archive, no installer — the file *is* the
executable):

| file | platform | build notes |
| --- | --- | --- |
| `jscd-@VERSION@-linux-amd64` | Linux x86_64 | static (musl) |
| `jscd-@VERSION@-linux-arm64` | Linux arm64 | glibc, cross-built |
| `jscd-@VERSION@-macos-arm64` | macOS (Apple Silicon) | |
| `jscd-@VERSION@-macos-amd64` | macOS (Intel) | |
| `jscd-@VERSION@-windows-amd64.exe` | Windows x86_64 | |
| `jscd-@VERSION@-windows-arm64.exe` | Windows arm64 | |

Linux and Windows builds are UPX-compressed (`--best --lzma`, best effort — a build
whose UPX step fails is still uploaded uncompressed). macOS binaries are not: UPX
does not support Mach-O.

### Quick start

```bash
chmod +x jscd-@VERSION@-linux-amd64
./jscd-@VERSION@-linux-amd64 app.jsc                 # decompile one file to stdout
./jscd-@VERSION@-linux-amd64 dist/                  # dist/**.jsc -> dist-out/**.js
./jscd-@VERSION@-linux-amd64 help decompile          # per-subcommand help, with examples
```

### Verify it yourself

```bash
./jscd-@VERSION@-linux-amd64 info app.jsc                    # header + detected Node/V8 version
./jscd-@VERSION@-linux-amd64 --verify app.jsc -o app.js      # output must pass `node --check`
```

Supported Node/V8 versions, the fixture list and how the test numbers are produced
(31 Node.js lines × 36 behavior fixtures, whole-script diffs, the sample corpus, Electron 20–45) are
documented in the README's **Verification** section.

<!-- GitHub's automatically generated release notes are appended below this description. -->