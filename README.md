# jscd

Reverse [bytenode](https://github.com/bytenode/bytenode)-compiled `.jsc` (V8 code cache) files back to JavaScript.

`jscd` statically parses the V8 serialized code cache — no patched V8 binaries, no Node runtime, single self-contained executable. See [README.zh.md](README.zh.md) for the Chinese documentation.

> **Reality check**: the source text is never stored inside a `.jsc` (V8 strips it at serialize time; the `source hash` header field is just the original source *length*). What `jscd` recovers is everything that *is* in the file: metadata, string constants, function tree, bytecode disassembly, and a reconstructed pseudo-JS that is syntactically valid and aims to be runnable. Variable names that survive only as scope slots are reported under slot names.

## Install

```sh
cargo install jscd   # or build from source: cargo build --release
```

## Usage

```sh
jscd info app.jsc         # header fields, detected Node/V8 version
jscd strings app.jsc      # string constants / symbol tables
jscd functions app.jsc    # function (SharedFunctionInfo) tree
jscd disasm app.jsc       # View8-compatible bytecode disassembly
jscd decompile app.jsc    # reconstructed JavaScript (default: `jscd app.jsc`)
jscd --json info app.jsc  # machine-readable output
```

## Supported versions

Node 8 (V8 5.8) through current Node, keyed on V8 versions rather than Node majors.
Version tables are generated from the Node source tree by `scripts/codegen.py` and embedded at compile time.

## How it works

1. **header** — parse the 28-byte `SerializedCodeData` header (magic, version hash, source hash, flag hash, read-only snapshot checksum, payload length, checksum); detect and transparently decompress Brotli-wrapped (`bytenode --compress`) files.
2. **identify** — map `version hash` → V8 version via an embedded 90-entry index (one per official Node↔V8 pairing); unknown hashes are brute-forced with the recovered `Version::Hash()` algorithm.
3. **deserialize** — parse the V8 serializer payload: SharedFunctionInfo tree, constant pools, bytecode arrays, source position tables, handler tables.
4. **disassemble** — table-driven opcode decoding (per-V8 operand sizes/scales extracted from `bytecodes.h`).
5. **decompile** — register IR → control-flow reconstruction → AST → printer; output is guaranteed syntactically valid.

## License

MIT
