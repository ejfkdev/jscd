**jscd @VERSION@** — reverse bytenode-compiled `.jsc` (V8 code cache) back to JavaScript.
Static parsing: no patched V8, no Node runtime, no subprocesses.

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
(31 Node.js lines × 36 behavior fixtures, whole-script diffs, the sample corpus) are
documented in the README's **Verification** section.

<!-- GitHub's automatically generated release notes are appended below this description. -->