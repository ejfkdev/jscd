# Contributing

Issues and pull requests are welcome — in English or Chinese.

## Build

```sh
cargo build --release        # -> target/release/jscd
cargo test                   # unit + integration tests (56)
```

Rust **1.96+** (the swc-based AST layer needs a recent `rustc`). `cargo fmt` is **not**
enforced: the code is hand-formatted (long explanatory comments, aligned tables) — please match
the surrounding style rather than reformatting wholesale.

## Gates a change has to pass

- `cargo test` — unit and integration tests.
- `cargo clippy --all-targets -- -D warnings` — the project keeps **zero warnings**; a plain
  `cargo build` must be silent too.
- `scripts/ci_smoke.sh` — compiles every fixture to a real `.jsc` with the `node` on `PATH`,
  decompiles it, gates the product with `node --check`, and byte-compares the 5 whole-script
  fixtures against their originals. No npm packages needed.
- If you touched version handling, run the behaviour matrix for at least the affected Node lines:
  `bash scripts/verify_behavior.sh 20.20.2` (Node versions come from `mise`; see the README's
  Verification section for the full matrix and its numbers).

## Adding a fixture

1. **Behaviour fixture** — a file in `tests/fixtures/behav/` whose source defines a top-level
   `function target(...)` (see the existing ones), plus an entry in
   `tests/fixtures/behav/cases.json`:

   ```json
   "my_case": { "cases": [[1, 2], ["a", "b"]] }
   ```

   `cases` is the list of argument lists `target` is called with; the returned values are compared
   against the original. Two optional keys: `min_node` (major version whose parser the syntax
   needs — the cell is skipped below it, e.g. `?.` needs 14) and `same_node_original` (run the
   original with the *same* Node version as the artifact instead of the host's V8 — use it when the
   case's native semantics differ between V8 versions).
2. **Whole-script fixture** — a file in `tests/fixtures/scripts/`; it is picked up automatically
   and its stdout + exit code are compared against the original script.

Fixtures are compiled on the fly by `scripts/mkcorpus.js` (bytenode's exact flags +
`createCachedData()`); no `.jsc` is checked in. The fixture list is read from `cases.json`, so a
new entry joins the matrix automatically.

## Notes

- `docs/VERSIONS.md` — per-V8-version engineering notes (Chinese), the place to record a new
  version finding.
- `docs/stream-format.md` — the serialized-payload format notes.
- Version numbers: `build.rs` injects the git tag as `jscd --version`; releases are cut with
  `scripts/release.sh vX.Y.Z`, which requires `Cargo.toml` to carry the same version.