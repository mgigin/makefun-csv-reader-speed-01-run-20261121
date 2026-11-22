// Driver: the harness runs `node /bot/main.ts` (node22). It reads every case from /input/cases.json (a JSON
// array of {id, input}), answers each one alone, and writes /output/answers.json: a JSON array of the
// answers, one per case and in the cases' order. Outside a container MAKEFUN_INPUT_DIR and
// MAKEFUN_OUTPUT_DIR stand in for /input and /output.
//
// Byte path (default): the raw cases.json bytes are read straight into WASM memory; the module scans the
// JSON structure and transduces each case's text — CSV parse + answer-JSON emission in one pass over the
// encoded bytes — emitting the whole answers.json blob: no JSON.parse, no TextEncoder, no decoded copy
// in JS. If the input is not the expected shape the module says so and this driver falls back to the
// per-case streamed path (solve.js's emitCaseJson), then to run() + JSON.stringify per case.
// MAKEFUN_CSV_STREAMED=0 forces run() + JSON.stringify for every case.

import { closeSync, fstatSync, openSync, readFileSync, writeFileSync, writeSync } from 'node:fs';
import { join } from 'node:path';
import { setFlagsFromString } from 'node:v8';

// V8's incremental marking off, set before solve.js is imported so all of its work runs under it (the house
// baseline runs with --no-incremental-marking, and an entry may tune V8's flags too). WASM tier-up off and
// single-threaded: the sandbox CPU clock credits every running task per 10 ms tick, so V8's background
// tier-up compile thread would accrue ticks in parallel with the main thread and inflate gross CPU; the
// Liftoff-compiled module is fast enough for this SIMD scan/copy workload.
setFlagsFromString('--no-incremental-marking --no-wasm-tier-up --single-threaded');

/** solve.js, imported by its URL (a .js module carries no types) and typed by its contract. */
const { run, emitCaseJson, emitAllFromFd } = (await import(new URL('./solve.js', import.meta.url).href)) as {
  run: (input: unknown) => unknown;
  emitCaseJson: (input: unknown) => { buffer: ArrayBuffer; ptr: number; len: number } | null;
  emitAllFromFd: (fd: number, size: number) => { buffer: ArrayBuffer; ptr: number; len: number } | null;
};

const INPUT_DIR = process.env.MAKEFUN_INPUT_DIR ?? '/input';
const OUTPUT_DIR = process.env.MAKEFUN_OUTPUT_DIR ?? '/output';

const inFd = openSync(join(INPUT_DIR, 'cases.json'), 'r');
const inSize = fstatSync(inFd).size;
const all = process.env.MAKEFUN_CSV_STREAMED === '0' ? null : emitAllFromFd(inFd, inSize);
if (all) {
  closeSync(inFd);
  writeFileSync(join(OUTPUT_DIR, 'answers.json'), Buffer.from(all.buffer, all.ptr, all.len));
} else {
  const raw = readFileSync(join(INPUT_DIR, 'cases.json'));
  closeSync(inFd);
  const cases = JSON.parse(raw.toString('utf8')) as { id: string; input: unknown }[];
  if (process.env.MAKEFUN_CSV_STREAMED === '0') {
    const answers: unknown[] = [];
    for (const c of cases) answers.push(run(c.input));
    writeFileSync(join(OUTPUT_DIR, 'answers.json'), JSON.stringify(answers));
  } else {
    const fd = openSync(join(OUTPUT_DIR, 'answers.json'), 'w');
    writeSync(fd, '[');
    for (let i = 0; i < cases.length; i++) {
      if (i > 0) writeSync(fd, ',');
      const r = emitCaseJson(cases[i].input);
      if (r) {
        writeSync(fd, Buffer.from(r.buffer, r.ptr, r.len));
      } else {
        writeSync(fd, JSON.stringify(run(cases[i].input)));
      }
    }
    writeSync(fd, ']');
    closeSync(fd);
  }
}
