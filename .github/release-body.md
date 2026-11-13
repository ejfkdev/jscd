**jscd @VERSION@** — reverse bytenode-compiled `.jsc` (V8 code cache) back to JavaScript.
Static parsing: no patched V8, no Node runtime, no subprocesses.

### What's new in 0.3.3

- **Large bundles now decompile correctly, not just quickly.** Scopes with 75+ context-local
  names use V8's `NameToIndexHashTable`; jscd read its capacity and entry slots at the wrong
  offsets, so large module bundles produced `__ctx.ctxN` placeholders and `ReferenceError`s at run
  time. The hash-table layout is fixed, including `Repeat(undefined)` empty slots. A webpack-shaped
  synthetic bundle (5,000 modules, 300 top-level variables) now produces output whose stdout is
  byte-identical to the original program.
- A new 80-variable fixture (`tests/fixtures/jsc/largescope.jsc`) guards the hash-table path.
- 0.3.2's huge-function speedup and `--skip-oversized` / `--continue-on-error` options are included:
  back edges/jump targets are computed once per function (465 KB function: 35.2 s → 2.34 s).
- Verified: **1,144 pass / 0 fail / 8 skip** across 32 Node lines × 36 fixtures plus **160 whole
  scripts**, 67 cargo tests, clippy clean.

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