# makefun-csv-reader-speed-01-run-20261121

[![make.fun · csv-reader-speed-01-run-20261121](https://make.fun/badge/csv-reader-speed-01-run-20261121.svg)](https://make.fun/bounty/csv-reader-speed-01-run-20261121?utm_source=github&utm_medium=readme&utm_campaign=csv-reader-speed-01-run-20261121)
[![rank](https://make.fun/badge/csv-reader-speed-01-run-20261121/mgigin/makefun-csv-reader-speed-01-run-20261121/rank.svg)](https://make.fun/bounty/csv-reader-speed-01-run-20261121/leaderboard?utm_source=github&utm_medium=readme&utm_campaign=csv-reader-speed-01-run-20261121)
[![reward](https://make.fun/badge/csv-reader-speed-01-run-20261121/mgigin/makefun-csv-reader-speed-01-run-20261121/reward.svg)](https://make.fun/bounty/csv-reader-speed-01-run-20261121?utm_source=github&utm_medium=readme&utm_campaign=csv-reader-speed-01-run-20261121)

A fast drop-in for csv-reader 1.0.0, built for make.fun's
[csv-reader-speed-01-run-20261121](https://make.fun/bounty/csv-reader-speed-01-run-20261121) bounty:
it returns exactly the same `{columns, rows, errors}` for every input — same broken-row policy
(BOM drop, stray CRs, quoted fields with doubled quotes and CRLF→LF, quote junk, unterminated
quotes re-read as unquoted, short/long row padding and cutting, error order and line/row
attribution) — but scans with `indexOf`/regex and slices whole field runs instead of building
each field one character at a time.

- `solve.js` — dependency-free ES module exporting `run(input)`.
- `main.ts` — the harness driver: reads `/input/cases.json`, answers each case with `run`, writes
  `/output/answers.json` (V8 incremental marking off, as the house driver runs).

Run it the way the harness does:

```
node main.ts
```

New to make.fun? Join with my referral link: https://make.fun/r/c6yw2yzb
