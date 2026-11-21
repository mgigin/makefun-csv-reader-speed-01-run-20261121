// WASM SIMD parser for csv-reader 1.0.0 semantics, written for the
// csv-reader-speed-01-run-20261121 bounty.
//
// The case text is copied by JS into linear memory as UTF-16LE at byte 65536
// (offset 0 holds AssemblyScript static data). parse() walks it once,
// recording a field index (8 B/field), a row table (8 B/record), an error
// table (16 B/error) and, for fields whose value is not a plain slice of the
// input, the assembled value in a scratch region. emitJson() then writes the
// case's answers JSON directly as UTF-8 bytes (byte-identical to
// JSON.stringify of the {columns, rows, errors} object), so the driver never
// builds per-field JS strings on the speed path.
//
// Delimiter scans use v128 i16x8.eq + bitmask + ctz over 64-byte blocks
// (4 x v128 = 32 code units); scalar code resolves boundaries and every
// edge case (quotes, doubled quotes, junk, unterminated re-read, stray CR,
// short/long rows, error line/row accounting), mirroring the reference
// policy exactly:
//   - a U+FEFF at the very start is dropped;
//   - a record ends at LF or CRLF, and a line with no characters is no record;
//   - outside quotes a CR not followed by LF is dropped (stray_cr);
//   - a quoted field runs to the next non-doubled quote, "" is one quote,
//     CRLF/lone CR inside reads as LF; text after the closing quote up to the
//     next comma/line-break is appended as it stands (quote_junk when the
//     field ends); an unclosed quote is a literal and the text after it is
//     re-read as unquoted (unterminated_quote);
//   - a quote inside an unquoted field is literal; fields stay strings;
//   - columns is the first record when header is true, else null; rows are
//     padded (short_row, fields: count) or cut (long_row, extra: cut fields)
//     to the first record's width;
//   - errors are {code, line, row} in the order met; line is the 1-based
//     line the record starts on (every LF/CRLF/lone CR inside quotes
//     counted), row the record's index in rows (null for the header).

const INPUT_BASE: usize = 65536;

const COMMA: i32 = 0;
const LINE_BREAK: i32 = 1;
const END: i32 = 2;

const ERR_STRAY_CR: u32 = 0;
const ERR_UNTERMINATED: u32 = 1;
const ERR_QUOTE_JUNK: u32 = 2;
const ERR_SHORT_ROW: u32 = 3;
const ERR_LONG_ROW: u32 = 4;

// Region bases (bytes), computed in prepare(); all 16-byte aligned.
let fieldsBase: usize = 0;
let rowsBase: usize = 0;
let asmBase: usize = 0;
let errBase: usize = 0;
let jsonBase: usize = 0;
let errCap: i32 = 0;

// Parse state.
let gN: i32 = 0;
let gHeader: bool = false;
let gPos: i32 = 0;
let gLine: i32 = 1;
let gRecordLine: i32 = 1;
let gRecordRow: i32 = -1;
let gWidth: i32 = -1;
let gHeaderDone: bool = false;
let gRowsPushed: i32 = 0;
let gFieldCount: i32 = 0;
let gRowCount: i32 = 0;
let gErrCount: i32 = 0;
let gAsmLen: i32 = 0;
let gAdded: i32 = 0;

// The field most recently read.
let fStart: i32 = 0;
let fEnd: i32 = 0;
let fKind: i32 = 0; // 0 = slice of input, 1 = slice of assembled region

// Emitter state.
let gOp: usize = 0;
let gOpLimit: usize = 0;

// Fused parse+emit state (parseEmit): fields are emitted to JSON as they are
// read, so no field index is built; fields past the record width go to an
// extras table in the fields region for the error emitter.
let gFused: bool = false;
let gExtrasCount: i32 = 0;
let gRowsEmitted: i32 = 0;
let gHeaderEmitted: bool = false;

@inline function align16(x: usize): usize {
  return (x + 15) & ~15;
}

@inline function unit(i: i32): i32 {
  return <i32>load<u16>(INPUT_BASE + ((<usize>i) << 1));
}

@inline function asmAppend(s: i32, e: i32): void {
  const len = e - s;
  if (len <= 0) return;
  memory.copy(
    asmBase + ((<usize>gAsmLen) << 1),
    INPUT_BASE + ((<usize>s) << 1),
    (<usize>len) << 1
  );
  gAsmLen += len;
}

@inline function asmAppendUnit(u: i32): void {
  store<u16>(asmBase + ((<usize>gAsmLen) << 1), <u16>u);
  gAsmLen++;
}

function growErrors(): void {
  const add = errCap;
  errCap += add;
  const newJsonBase = errBase + ((<usize>errCap) << 4);
  if (gFused) {
    // JSON already emitted sits at the old jsonBase; move it past the
    // enlarged error table before anything else writes there.
    const written = gOp - jsonBase;
    const need = newJsonBase + written + 65536;
    const cur = <usize>memory.size() << 16;
    if (need > cur) memory.grow(<i32>(((need - cur) + 65535) >> 16));
    memory.copy(newJsonBase, jsonBase, written);
    gOp = newJsonBase + written;
    jsonBase = newJsonBase;
    gOpLimit = <usize>memory.size() << 16;
  } else {
    jsonBase = newJsonBase;
    const need = jsonBase + 65536;
    const cur = <usize>memory.size() << 16;
    if (need > cur) memory.grow(<i32>(((need - cur) + 65535) >> 16));
  }
}

@inline function pushError(code: u32, aux: u32): void {
  if (gErrCount >= errCap) growErrors();
  const p = errBase + ((<usize>gErrCount) << 4);
  store<u32>(p, code);
  store<i32>(p + 4, gRecordLine);
  store<i32>(p + 8, gRecordRow);
  store<u32>(p + 12, aux);
  gErrCount++;
}

@inline function pushField(start: i32, end: i32, kind: i32): void {
  const p = fieldsBase + ((<usize>gFieldCount) << 3);
  store<u32>(p, <u32>start | (<u32>kind << 31));
  store<u32>(p + 4, <u32>end);
  gFieldCount++;
}

// Fused mode: fields past the record width are kept in an extras table in
// the fields region so the error emitter can print them under "extra".
@inline function pushExtra(start: i32, end: i32, kind: i32): void {
  const p = fieldsBase + ((<usize>gExtrasCount) << 3);
  store<u32>(p, <u32>start | (<u32>kind << 31));
  store<u32>(p + 4, <u32>end);
  gExtrasCount++;
}

// ---- SIMD scans ----

@inline function delimMask(v: v128, a: v128, b: v128, c: v128): i32 {
  const m = v128.or(v128.or(i16x8.eq(v, a), i16x8.eq(v, b)), i16x8.eq(v, c));
  return i16x8.bitmask(m);
}

// Next index >= i (and < limit) whose unit is one of the three splats, else limit.
@inline function scan3(i: i32, limit: i32, a: v128, b: v128, c: v128, s0: i32, s1: i32, s2: i32): i32 {
  while (i + 32 <= limit) {
    const p = INPUT_BASE + ((<usize>i) << 1);
    let m = delimMask(v128.load(p), a, b, c);
    if (m != 0) return i + ctz(m);
    m = delimMask(v128.load(p + 16), a, b, c);
    if (m != 0) return i + 8 + ctz(m);
    m = delimMask(v128.load(p + 32), a, b, c);
    if (m != 0) return i + 16 + ctz(m);
    m = delimMask(v128.load(p + 48), a, b, c);
    if (m != 0) return i + 24 + ctz(m);
    i += 32;
  }
  while (i + 8 <= limit) {
    const m = delimMask(v128.load(INPUT_BASE + ((<usize>i) << 1)), a, b, c);
    if (m != 0) return i + ctz(m);
    i += 8;
  }
  while (i < limit) {
    const u = unit(i);
    if (u == s0 || u == s1 || u == s2) return i;
    i++;
  }
  return limit;
}

@inline function scanDelim(i: i32): i32 {
  return scan3(i, gN, i16x8.splat(44), i16x8.splat(10), i16x8.splat(13), 44, 10, 13);
}

@inline function scanQuote(i: i32, limit: i32): i32 {
  return scan3(i, limit, i16x8.splat(34), i16x8.splat(13), i16x8.splat(10), 34, 13, 10);
}

// ---- Field readers ----

// Reads an unquoted field starting at gPos (no prefix). Sets fStart/fEnd/fKind.
@inline function readUnquotedPlain(): i32 {
  const start = gPos;
  let i = gPos;
  let pieceStart = start;
  let asmStart = -1;
  while (true) {
    i = scanDelim(i);
    if (i >= gN) {
      if (asmStart < 0) {
        fStart = start; fEnd = gN; fKind = 0;
      } else {
        asmAppend(pieceStart, gN);
        fStart = asmStart; fEnd = gAsmLen; fKind = 1;
      }
      gPos = gN;
      return END;
    }
    const c = unit(i);
    if (c == 44) {
      if (asmStart < 0) {
        fStart = start; fEnd = i; fKind = 0;
      } else {
        asmAppend(pieceStart, i);
        fStart = asmStart; fEnd = gAsmLen; fKind = 1;
      }
      gPos = i + 1;
      return COMMA;
    }
    if (c == 10) {
      if (asmStart < 0) {
        fStart = start; fEnd = i; fKind = 0;
      } else {
        asmAppend(pieceStart, i);
        fStart = asmStart; fEnd = gAsmLen; fKind = 1;
      }
      gPos = i + 1;
      gLine++;
      return LINE_BREAK;
    }
    // CR
    if (unit(i + 1) == 10) {
      if (asmStart < 0) {
        fStart = start; fEnd = i; fKind = 0;
      } else {
        asmAppend(pieceStart, i);
        fStart = asmStart; fEnd = gAsmLen; fKind = 1;
      }
      gPos = i + 2;
      gLine++;
      return LINE_BREAK;
    }
    pushError(ERR_STRAY_CR, 0);
    if (asmStart < 0) asmStart = gAsmLen;
    asmAppend(pieceStart, i);
    i += 1;
    pieceStart = i;
  }
}

// Reads an unquoted field whose value is '"' (at `open`) followed by the
// unquoted content starting at gPos (= open + 1). Unterminated-quote re-read.
@inline function readUnquotedPrefixed(open: i32): i32 {
  let i = gPos;
  let pieceStart = i;
  let asmStart = -1;
  while (true) {
    i = scanDelim(i);
    if (i >= gN) {
      if (asmStart < 0) {
        fStart = open; fEnd = gN; fKind = 0;
      } else {
        asmAppend(pieceStart, gN);
        fStart = asmStart; fEnd = gAsmLen; fKind = 1;
      }
      gPos = gN;
      return END;
    }
    const c = unit(i);
    if (c == 44 || c == 10 || (c == 13 && unit(i + 1) == 10)) {
      const isLB = c != 44;
      if (asmStart < 0) {
        fStart = open; fEnd = i; fKind = 0;
      } else {
        asmAppend(pieceStart, i);
        fStart = asmStart; fEnd = gAsmLen; fKind = 1;
      }
      gPos = i + (c == 13 ? 2 : 1);
      if (isLB) gLine++;
      return isLB ? LINE_BREAK : COMMA;
    }
    pushError(ERR_STRAY_CR, 0);
    if (asmStart < 0) {
      asmStart = gAsmLen;
      asmAppendUnit(34);
    }
    asmAppend(pieceStart, i);
    i += 1;
    pieceStart = i;
  }
}

// Reads junk after a closing quote: the field value is the quoted value
// (pStart, pEnd, pKind) followed by unquoted content from gPos. Sets
// fStart/fEnd/fKind and gAdded (characters kept from the junk).
@inline function readUnquotedJunk(pStart: i32, pEnd: i32, pKind: i32): i32 {
  let asmStart: i32;
  if (pKind == 1) {
    asmStart = pStart; // quoted value already assembled, ends at gAsmLen
  } else {
    asmStart = gAsmLen;
    asmAppend(pStart, pEnd);
  }
  gAdded = 0;
  let i = gPos;
  let pieceStart = i;
  while (true) {
    i = scanDelim(i);
    if (i >= gN) {
      asmAppend(pieceStart, gN);
      gAdded += gN - pieceStart;
      fStart = asmStart; fEnd = gAsmLen; fKind = 1;
      gPos = gN;
      return END;
    }
    const c = unit(i);
    if (c == 44 || c == 10 || (c == 13 && unit(i + 1) == 10)) {
      const isLB = c != 44;
      asmAppend(pieceStart, i);
      gAdded += i - pieceStart;
      fStart = asmStart; fEnd = gAsmLen; fKind = 1;
      gPos = i + (c == 13 ? 2 : 1);
      if (isLB) gLine++;
      return isLB ? LINE_BREAK : COMMA;
    }
    pushError(ERR_STRAY_CR, 0);
    asmAppend(pieceStart, i);
    gAdded += i - pieceStart;
    i += 1;
    pieceStart = i;
  }
}

// Reads a quoted field whose opening quote is at gPos. Sets fStart/fEnd/fKind.
@inline function readQuoted(): i32 {
  const open = gPos;
  // Find the closing quote: the next quote that is not doubled. Along the
  // way, note CRs / doubled quotes in the segment and count its LFs.
  let scan = open + 1;
  let close = -1;
  let sawCR = false;
  let sawDoubled = false;
  let lfCount = 0;
  while (scan < gN) {
    const hit = scanQuote(scan, gN);
    if (hit >= gN) break;
    const c = unit(hit);
    if (c == 10) {
      lfCount++;
      scan = hit + 1;
      continue;
    }
    if (c == 13) {
      sawCR = true;
      scan = hit + 1;
      continue;
    }
    // quote
    if (unit(hit + 1) == 34) {
      sawDoubled = true;
      scan = hit + 2;
      continue;
    }
    close = hit;
    break;
  }
  if (close < 0) {
    // Never closes: literal quote, text after it re-read as unquoted.
    pushError(ERR_UNTERMINATED, 0);
    gPos = open + 1;
    return readUnquotedPrefixed(open);
  }
  const segStart = open + 1;
  let vStart: i32;
  let vEnd: i32;
  let vKind: i32;
  if (!sawCR && !sawDoubled) {
    vStart = segStart;
    vEnd = close;
    vKind = 0;
    gLine += lfCount;
  } else {
    vStart = gAsmLen;
    vKind = 1;
    let i = segStart;
    let pieceStart = segStart;
    while (i < close) {
      const hit = scanQuote(i, close);
      if (hit >= close) break;
      const c = unit(hit);
      if (c == 34) {
        asmAppend(pieceStart, hit);
        asmAppendUnit(34);
        i = hit + 2;
        pieceStart = i;
      } else if (c == 13) {
        asmAppend(pieceStart, hit);
        asmAppendUnit(10);
        gLine++;
        i = hit + (unit(hit + 1) == 10 ? 2 : 1);
        pieceStart = i;
      } else {
        gLine++;
        i = hit + 1;
      }
    }
    asmAppend(pieceStart, close);
    vEnd = gAsmLen;
  }
  gPos = close + 1;
  if (gPos >= gN) {
    fStart = vStart; fEnd = vEnd; fKind = vKind;
    return END;
  }
  const c = unit(gPos);
  if (c == 44) {
    gPos++;
    fStart = vStart; fEnd = vEnd; fKind = vKind;
    return COMMA;
  }
  if (c == 10) {
    gPos++;
    gLine++;
    fStart = vStart; fEnd = vEnd; fKind = vKind;
    return LINE_BREAK;
  }
  if (c == 13 && unit(gPos + 1) == 10) {
    gPos += 2;
    gLine++;
    fStart = vStart; fEnd = vEnd; fKind = vKind;
    return LINE_BREAK;
  }
  const ended = readUnquotedJunk(vStart, vEnd, vKind);
  if (gAdded > 0) pushError(ERR_QUOTE_JUNK, 0);
  return ended;
}

// ---- Public entry points ----

export function prepare(n: i32): void {
  gN = n;
  const nn = <usize>n;
  let base = INPUT_BASE + ((nn + 34) << 1); // input + sentinel + padding
  base = align16(base);
  fieldsBase = base;
  base += (<usize>(n + 2)) << 3;
  base = align16(base);
  rowsBase = base;
  base += (<usize>(n + 2)) << 3;
  base = align16(base);
  asmBase = base;
  base += (nn + 2) << 1;
  base = align16(base);
  errBase = base;
  errCap = 8192;
  base += (<usize>errCap) << 4;
  base = align16(base);
  jsonBase = base;
  const need = base + 65536;
  const cur = <usize>memory.size() << 16;
  if (need > cur) memory.grow(<i32>(((need - cur) + 65535) >> 16));
}

export function parse(n: i32, header: i32): void {
  gFused = false;
  parseImpl(n, header);
}

// Fused parse+emit: parses and writes the case's answers JSON directly,
// returning its byte length. Fields are emitted as they are read (no field
// index); the JSON is byte-identical to emitJson() after parse().
export function parseEmit(n: i32, header: i32): i32 {
  gFused = true;
  gExtrasCount = 0;
  gRowsEmitted = 0;
  gHeaderEmitted = false;
  gOp = jsonBase;
  gOpLimit = <usize>memory.size() << 16;
  ensureCap(64);
  emitStr("{\"columns\":");
  if (header == 0) emitStr("null,\"rows\":[");
  parseImpl(n, header);
  if (header != 0 && !gHeaderEmitted) emitStr("[],\"rows\":[");
  emitStr("],\"errors\":[");
  for (let e = 0; e < gErrCount; e++) {
    if (e > 0) store<u8>(gOp++, 44);
    emitError(e);
  }
  emitStr("]}");
  return <i32>(gOp - jsonBase);
}

function parseImpl(n: i32, header: i32): void {
  gHeader = header != 0;
  gPos = 0;
  gLine = 1;
  gWidth = -1;
  gHeaderDone = false;
  gRowsPushed = 0;
  gFieldCount = 0;
  gRowCount = 0;
  gErrCount = 0;
  gAsmLen = 0;
  store<u16>(INPUT_BASE + ((<usize>n) << 1), 0); // sentinel
  if (n > 0 && unit(0) == 0xFEFF) gPos = 1;
  while (true) {
    while (gPos < n) {
      const c = unit(gPos);
      if (c == 10) {
        gPos++;
        gLine++;
      } else if (c == 13 && unit(gPos + 1) == 10) {
        gPos += 2;
        gLine++;
      } else {
        break;
      }
    }
    if (gPos >= n) break;
    gRecordLine = gLine;
    gRecordRow = gHeader && !gHeaderDone ? -1 : gRowsPushed;
    const firstField = gFieldCount;
    const asmSave = gAsmLen;
    const extrasSave = gExtrasCount;
    const opSave = gOp;
    const isFirst = gWidth < 0;
    if (gFused) {
      ensureCap(64);
      if (gHeaderEmitted || !gHeader) {
        if (gRowsEmitted > 0) store<u8>(gOp++, 44);
      }
      store<u8>(gOp++, 91); // [
    }
    let ended: i32 = 0;
    let quoted: bool = false;
    let k: i32 = 0;
    do {
      quoted = unit(gPos) == 34;
      ended = quoted ? readQuoted() : readUnquotedPlain();
      if (gFused) {
        if (isFirst || k < gWidth) {
          if (k > 0) store<u8>(gOp++, 44);
          emitFieldValue(fStart, fEnd, fKind);
        } else {
          pushExtra(fStart, fEnd, fKind);
        }
      } else {
        pushField(fStart, fEnd, fKind);
      }
      k++;
    } while (ended == COMMA);
    const count = k;
    // A line that held only stray CRs is left with no characters: no record.
    if (count == 1 && !quoted && fEnd == fStart) {
      gFieldCount = firstField;
      gAsmLen = asmSave;
      gExtrasCount = extrasSave;
      if (gFused) gOp = opSave;
      continue;
    }
    if (gWidth < 0) gWidth = count;
    const recIdx = gRowCount;
    if (gFused) {
      for (let p = count; p < gWidth; p++) {
        store<u8>(gOp++, 44);
        store<u8>(gOp++, 34);
        store<u8>(gOp++, 34);
      }
      store<u8>(gOp++, 93); // ]
      store<u32>(rowsBase + ((<usize>recIdx) << 3), <u32>extrasSave);
      store<u32>(rowsBase + ((<usize>recIdx) << 3) + 4, <u32>count);
    } else {
      store<u32>(rowsBase + ((<usize>recIdx) << 3), <u32>firstField);
      store<u32>(rowsBase + ((<usize>recIdx) << 3) + 4, <u32>count);
    }
    gRowCount++;
    if (gHeader && !gHeaderDone) {
      gHeaderDone = true;
      if (gFused) {
        gHeaderEmitted = true;
        emitStr(",\"rows\":[");
      }
      continue;
    }
    if (gFused) gRowsEmitted++;
    if (count < gWidth) pushError(ERR_SHORT_ROW, <u32>recIdx);
    else if (count > gWidth) pushError(ERR_LONG_ROW, <u32>recIdx);
    gRowsPushed++;
  }
}

export function inputBase(): usize { return INPUT_BASE; }
export function getFieldsBase(): usize { return fieldsBase; }
export function getRowsBase(): usize { return rowsBase; }
export function getAsmBase(): usize { return asmBase; }
export function getErrBase(): usize { return errBase; }
export function getJsonBase(): usize { return jsonBase; }
export function getFieldCount(): i32 { return gFieldCount; }
export function getRowCount(): i32 { return gRowCount; }
export function getErrCount(): i32 { return gErrCount; }
export function getWidth(): i32 { return gWidth; }

// ---- JSON emitter (byte-identical to JSON.stringify) ----

@inline function ensureCap(extra: i32): void {
  if (gOp + (<usize>extra) + 64 > gOpLimit) {
    const cur = <usize>memory.size() << 16;
    let need = gOp + (<usize>extra) + 65536;
    const dbl = cur << 1;
    if (dbl > need) need = dbl;
    if (need > cur) memory.grow(<i32>(((need - cur) + 65535) >> 16));
    gOpLimit = <usize>memory.size() << 16;
  }
}

@inline function emitStr(s: string): void {
  for (let i = 0; i < s.length; i++) {
    store<u8>(gOp++, <u8>s.charCodeAt(i));
  }
}

@inline function emitInt(v: i32): void {
  if (v == 0) {
    store<u8>(gOp++, 48);
    return;
  }
  let div = 1;
  while (v / div >= 10) div *= 10;
  while (div > 0) {
    store<u8>(gOp++, <u8>(48 + (v / div) % 10));
    div /= 10;
  }
}

@inline function hexDigit(d: i32): u8 {
  return <u8>(d < 10 ? 48 + d : 87 + d); // 0-9a-f
}

@inline function emitUXXXX(u: i32): void {
  store<u8>(gOp++, 92); // backslash
  store<u8>(gOp++, 117); // u
  store<u8>(gOp++, hexDigit((u >> 12) & 15));
  store<u8>(gOp++, hexDigit((u >> 8) & 15));
  store<u8>(gOp++, hexDigit((u >> 4) & 15));
  store<u8>(gOp++, hexDigit(u & 15));
}

@inline function emitUtf8(cp: i32): void {
  if (cp < 0x800) {
    store<u8>(gOp++, <u8>(0xC0 | (cp >> 6)));
    store<u8>(gOp++, <u8>(0x80 | (cp & 0x3F)));
  } else if (cp < 0x10000) {
    store<u8>(gOp++, <u8>(0xE0 | (cp >> 12)));
    store<u8>(gOp++, <u8>(0x80 | ((cp >> 6) & 0x3F)));
    store<u8>(gOp++, <u8>(0x80 | (cp & 0x3F)));
  } else {
    store<u8>(gOp++, <u8>(0xF0 | (cp >> 18)));
    store<u8>(gOp++, <u8>(0x80 | ((cp >> 12) & 0x3F)));
    store<u8>(gOp++, <u8>(0x80 | ((cp >> 6) & 0x3F)));
    store<u8>(gOp++, <u8>(0x80 | (cp & 0x3F)));
  }
}

// Emits one non-safe unit (or surrogate pair) at i; returns the next index.
@inline function emitEscaped(base: usize, i: i32, end: i32): i32 {
  const u = <i32>load<u16>(base + ((<usize>i) << 1));
  if (u >= 0x80) {
    if (u < 0xD800 || u > 0xDFFF) {
      emitUtf8(u);
      return i + 1;
    }
    if (u <= 0xDBFF) {
      if (i + 1 < end) {
        const u2 = <i32>load<u16>(base + ((<usize>(i + 1)) << 1));
        if (u2 >= 0xDC00 && u2 <= 0xDFFF) {
          emitUtf8(0x10000 + ((u - 0xD800) << 10) + (u2 - 0xDC00));
          return i + 2;
        }
      }
    }
    emitUXXXX(u);
    return i + 1;
  }
  if (u == 34) {
    store<u8>(gOp++, 92); store<u8>(gOp++, 34);
    return i + 1;
  }
  if (u == 92) {
    store<u8>(gOp++, 92); store<u8>(gOp++, 92);
    return i + 1;
  }
  if (u == 8) {
    store<u8>(gOp++, 92); store<u8>(gOp++, 98);
    return i + 1;
  }
  if (u == 9) {
    store<u8>(gOp++, 92); store<u8>(gOp++, 116);
    return i + 1;
  }
  if (u == 10) {
    store<u8>(gOp++, 92); store<u8>(gOp++, 110);
    return i + 1;
  }
  if (u == 12) {
    store<u8>(gOp++, 92); store<u8>(gOp++, 102);
    return i + 1;
  }
  if (u == 13) {
    store<u8>(gOp++, 92); store<u8>(gOp++, 114);
    return i + 1;
  }
  if (u < 0x20) {
    emitUXXXX(u);
    return i + 1;
  }
  // Remaining ASCII (0x20..0x7F) other than " and \ is safe; emitUnits only
  // routes non-safe units here, so this is unreachable in practice.
  store<u8>(gOp++, <u8>u);
  return i + 1;
}

// True when every lane is a JSON-safe ASCII unit: 0x20..0x7F except " and \.
@inline function isSafe(v: v128): bool {
  const t = i16x8.sub(v, i16x8.splat(0x20));
  const inRange = i16x8.lt_u(t, i16x8.splat(0x60));
  const notQ = v128.not(i16x8.eq(v, i16x8.splat(34)));
  const notB = v128.not(i16x8.eq(v, i16x8.splat(92)));
  const all = v128.and(v128.and(inRange, notQ), notB);
  return i16x8.bitmask(all) == 0xFF;
}

@inline function emitUnits(base: usize, start: i32, end: i32): void {
  let i = start;
  while (i + 16 <= end) {
    const p = base + ((<usize>i) << 1);
    const v0 = v128.load(p);
    const v1 = v128.load(p + 16);
    if (isSafe(v0) && isSafe(v1)) {
      v128.store(gOp, i8x16.narrow_i16x8_s(v0, v1));
      gOp += 16;
      i += 16;
    } else {
      break;
    }
  }
  const rem = end - i;
  if (rem == 16) {
    const p = base + ((<usize>i) << 1);
    const v0 = v128.load(p);
    const v1 = v128.load(p + 16);
    if (isSafe(v0) && isSafe(v1)) {
      v128.store(gOp, i8x16.narrow_i16x8_s(v0, v1));
      gOp += 16;
      return;
    }
  } else if (rem > 0 && rem < 16) {
    // Blended tail: lanes at/past `rem` are forced to 0x20 so the safety
    // check covers exactly the field's units; the overshoot bytes of the
    // 16-byte store are overwritten by what follows. Reads may pass `end`
    // by <16 units — both source regions have slack after them.
    const p = base + ((<usize>i) << 1);
    const lanes0 = v128(0, 0, 1, 0, 2, 0, 3, 0, 4, 0, 5, 0, 6, 0, 7, 0);
    const lanes1 = v128(8, 0, 9, 0, 10, 0, 11, 0, 12, 0, 13, 0, 14, 0, 15, 0);
    const sp = i16x8.splat(0x20);
    const rs = i16x8.splat(<i16>rem);
    const b0 = v128.bitselect(v128.load(p), sp, i16x8.lt_u(lanes0, rs));
    const b1 = v128.bitselect(v128.load(p + 16), sp, i16x8.lt_u(lanes1, rs));
    if (isSafe(b0) && isSafe(b1)) {
      v128.store(gOp, i8x16.narrow_i16x8_s(b0, b1));
      gOp += <usize>rem;
      return;
    }
  }
  while (i + 8 <= end) {
    const p = base + ((<usize>i) << 1);
    const v = v128.load(p);
    if (isSafe(v)) {
      const nw = i8x16.narrow_i16x8_s(v, i16x8.splat(0));
      v128.store32_lane(gOp, nw, 0);
      v128.store32_lane(gOp + 4, nw, 1);
      gOp += 8;
      i += 8;
    } else {
      break;
    }
  }
  while (i < end) {
    const u = <i32>load<u16>(base + ((<usize>i) << 1));
    if (u >= 0x20 && u < 0x80 && u != 34 && u != 92) {
      store<u8>(gOp++, <u8>u);
      i++;
    } else {
      i = emitEscaped(base, i, end);
    }
  }
}

@inline function emitFieldValue(start: i32, end: i32, kind: i32): void {
  ensureCap(6 * (end - start) + 2);
  store<u8>(gOp++, 34);
  emitUnits(kind == 0 ? INPUT_BASE : asmBase, start, end);
  store<u8>(gOp++, 34);
}

@inline function emitField(fi: i32): void {
  const p = fieldsBase + ((<usize>fi) << 3);
  const sw = load<u32>(p);
  const start = <i32>(sw & 0x7FFFFFFF);
  const kind = <i32>(sw >> 31);
  const end = <i32>load<u32>(p + 4);
  emitFieldValue(start, end, kind);
}

function emitRecordFields(r: i32, applyWidth: bool): void {
  const rp = rowsBase + ((<usize>r) << 3);
  const first = <i32>load<u32>(rp);
  const count = <i32>load<u32>(rp + 4);
  ensureCap(64);
  store<u8>(gOp++, 91); // [
  const emitCount = applyWidth && count > gWidth ? gWidth : count;
  for (let k = 0; k < emitCount; k++) {
    if (k > 0) store<u8>(gOp++, 44);
    emitField(first + k);
  }
  if (applyWidth) {
    for (let k = count; k < gWidth; k++) {
      store<u8>(gOp++, 44);
      store<u8>(gOp++, 34);
      store<u8>(gOp++, 34);
    }
  }
  store<u8>(gOp++, 93); // ]
}

function emitError(e: i32): void {
  const p = errBase + ((<usize>e) << 4);
  const code = load<u32>(p);
  const line = load<i32>(p + 4);
  const row = load<i32>(p + 8);
  const aux = <i32>load<u32>(p + 12);
  ensureCap(96);
  emitStr("{\"code\":\"");
  if (code == ERR_STRAY_CR) emitStr("stray_cr");
  else if (code == ERR_UNTERMINATED) emitStr("unterminated_quote");
  else if (code == ERR_QUOTE_JUNK) emitStr("quote_junk");
  else if (code == ERR_SHORT_ROW) emitStr("short_row");
  else emitStr("long_row");
  emitStr("\",\"line\":");
  emitInt(line);
  emitStr(",\"row\":");
  if (row < 0) emitStr("null");
  else emitInt(row);
  if (code == ERR_SHORT_ROW) {
    emitStr(",\"fields\":");
    emitInt(<i32>load<u32>(rowsBase + ((<usize>aux) << 3) + 4));
  } else if (code == ERR_LONG_ROW) {
    emitStr(",\"extra\":[");
    const rp = rowsBase + ((<usize>aux) << 3);
    const first = <i32>load<u32>(rp);
    const count = <i32>load<u32>(rp + 4);
    if (gFused) {
      // rows word0 = extras table offset, word1 = field count.
      const nExtra = count - gWidth;
      for (let j = 0; j < nExtra; j++) {
        if (j > 0) store<u8>(gOp++, 44);
        emitField(first + j);
      }
    } else {
      for (let k = gWidth; k < count; k++) {
        if (k > gWidth) store<u8>(gOp++, 44);
        emitField(first + k);
      }
    }
    store<u8>(gOp++, 93); // ]
  }
  store<u8>(gOp++, 125); // }
}

export function emitJson(): i32 {
  gOp = jsonBase;
  gOpLimit = <usize>memory.size() << 16;
  ensureCap(64);
  emitStr("{\"columns\":");
  if (!gHeader) {
    emitStr("null");
  } else if (gRowCount == 0) {
    emitStr("[]");
  } else {
    emitRecordFields(0, false);
  }
  emitStr(",\"rows\":[");
  const firstRow = gHeader ? 1 : 0;
  for (let r = firstRow; r < gRowCount; r++) {
    if (r > firstRow) store<u8>(gOp++, 44);
    emitRecordFields(r, true);
  }
  emitStr("],\"errors\":[");
  for (let e = 0; e < gErrCount; e++) {
    if (e > 0) store<u8>(gOp++, 44);
    emitError(e);
  }
  emitStr("]}");
  return <i32>(gOp - jsonBase);
}
