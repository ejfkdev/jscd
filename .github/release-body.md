**jscd @VERSION@** — reverse bytenode-compiled `.jsc` (V8 code cache) back to JavaScript.
Static parsing: no patched V8, no Node runtime, no subprocesses.

### What's new in 0.3.2

- **Huge functions no longer stall `decompile`.** Loop detection used to rescan the whole function
  for every instruction (O(n²)); back edges and jump targets are now computed once per function.
  A synthetic 465 KB function went from **35.2 s to 2.34 s** with byte-identical output; a 3.4 MB /
  12,002-function bundle finishes in **3.5 s**. This addresses
  [issue #4](https://github.com/ejfkdev/jscd/issues/4).
- **`--skip-oversized <N>`** emits a stub for functions whose bytecode is longer than N bytes, and
  **`--continue-on-error`** emits a stub and keeps going when a single function fails, so one
  pathological function no longer costs the whole file. Stubs are `function name() { /* [jscd] … */ }`;
  `--runtime` keeps the full comment, the default output keeps the function shell.
- Fixed a latent clap debug assertion in the `decompile` subcommand: the positional `OUTPUT`
  argument and `-o/--output` shared the same id (the positional is now `dest`).
- 0.3.1's dependency refresh remains: cure 0.4.2, brotli 9.1.1, windows-sys 0.61.2, clap 4.6.7.
- Verified: **1,144 pass / 0 fail / 8 skip** across 32 Node lines × 36 fixtures plus **160 whole
  scripts**, 66 cargo tests, clippy clean.

### Install

```sh
brew install ejfkdev/tap/jscd                                        # macOS / Linux
scoop bucket add ejfkdev https://github.com/ejfkdev/scoop-bucket && scoop install jscd   # Windows
cargo binstall jscd                                                  # anywhere with Rust (cargo-binstall)
npm install -g @ejfkdev/jscd                                         # any platform with Node 18+
winget install --id ejfkdev.jscd -e                                  # Windows (once the manifest is merged)
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
(32 Node.js lines × 36 behavior fixtures, whole-script diffs, the sample corpus, Electron 20–45) are
documented in the README's **Verification** section.

<!-- GitHub's automatically generated release notes are appended below this description. -->