// Driver: the harness runs `node /bot/main.ts` (node22). It reads every case from /input/cases.json (a JSON
// array of {id, input}), answers each one alone, and writes /output/answers.json: a JSON array of the
// answers, one per case and in the cases' order. Outside a container MAKEFUN_INPUT_DIR and
// MAKEFUN_OUTPUT_DIR stand in for /input and /output.
//
// Streamed mode (default): solve.js's emitCaseJson writes each case's answers JSON as UTF-8 bytes in WASM
// memory; this driver only frames the per-case blobs as a JSON array with writeSync — no per-field JS
// strings, no JSON.stringify of the answers. MAKEFUN_CSV_STREAMED=0 falls back to run() + JSON.stringify.

import { closeSync, openSync, readFileSync, writeFileSync, writeSync } from 'node:fs';
import { join } from 'node:path';
import { setFlagsFromString } from 'node:v8';

// V8's incremental marking off, set before solve.js is imported so all of its work runs under it (the house
// baseline runs with --no-incremental-marking, and an entry may tune V8's flags too).
setFlagsFromString('--no-incremental-marking');

/** solve.js, imported by its URL (a .js module carries no types) and typed by its contract. */
const { run, emitCaseJson } = (await import(new URL('./solve.js', import.meta.url).href)) as {
  run: (input: unknown) => unknown;
  emitCaseJson: (input: unknown) => { buffer: ArrayBuffer; ptr: number; len: number };
};

const INPUT_DIR = process.env.MAKEFUN_INPUT_DIR ?? '/input';
const OUTPUT_DIR = process.env.MAKEFUN_OUTPUT_DIR ?? '/output';

const cases = JSON.parse(readFileSync(join(INPUT_DIR, 'cases.json'), 'utf8')) as { id: string; input: unknown }[];

if (process.env.MAKEFUN_CSV_STREAMED === '0') {
  const answers: unknown[] = [];
  for (const c of cases) answers.push(run(c.input));
  writeFileSync(join(OUTPUT_DIR, 'answers.json'), JSON.stringify(answers));
} else {
  const fd = openSync(join(OUTPUT_DIR, 'answers.json'), 'w');
  writeSync(fd, '[');
  for (let i = 0; i < cases.length; i++) {
    if (i > 0) writeSync(fd, ',');
    const { buffer, ptr, len } = emitCaseJson(cases[i].input);
    writeSync(fd, Buffer.from(buffer, ptr, len));
  }
  writeSync(fd, ']');
  closeSync(fd);
}
