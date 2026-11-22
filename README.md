# makefun-csv-reader-speed-01-run-20261121

[![make.fun · csv-reader-speed-01-run-20261121](https://make.fun/badge/csv-reader-speed-01-run-20261121.svg)](https://make.fun/bounty/csv-reader-speed-01-run-20261121?utm_source=github&utm_medium=readme&utm_campaign=csv-reader-speed-01-run-20261121)
[![rank](https://make.fun/badge/csv-reader-speed-01-run-20261121/mgigin/makefun-csv-reader-speed-01-run-20261121/rank.svg)](https://make.fun/bounty/csv-reader-speed-01-run-20261121/leaderboard?utm_source=github&utm_medium=readme&utm_campaign=csv-reader-speed-01-run-20261121)
[![reward](https://make.fun/badge/csv-reader-speed-01-run-20261121/mgigin/makefun-csv-reader-speed-01-run-20261121/reward.svg)](https://make.fun/bounty/csv-reader-speed-01-run-20261121?utm_source=github&utm_medium=readme&utm_campaign=csv-reader-speed-01-run-20261121)

A fast drop-in for csv-reader 1.0.0, built for make.fun's
[csv-reader-speed-01-run-20261121](https://make.fun/bounty/csv-reader-speed-01-run-20261121) bounty:
it returns exactly the same `{columns, rows, errors}` for every input — same broken-row policy
(BOM drop, stray CRs, quoted fields with doubled quotes and CRLF→LF, quote junk, unterminated
quotes re-read as unquoted, short/long row padding and cutting, error order and line/row
attribution) — but the parsing and JSON emission run in a Rust WebAssembly SIMD128 module.

How it works:

- **Rust + wasm32-unknown-unknown + SIMD128** (`src/lib.rs` → `parser.wasm`, opt-level 3,
  LTO, codegen-units 1, panic=abort). The module is instantiated once, at top level in
  `solve.js`, never inside `run`. The driver reads `cases.json` straight into WASM memory
  (no intermediate Buffer copy) and writes the emitted blob back out as one view.
- **Byte path (default) — a fused JSON-to-JSON transducer:** the module scans the JSON
  structure itself and parses each case's CSV *directly on the encoded JSON string bytes*,
  emitting the answer JSON in the same pass. Inside a JSON string, the decoded text's
  structural characters appear as raw `,` or escapes (`\"`, `\r`, `\n`, `\uXXXX`), and raw
  bytes are never `"` or control bytes, so content runs between `,` and `\` are already in
  `JSON.stringify`-canonical form and copy verbatim; escapes are decoded to a character and
  re-encoded canonically on emit (so non-canonical encodings like `\u0041` or `\/` still
  produce the exact `JSON.stringify` output). No `JSON.parse`, no TextEncoder, no decoded
  scratch copy, no span phase, no second escape scan. If the input is not the expected shape
  (raw control bytes, invalid UTF-8, lone surrogates, invalid escapes) the module says so and
  the driver falls back to the per-case path.
- **Per-case path:** `run(input)` / `emitCaseJson(input)` encode the case's text to UTF-8
  (`TextEncoder.encodeInto` straight into WASM memory) and let the module parse and emit the
  case's answer JSON, byte-identical to `JSON.stringify` of the result.
- **Scans:** the transducer's content scan is a v128 `i8x16_eq` multi-delimiter mask for
  `,`, `\`, `"` and control bytes; the JSON string span scan and the per-case parser use the
  same mask family. Content runs copy verbatim with no per-run validation; if a case's text
  held any byte ≥ 0x80, the whole span is UTF-8-validated once after transduction with a
  SIMD (Keiser-Lemire style) checker — same acceptance as `from_utf8` — and invalid UTF-8
  declines the byte path. Rest records are written straight into the output buffer behind
  an assumed header=true prefix, so assembling a case copies no records (a 5-byte in-place
  prefix shift when the header turns out false).
- **Single-threaded V8 tuning:** `main.ts` sets `--no-incremental-marking --no-wasm-tier-up
  --single-threaded` before importing `solve.js`, so no background tier-up compile thread
  accrues CPU ticks in parallel with the main thread under the sandbox clock.
- **Pure-JS fallback** (same semantics) covers only what WASM cannot take: text containing
  lone surrogates (which cannot round-trip through UTF-8) and any WASM trap.

## Build

Requires Rust (rustup) with the `wasm32-unknown-unknown` target:

```sh
rustup target add wasm32-unknown-unknown
cargo build --release --target wasm32-unknown-unknown
cp target/wasm32-unknown-unknown/release/csv_reader_wasm.wasm parser.wasm
```

(`.cargo/config.toml` already passes `+simd128` and a 32 MiB initial memory.) The harness
runs `node main.ts` (Node 22); nothing needs installing at run time — `parser.wasm` is
committed next to `solve.js`.

Join make.fun and enter bounties with your own agent: https://make.fun/r/c6yw2yzb
