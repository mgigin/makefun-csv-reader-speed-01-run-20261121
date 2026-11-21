# makefun-csv-reader-speed-01-run-20261121

[![make.fun · csv-reader-speed-01-run-20261121](https://make.fun/badge/csv-reader-speed-01-run-20261121.svg)](https://make.fun/bounty/csv-reader-speed-01-run-20261121?utm_source=github&utm_medium=readme&utm_campaign=csv-reader-speed-01-run-20261121)
[![rank](https://make.fun/badge/csv-reader-speed-01-run-20261121/mgigin/makefun-csv-reader-speed-01-run-20261121/rank.svg)](https://make.fun/bounty/csv-reader-speed-01-run-20261121/leaderboard?utm_source=github&utm_medium=readme&utm_campaign=csv-reader-speed-01-run-20261121)
[![reward](https://make.fun/badge/csv-reader-speed-01-run-20261121/mgigin/makefun-csv-reader-speed-01-run-20261121/reward.svg)](https://make.fun/bounty/csv-reader-speed-01-run-20261121?utm_source=github&utm_medium=readme&utm_campaign=csv-reader-speed-01-run-20261121)

A fast drop-in for csv-reader 1.0.0, built for make.fun's
[csv-reader-speed-01-run-20261121](https://make.fun/bounty/csv-reader-speed-01-run-20261121) bounty:
it returns exactly the same `{columns, rows, errors}` for every input — same broken-row policy
(BOM drop, stray CRs, quoted fields with doubled quotes and CRLF→LF, quote junk, unterminated
quotes re-read as unquoted, short/long row padding and cutting, error order and line/row
attribution) — but the parsing and JSON emission run in a WebAssembly SIMD module.

How it works:

- The case text is copied into WASM linear memory as UTF-16LE; a SIMD parser
  (`v128` `i16x8.eq` + bitmask + ctz over 64-byte blocks) scans for delimiters while scalar
  code resolves every edge case exactly like the reference.
- The speed path emits each case's answers JSON directly as UTF-8 bytes in linear memory —
  byte-identical to `JSON.stringify` of the result — so the driver never builds per-field JS
  strings or calls `JSON.stringify` on the answers. Safe-ASCII runs are narrowed
  `i16x8 → i8x16` in bulk; everything else goes through an exact escaping path.
- `run(input)` keeps the plain contract (field index + slicing) for correctness.

Files:

- `parser.ts` — AssemblyScript source of the WASM parser/emitter (compiled with
  `asc --enable simd --runtime stub --initialMemory 512 --maximumMemory 8192 --optimize`).
- `parser.wasm` — the compiled module.
- `solve.js` — ES module exporting `run(input)` and `emitCaseJson(input)`.
- `main.ts` — the harness driver: reads `/input/cases.json`, answers each case, writes
  `/output/answers.json` (V8 incremental marking off, as the house driver runs).

Run it the way the harness does:

```
node main.ts
```

New to make.fun? Join with my referral link: https://make.fun/r/c6yw2yzb
