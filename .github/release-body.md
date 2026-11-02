**jscd @VERSION@** — reverse bytenode-compiled `.jsc` (V8 code cache) back to JavaScript.
Static parsing: no patched V8, no Node runtime, no subprocesses.

### What's new in 0.3.1

- **Dependencies refreshed.** cure 0.2.0 → **0.4.2** (same `parse_js` → `simplify_unit` →
  `print_module` pipeline; TS/JS gains IIFE inlining and freezes line-level directives such as
  `@ts-ignore` / `/*#__PURE__*/`). clap 4.6.7, serde 1.0.229, serde_json 1.0.154, brotli 9.1.1,
  windows-sys 0.61.2. No output regressions: **1,144 pass / 0 fail / 8 skip** across 32 Node
  lines × 36 fixtures, plus **160 whole scripts**.
- **Node 26.13.0** (V8 14.6.202.34), **24.22.2** (13.6.233.17) and **22.23.4** (12.4.254.21)
  are recognised; the manifest now lists **519 Node releases**. 26.13.0 and 24.22.2 pass
  36/36 + 5/5 out of the box. 22.23.4 shares its V8 version string with 22.12.0 but comes from a
  different `-node.NN` V8 build, so read-only-heap addresses shift; generate a table with
  `jscd ro-map` for that build (with it, 36/36 + 5/5 pass).
- **Electron 45.2.1** (V8 15.6.75.25) and **44.10.1** (15.2.124.45) are covered using hashes read
  from real caches; the layout is unchanged from 45.0.0/44.9.3, so the existing tables apply.
  **Electron 43.8.6** is labelled (same V8 as 43.8.3).
- **bytenode 2.1 Electron modes.** Caches produced inside an Electron **main** or **renderer**
  process (`compileElectronMainCode` / `compileElectronRendererCode`, required by bytenode for
  Electron ≥ 42) use the same payload layout and version hash as `ELECTRON_RUN_AS_NODE` output;
  only the read-only-snapshot checksum differs. All three modes decompile and run; two new
  fixtures pin this.
- **Robustness:** when an unknown version hash falls back to trying every table, a legacy
  (≤ V8 8.4) candidate used to trust a bogus reservation count read out of the modern header,
  allocating gigabytes and looping for minutes. Reservation tables are now bounds-checked:
  the Electron 45.2.1 renderer case went from **120 s / 1.4 GB to 0.28 s**.
- 0.3.0's **swc → cure** swap remains: statement-per-line output, no renaming, ~6.4 MB binary.
- Manifest: **549 entries (519 Node + 30 Electron), 62 version tables**.

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