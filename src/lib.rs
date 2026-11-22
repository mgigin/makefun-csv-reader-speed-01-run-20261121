//! csv-reader-wasm: a Rust + WebAssembly SIMD128 drop-in for csv-reader 1.0.0.
//!
//! The parser works on the UTF-8 bytes of one case's text and emits the case's
//! answer ({columns, rows, errors}) as JSON bytes identical to
//! JSON.stringify(csv-reader's answer). UTF-8 multibyte sequences never contain
//! bytes below 0x80, so the ASCII structure (comma, quote, CR, LF) can be
//! scanned byte-wise; SIMD v128 masks find the next delimiter in one pass.
//!
//! Semantics mirror csv-reader 1.0.0 exactly:
//! - a U+FEFF (EF BB BF) at the very start is dropped;
//! - a record ends at LF or CRLF; a line with no characters is no record;
//! - outside quotes a CR not followed by LF is dropped (stray_cr);
//! - a field opening with a quote runs to the next undoubled quote, "" inside
//!   it one quote, each CRLF or lone CR inside it read as LF; what follows the
//!   closing quote up to the next comma or line break is appended as it stands
//!   (quote_junk when the field ends, if anything was appended);
//! - a quote that never closes is a literal character and the text after it is
//!   read again as unquoted (unterminated_quote);
//! - a quote inside an unquoted field is a literal character;
//! - columns is the first record when header is true, else null; rows holds the
//!   records after it (every record when header is false), each padded with ""
//!   (short_row, with fields: its count) or cut (long_row, with extra: the cut
//!   fields) to the first record's width;
//! - each error is {code, line, row} in the order met, a field's quote_junk
//!   when the field ends and a record's short_row/long_row when it ends; line
//!   is the 1-based line the record starts on, counting every LF, CRLF and lone
//!   CR inside quotes; row is the record's index in rows, null for the header.
//!
//! The scan advances through the input in 256 KiB windows; the state machine is
//! pos-based and resumes across window boundaries, so fields may span windows.

use std::vec::Vec;
use core::arch::wasm32::*;

/// Streaming window: the delimiter scan and span emission advance in slices of
/// at most this many bytes.
const WINDOW: usize = 256 * 1024;

static mut INPUT: Vec<u8> = Vec::new();
static mut OUTPUT: Vec<u8> = Vec::new();

// ---------------------------------------------------------------------------
// SIMD delimiter search
// ---------------------------------------------------------------------------

/// Find the offset of the first byte in `b[..]` equal to one of `set`
/// (up to 4 distinct bytes), or `b.len()` if none. One pass, v128 masks.
#[inline]
fn find_delimiters(b: &[u8], set: [u8; 4]) -> usize {
    let len = b.len();
    let mut i = 0usize;
    // Windowed scan: never look more than WINDOW bytes ahead of `i` at once.
    while i < len {
        let win_end = core::cmp::min(i + WINDOW, len);
        let hit = find_delims_window(&b[i..win_end], set);
        if hit < win_end - i {
            return i + hit;
        }
        i = win_end;
    }
    len
}

#[inline]
fn find_delims_window(b: &[u8], set: [u8; 4]) -> usize {
    let len = b.len();
    let mut i = 0usize;
    unsafe {
        let va = i8x16_splat(set[0] as i8);
        let vb = i8x16_splat(set[1] as i8);
        let vc = i8x16_splat(set[2] as i8);
        let vd = i8x16_splat(set[3] as i8);
        while i + 16 <= len {
            let v = v128_load(b.as_ptr().add(i) as *const v128);
            let m = v128_or(
                v128_or(i8x16_eq(v, va), i8x16_eq(v, vb)),
                v128_or(i8x16_eq(v, vc), i8x16_eq(v, vd)),
            );
            let mask = i8x16_bitmask(m);
            if mask != 0 {
                return i + mask.trailing_zeros() as usize;
            }
            i += 16;
        }
    }
    while i < len {
        let c = b[i];
        if c == set[0] || c == set[1] || c == set[2] || c == set[3] {
            return i;
        }
        i += 1;
    }
    len
}

/// Find the offset of the first byte that needs JSON escaping: `"` , `\`, or a
/// control byte < 0x20 (detected as (x & 0xE0) == 0). Or `b.len()`.
#[inline]
fn find_json_special(b: &[u8]) -> usize {
    let len = b.len();
    let mut i = 0usize;
    unsafe {
        let vq = i8x16_splat(b'"' as i8);
        let vbs = i8x16_splat(b'\\' as i8);
        let ve0 = i8x16_splat(0xE0u8 as i8);
        let vzero = i8x16_splat(0);
        while i + 16 <= len {
            let v = v128_load(b.as_ptr().add(i) as *const v128);
            let ctrl = i8x16_eq(v128_and(v, ve0), vzero);
            let m = v128_or(v128_or(i8x16_eq(v, vq), i8x16_eq(v, vbs)), ctrl);
            let mask = i8x16_bitmask(m);
            if mask != 0 {
                return i + mask.trailing_zeros() as usize;
            }
            i += 16;
        }
    }
    while i < len {
        let c = b[i];
        if c == b'"' || c == b'\\' || c < 0x20 {
            return i;
        }
        i += 1;
    }
    len
}

// ---------------------------------------------------------------------------
// JSON emission helpers (byte-identical to JSON.stringify for our values)
// ---------------------------------------------------------------------------

#[inline]
fn push_u32(out: &mut Vec<u8>, mut n: u32) {
    if n == 0 {
        out.push(b'0');
        return;
    }
    let mut buf = [0u8; 10];
    let mut i = 10;
    while n > 0 {
        i -= 1;
        buf[i] = b'0' + (n % 10) as u8;
        n /= 10;
    }
    out.extend_from_slice(&buf[i..]);
}

const HEX: &[u8; 16] = b"0123456789abcdef";

/// Append `bytes` to `out` as a JSON string literal, escaping exactly as
/// JSON.stringify does: `"` `\` and control bytes (short forms \b \t \n \f \r,
/// else \u00XX with lowercase hex). All other bytes pass through raw.
fn push_json_string(out: &mut Vec<u8>, bytes: &[u8]) {
    out.push(b'"');
    let mut rest = bytes;
    loop {
        let hit = find_json_special(rest);
        if hit > 0 {
            out.extend_from_slice(&rest[..hit]);
        }
        if hit >= rest.len() {
            break;
        }
        let c = rest[hit];
        match c {
            b'"' => out.extend_from_slice(b"\\\""),
            b'\\' => out.extend_from_slice(b"\\\\"),
            0x08 => out.extend_from_slice(b"\\b"),
            0x09 => out.extend_from_slice(b"\\t"),
            0x0A => out.extend_from_slice(b"\\n"),
            0x0C => out.extend_from_slice(b"\\f"),
            0x0D => out.extend_from_slice(b"\\r"),
            _ => {
                out.extend_from_slice(b"\\u00");
                out.push(HEX[(c >> 4) as usize]);
                out.push(HEX[(c & 0xF) as usize]);
            }
        }
        rest = &rest[hit + 1..];
    }
    out.push(b'"');
}

// ---------------------------------------------------------------------------
// Parser
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
enum Ended {
    Comma,
    LineBreak,
    End,
}

/// A field's final bytes: either a span of the input (zero-copy) or a span of
/// the assembly scratch (quoted fields, fields with dropped stray CRs, ...).
#[derive(Clone, Copy)]
enum Span {
    Input(usize, usize),
    Asm(usize, usize),
}

struct Parser<'a> {
    b: &'a [u8],
    pos: usize,
    line: u32,
    /// Errors JSON fragments, in order met (comma-joined, no brackets).
    errors: Vec<u8>,
    err_count: usize,
    /// Assembly scratch for the current record's transformed fields.
    asm: Vec<u8>,
    /// The current record's field spans (reused across records).
    fields: Vec<Span>,
    /// The record being read: its start line and its index in rows (-1: header).
    rec_line: u32,
    rec_row: i64,
}

impl<'a> Parser<'a> {
    fn new(b: &'a [u8]) -> Self {
        Parser {
            b,
            pos: 0,
            line: 1,
            errors: Vec::new(),
            err_count: 0,
            asm: Vec::new(),
            fields: Vec::new(),
            rec_line: 1,
            rec_row: -1,
        }
    }

    #[inline]
    fn at(&self, i: usize) -> u8 {
        if i < self.b.len() {
            self.b[i]
        } else {
            0
        }
    }

    fn err_open(&mut self, code: &str) {
        if self.err_count > 0 {
            self.errors.push(b',');
        }
        self.err_count += 1;
        self.errors.extend_from_slice(b"{\"code\":");
        push_json_string(&mut self.errors, code.as_bytes());
        self.errors.extend_from_slice(b",\"line\":");
        push_u32(&mut self.errors, self.rec_line);
        self.errors.extend_from_slice(b",\"row\":");
        if self.rec_row < 0 {
            self.errors.extend_from_slice(b"null");
        } else {
            push_u32(&mut self.errors, self.rec_row as u32);
        }
    }

    fn err(&mut self, code: &str) {
        self.err_open(code);
        self.errors.push(b'}');
    }

    fn err_short(&mut self, fields: usize) {
        self.err_open("short_row");
        self.errors.extend_from_slice(b",\"fields\":");
        push_u32(&mut self.errors, fields as u32);
        self.errors.push(b'}');
    }

    /// Read unquoted characters onto the current field up to the next comma or
    /// line break; a CR not followed by LF is dropped (stray_cr). When
    /// `prefix` is set the field starts assembled with that byte (the literal
    /// quote of an unterminated quoted field). Returns the field span, how it
    /// ended, and how many bytes were kept (for the quote_junk decision).
    fn read_unquoted(&mut self, prefix: Option<u8>) -> (Span, Ended, usize) {
        let start = self.pos;
        let asm_start = self.asm.len();
        let mut assembling = false;
        let mut added = 0usize;
        if let Some(p) = prefix {
            self.asm.push(p);
            assembling = true;
            added += 1;
        }
        loop {
            if self.pos >= self.b.len() {
                let span = if assembling {
                    Span::Asm(asm_start, self.asm.len())
                } else {
                    Span::Input(start, self.pos)
                };
                return (span, Ended::End, added);
            }
            let hit = find_delimiters(&self.b[self.pos..], [b',', b'\n', b'\r', b',']);
            let seg_end = self.pos + hit;
            if seg_end > self.pos {
                if assembling {
                    self.asm.extend_from_slice(&self.b[self.pos..seg_end]);
                }
                added += seg_end - self.pos;
                self.pos = seg_end;
            }
            if self.pos >= self.b.len() {
                let span = if assembling {
                    Span::Asm(asm_start, self.asm.len())
                } else {
                    Span::Input(start, self.pos)
                };
                return (span, Ended::End, added);
            }
            match self.b[self.pos] {
                b',' => {
                    self.pos += 1;
                    let span = if assembling {
                        Span::Asm(asm_start, self.asm.len())
                    } else {
                        Span::Input(start, self.pos - 1)
                    };
                    return (span, Ended::Comma, added);
                }
                b'\n' => {
                    self.pos += 1;
                    self.line += 1;
                    let span = if assembling {
                        Span::Asm(asm_start, self.asm.len())
                    } else {
                        Span::Input(start, self.pos - 1)
                    };
                    return (span, Ended::LineBreak, added);
                }
                b'\r' => {
                    if self.at(self.pos + 1) == b'\n' {
                        let end = self.pos;
                        self.pos += 2;
                        self.line += 1;
                        let span = if assembling {
                            Span::Asm(asm_start, self.asm.len())
                        } else {
                            Span::Input(start, end)
                        };
                        return (span, Ended::LineBreak, added);
                    }
                    // Stray CR: dropped, error. Switch to assembly so the kept
                    // segments join up.
                    self.err("stray_cr");
                    if !assembling {
                        self.asm.extend_from_slice(&self.b[start..self.pos]);
                        assembling = true;
                    }
                    self.pos += 1;
                }
                _ => unreachable!(),
            }
        }
    }

    /// Read a quoted field whose opening quote is at `pos`.
    fn read_quoted(&mut self) -> (Span, Ended) {
        let open = self.pos;
        let open_line = self.line;
        let asm_start = self.asm.len();
        self.pos += 1;
        let mut closed = false;
        loop {
            if self.pos >= self.b.len() {
                break;
            }
            let hit = find_delimiters(&self.b[self.pos..], [b'"', b'\r', b'\n', b'"']);
            let seg_end = self.pos + hit;
            if seg_end > self.pos {
                self.asm.extend_from_slice(&self.b[self.pos..seg_end]);
                self.pos = seg_end;
            }
            if self.pos >= self.b.len() {
                break;
            }
            match self.b[self.pos] {
                b'"' => {
                    if self.at(self.pos + 1) == b'"' {
                        self.asm.push(b'"');
                        self.pos += 2;
                    } else {
                        self.pos += 1;
                        closed = true;
                        break;
                    }
                }
                b'\r' => {
                    self.asm.push(b'\n');
                    self.line += 1;
                    self.pos += if self.at(self.pos + 1) == b'\n' { 2 } else { 1 };
                }
                b'\n' => {
                    self.line += 1;
                    self.asm.push(b'\n');
                    self.pos += 1;
                }
                _ => unreachable!(),
            }
        }
        if !closed {
            // The quote never closes: it is a literal character and the text
            // after it is read again as unquoted. Discard the assembled scan.
            self.asm.truncate(asm_start);
            self.pos = open + 1;
            self.line = open_line;
            self.err("unterminated_quote");
            let (span, ended, _) = self.read_unquoted(Some(b'"'));
            return (span, ended);
        }
        let span = Span::Asm(asm_start, self.asm.len());
        if self.pos >= self.b.len() {
            return (span, Ended::End);
        }
        match self.b[self.pos] {
            b',' => {
                self.pos += 1;
                (span, Ended::Comma)
            }
            b'\n' => {
                self.pos += 1;
                self.line += 1;
                (span, Ended::LineBreak)
            }
            b'\r' if self.at(self.pos + 1) == b'\n' => {
                self.pos += 2;
                self.line += 1;
                (span, Ended::LineBreak)
            }
            _ => {
                // Junk after the closing quote, appended as it stands (with
                // unquoted stray-CR handling), up to the next comma or break.
                let (jspan, ended, added) = self.read_unquoted_junk(asm_start);
                if added > 0 {
                    self.err("quote_junk");
                }
                (jspan, ended)
            }
        }
    }

    /// Continue an assembled field (already in asm from `asm_start`) with
    /// unquoted reading: stray CRs dropped with errors. Returns span, ended,
    /// and bytes kept during this junk phase.
    fn read_unquoted_junk(&mut self, asm_start: usize) -> (Span, Ended, usize) {
        let mut added = 0usize;
        loop {
            if self.pos >= self.b.len() {
                return (Span::Asm(asm_start, self.asm.len()), Ended::End, added);
            }
            let hit = find_delimiters(&self.b[self.pos..], [b',', b'\n', b'\r', b',']);
            let seg_end = self.pos + hit;
            if seg_end > self.pos {
                self.asm.extend_from_slice(&self.b[self.pos..seg_end]);
                added += seg_end - self.pos;
                self.pos = seg_end;
            }
            if self.pos >= self.b.len() {
                return (Span::Asm(asm_start, self.asm.len()), Ended::End, added);
            }
            match self.b[self.pos] {
                b',' => {
                    self.pos += 1;
                    return (Span::Asm(asm_start, self.asm.len()), Ended::Comma, added);
                }
                b'\n' => {
                    self.pos += 1;
                    self.line += 1;
                    return (Span::Asm(asm_start, self.asm.len()), Ended::LineBreak, added);
                }
                b'\r' => {
                    if self.at(self.pos + 1) == b'\n' {
                        self.pos += 2;
                        self.line += 1;
                        return (Span::Asm(asm_start, self.asm.len()), Ended::LineBreak, added);
                    }
                    self.err("stray_cr");
                    self.pos += 1;
                }
                _ => unreachable!(),
            }
        }
    }

    fn span_bytes(&self, span: Span) -> &[u8] {
        match span {
            Span::Input(s, e) => &self.b[s..e],
            Span::Asm(s, e) => &self.asm[s..e],
        }
    }

    fn span_is_empty(&self, span: Span) -> bool {
        match span {
            Span::Input(s, e) => s == e,
            Span::Asm(s, e) => s == e,
        }
    }
}

/// Parse one case and append its answer JSON to `out`.
fn run_case_into(input: &[u8], header: bool, out: &mut Vec<u8>) {
    let mut p = Parser::new(input);
    // Drop a U+FEFF at the very start (EF BB BF in UTF-8).
    if input.len() >= 3 && input[0] == 0xEF && input[1] == 0xBB && input[2] == 0xBF {
        p.pos = 3;
    }

    out.extend_from_slice(b"{\"columns\":");
    let mut columns_done = false;
    let mut rows_started = false;
    let mut rows_count: usize = 0;
    let mut width: i64 = -1;

    loop {
        // Lines with no characters are no records.
        while p.pos < p.b.len() {
            if p.b[p.pos] == b'\n' {
                p.pos += 1;
                p.line += 1;
            } else if p.b[p.pos] == b'\r' && p.at(p.pos + 1) == b'\n' {
                p.pos += 2;
                p.line += 1;
            } else {
                break;
            }
        }
        if p.pos >= p.b.len() {
            break;
        }

        p.rec_line = p.line;
        p.rec_row = if header && !columns_done {
            -1
        } else {
            rows_count as i64
        };
        p.asm.clear();
        p.fields.clear();

        let mut last_quoted = false;
        loop {
            let quoted = p.at(p.pos) == b'"';
            last_quoted = quoted;
            let (span, ended) = if quoted {
                p.read_quoted()
            } else {
                let (s, e, _) = p.read_unquoted(None);
                (s, e)
            };
            p.fields.push(span);
            if ended != Ended::Comma {
                break;
            }
        }
        // Take the spans out so emission can borrow the parser freely; the
        // Vec's capacity returns to the parser at the end of the record.
        let fields = core::mem::take(&mut p.fields);

        // A line that held only stray CRs is left with no characters: no
        // record (its stray_cr errors stay).
        if fields.len() == 1 && p.span_is_empty(fields[0]) && !last_quoted {
            p.fields = fields;
            continue;
        }

        if width < 0 {
            width = fields.len() as i64;
        }
        let w = width as usize;

        if header && !columns_done {
            // The header record: columns, unadjusted.
            out.push(b'[');
            for (i, f) in fields.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                let bytes = p.span_bytes(*f);
                push_json_string(out, bytes);
            }
            out.push(b']');
            columns_done = true;
            p.fields = fields;
            continue;
        }

        if !rows_started {
            if !columns_done {
                // header is false: columns is null.
                out.extend_from_slice(b"null");
                columns_done = true;
            }
            out.extend_from_slice(b",\"rows\":[");
            rows_started = true;
        }
        if rows_count > 0 {
            out.push(b',');
        }

        // Width adjustment errors come when the record ends (after any
        // field-level errors already recorded).
        let n = fields.len();
        if n < w {
            p.err_short(n);
        } else if n > w {
            p.err_open("long_row");
            p.errors.extend_from_slice(b",\"extra\":[");
            for (i, f) in fields[w..].iter().enumerate() {
                if i > 0 {
                    p.errors.push(b',');
                }
                // Disjoint field borrows: read the span bytes from b/asm
                // while appending to errors.
                let bytes: &[u8] = match *f {
                    Span::Input(s, e) => &p.b[s..e],
                    Span::Asm(s, e) => &p.asm[s..e],
                };
                let mut tmp: Vec<u8> = Vec::new();
                push_json_string(&mut tmp, bytes);
                p.errors.extend_from_slice(&tmp);
            }
            p.errors.push(b']');
            p.errors.push(b'}');
        }

        out.push(b'[');
        let emit_n = core::cmp::min(n, w);
        for i in 0..emit_n {
            if i > 0 {
                out.push(b',');
            }
            let bytes = p.span_bytes(fields[i]);
            push_json_string(out, bytes);
        }
        for i in n..w {
            if i > 0 {
                out.push(b',');
            }
            out.extend_from_slice(b"\"\"");
        }
        out.push(b']');
        rows_count += 1;
        p.fields = fields;
    }

    if !columns_done {
        // No records at all: header ? [] : null.
        if header {
            out.extend_from_slice(b"[]");
        } else {
            out.extend_from_slice(b"null");
        }
    }
    if !rows_started {
        out.extend_from_slice(b",\"rows\":[");
    }
    out.extend_from_slice(b"],\"errors\":[");
    out.extend_from_slice(&p.errors);
    out.extend_from_slice(b"]}");
}

// ---------------------------------------------------------------------------
// Fused JSON-to-JSON transducer: parse the CSV directly on the *encoded*
// JSON string content of a case's text and emit the answer JSON in the same
// pass — no decoded scratch copy, no span phase, no second escape scan.
//
// Inside a JSON string the decoded text's structural characters appear as:
//   ',' -> raw ',' byte or \u002C
//   '"' -> \" or \u0022
//   CR  -> \r or \u000D
//   LF  -> \n or \u000A
// Raw bytes in the span are never '"' or control bytes (the span scan
// rejects raw controls), so a raw run between ',' and '\\' is pure content
// already in JSON.stringify-canonical form and copies verbatim. Escapes are
// decoded to a character and re-encoded canonically on emit, so
// non-canonical input encodings (\u0041, \/, ...) still produce the exact
// JSON.stringify output. Lone surrogates are Err (the JS fallback decides).
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
enum Ch {
    Comma,
    Quote,
    Cr,
    Lf,
    Other,
}

/// Decode the character at `pos` in an encoded JSON string span. Returns
/// (kind, bytes consumed, code point when the char came from an escape).
/// Raw content bytes report Other with cp = u32::MAX; callers use scan_run
/// for those and consult decode_at only at ',' or '\\' positions (and for
/// lookahead).
#[inline]
fn decode_at(b: &[u8], pos: usize) -> Result<(Ch, usize, u32), ()> {
    // SAFETY: every caller guarantees pos < b.len().
    let c = unsafe { *b.get_unchecked(pos) };
    if c == b',' {
        return Ok((Ch::Comma, 1, 0));
    }
    if c != b'\\' {
        return Ok((Ch::Other, 1, u32::MAX));
    }
    if pos + 1 >= b.len() {
        return Err(());
    }
    match b[pos + 1] {
        b'"' => Ok((Ch::Quote, 2, 0)),
        b'\\' => Ok((Ch::Other, 2, 0x5C)),
        b'/' => Ok((Ch::Other, 2, 0x2F)),
        b'b' => Ok((Ch::Other, 2, 0x08)),
        b'f' => Ok((Ch::Other, 2, 0x0C)),
        b'n' => Ok((Ch::Lf, 2, 0)),
        b'r' => Ok((Ch::Cr, 2, 0)),
        b't' => Ok((Ch::Other, 2, 0x09)),
        b'u' => {
            let cp = hex4(&b[pos + 2..])?;
            if (0xD800..0xDC00).contains(&cp) {
                // High surrogate: only a following \uDC00-\uDFFF is accepted.
                if pos + 7 < b.len() && b[pos + 6] == b'\\' && b[pos + 7] == b'u' {
                    let lo = hex4(&b[pos + 8..])?;
                    if (0xDC00..0xE000).contains(&lo) {
                        let full = 0x10000 + ((cp - 0xD800) << 10) + (lo - 0xDC00);
                        return Ok((Ch::Other, 12, full));
                    }
                }
                Err(())
            } else if (0xDC00..0xE000).contains(&cp) {
                Err(())
            } else {
                let kind = match cp {
                    0x2C => Ch::Comma,
                    0x22 => Ch::Quote,
                    0x0D => Ch::Cr,
                    0x0A => Ch::Lf,
                    _ => Ch::Other,
                };
                Ok((kind, 6, cp))
            }
        }
        _ => Err(()),
    }
}

/// Emit one decoded content character (from an escape) in JSON.stringify
/// canonical form. Structural characters never reach here.
#[inline]
fn emit_escape_char(dst: &mut Vec<u8>, cp: u32) {
    match cp {
        0x08 => dst.extend_from_slice(b"\\b"),
        0x09 => dst.extend_from_slice(b"\\t"),
        0x0C => dst.extend_from_slice(b"\\f"),
        0x5C => dst.extend_from_slice(b"\\\\"),
        0x00..=0x1F => {
            dst.extend_from_slice(b"\\u00");
            dst.push(HEX[(cp >> 4) as usize]);
            dst.push(HEX[(cp & 0xF) as usize]);
        }
        _ => push_utf8(dst, cp),
    }
}

/// Length of the content run at the start of `b`: bytes that are neither
/// ',' nor '\\' nor '"' (the JSON string's closing quote) nor a raw control
/// byte < 0x20. Also reports whether the run contains a byte >= 0x80, so
/// the caller knows the case's span needs UTF-8 validation before its
/// output is trusted (validation runs once per case, after transduction;
/// on invalid UTF-8 the case declines to the fallback).
#[inline]
fn scan_run2(b: &[u8]) -> (usize, bool) {
    let len = b.len();
    let mut i = 0usize;
    let mut hi = false;
    unsafe {
        let vcomma = i8x16_splat(b',' as i8);
        let vbs = i8x16_splat(b'\\' as i8);
        let vq = i8x16_splat(b'"' as i8);
        let ve0 = i8x16_splat(0xE0u8 as i8);
        let vzero = i8x16_splat(0);
        while i + 16 <= len {
            let v = v128_load(b.as_ptr().add(i) as *const v128);
            let ctrl = i8x16_eq(v128_and(v, ve0), vzero);
            let m = v128_or(
                v128_or(i8x16_eq(v, vcomma), i8x16_eq(v, vbs)),
                v128_or(i8x16_eq(v, vq), ctrl),
            );
            let mask = i8x16_bitmask(m);
            if mask != 0 {
                let k = mask.trailing_zeros() as usize;
                let before = (1u32 << k) - 1;
                if (i8x16_bitmask(v) as u32) & before != 0 {
                    hi = true;
                }
                return (i + k, hi);
            }
            if i8x16_bitmask(v) != 0 {
                hi = true;
            }
            i += 16;
        }
    }
    while i < len {
        let c = b[i];
        if c == b',' || c == b'\\' || c == b'"' || c < 0x20 {
            return (i, hi);
        }
        if c >= 0x80 {
            hi = true;
        }
        i += 1;
    }
    (len, hi)
}

struct Tr<'a> {
    b: &'a [u8],
    pos: usize,
    line: u32,
    errors: Vec<u8>,
    err_count: usize,
    /// Offsets of every `"row":` value in `errors`, for header fix-up.
    row_toks: Vec<usize>,
    rec_line: u32,
    rec_row: i64,
    /// Set when any content run held a byte >= 0x80: the span then needs
    /// UTF-8 validation before the case's output is trusted.
    hi: bool,
}

impl<'a> Tr<'a> {
    /// End of the encoded text: the raw closing quote of the JSON string
    /// (or the end of the input slice, on malformed input).
    #[inline]
    fn at_end(&self) -> bool {
        self.pos >= self.b.len() || self.b[self.pos] == b'"'
    }

    fn err_open(&mut self, code: &str) {
        if self.err_count > 0 {
            self.errors.push(b',');
        }
        self.err_count += 1;
        self.errors.extend_from_slice(b"{\"code\":");
        push_json_string(&mut self.errors, code.as_bytes());
        self.errors.extend_from_slice(b",\"line\":");
        push_u32(&mut self.errors, self.rec_line);
        self.errors.extend_from_slice(b",\"row\":");
        self.row_toks.push(self.errors.len());
        if self.rec_row < 0 {
            self.errors.extend_from_slice(b"null");
        } else {
            push_u32(&mut self.errors, self.rec_row as u32);
        }
    }

    fn err(&mut self, code: &str) {
        self.err_open(code);
        self.errors.push(b'}');
    }

    fn err_short(&mut self, fields: usize) {
        self.err_open("short_row");
        self.errors.extend_from_slice(b",\"fields\":");
        push_u32(&mut self.errors, fields as u32);
        self.errors.push(b'}');
    }

    /// If the character at `pos` is an LF, its byte length; else None.
    #[inline]
    fn lf_at(&self, pos: usize) -> Result<Option<usize>, ()> {
        if pos >= self.b.len() {
            return Ok(None);
        }
        // Fast path: the common \n encoding.
        if self.b[pos] == b'\\' && pos + 1 < self.b.len() && self.b[pos + 1] == b'n' {
            return Ok(Some(2));
        }
        let (ch, adv, _) = decode_at(self.b, pos)?;
        Ok(if ch == Ch::Lf { Some(adv) } else { None })
    }

    /// Read unquoted content onto `dst` (the field's JSON string body, the
    /// opening quote already pushed by the caller) up to the next comma or
    /// line break; a CR not followed by LF is dropped (stray_cr). Returns
    /// how the field ended and how many content units were appended (only
    /// zero vs non-zero matters, for the quote_junk decision).
    fn read_unquoted(&mut self, dst: &mut Vec<u8>) -> Result<(Ended, usize), ()> {
        let mut added = 0usize;
        loop {
            if self.at_end() {
                return Ok((Ended::End, added));
            }
            let (run, hi) = scan_run2(unsafe { self.b.get_unchecked(self.pos..) });
            self.hi |= hi;
            if run > 0 {
                let seg = unsafe { self.b.get_unchecked(self.pos..self.pos + run) };
                dst.extend_from_slice(seg);
                self.pos += run;
                added += run;
            }
            if self.at_end() {
                return Ok((Ended::End, added));
            }
            match unsafe { *self.b.get_unchecked(self.pos) } {
                b',' => {
                    self.pos += 1;
                    return Ok((Ended::Comma, added));
                }
                b'\\' => {
                    // Fast paths for the one-letter escapes (the only kinds
                    // the generator emits); \uXXXX goes through decode_at.
                    if self.pos + 1 >= self.b.len() {
                        return Err(());
                    }
                    match unsafe { *self.b.get_unchecked(self.pos + 1) } {
                        b'n' => {
                            self.pos += 2;
                            self.line += 1;
                            return Ok((Ended::LineBreak, added));
                        }
                        b'r' => {
                            let npos = self.pos + 2;
                            if let Some(ladv) = self.lf_at(npos)? {
                                self.pos = npos + ladv;
                                self.line += 1;
                                return Ok((Ended::LineBreak, added));
                            }
                            self.err("stray_cr");
                            self.pos = npos;
                        }
                        b'"' => {
                            dst.extend_from_slice(b"\\\"");
                            self.pos += 2;
                            added += 1;
                        }
                        b'\\' => {
                            dst.extend_from_slice(b"\\\\");
                            self.pos += 2;
                            added += 1;
                        }
                        b't' => {
                            dst.extend_from_slice(b"\\t");
                            self.pos += 2;
                            added += 1;
                        }
                        b'b' => {
                            dst.extend_from_slice(b"\\b");
                            self.pos += 2;
                            added += 1;
                        }
                        b'f' => {
                            dst.extend_from_slice(b"\\f");
                            self.pos += 2;
                            added += 1;
                        }
                        b'/' => {
                            dst.push(b'/');
                            self.pos += 2;
                            added += 1;
                        }
                        _ => {
                            let (ch, adv, cp) = decode_at(self.b, self.pos)?;
                            match ch {
                                Ch::Lf => {
                                    self.pos += adv;
                                    self.line += 1;
                                    return Ok((Ended::LineBreak, added));
                                }
                                Ch::Cr => {
                                    let npos = self.pos + adv;
                                    if let Some(ladv) = self.lf_at(npos)? {
                                        self.pos = npos + ladv;
                                        self.line += 1;
                                        return Ok((Ended::LineBreak, added));
                                    }
                                    self.err("stray_cr");
                                    self.pos = npos;
                                }
                                Ch::Comma => {
                                    self.pos += adv;
                                    return Ok((Ended::Comma, added));
                                }
                                Ch::Quote => {
                                    dst.extend_from_slice(b"\\\"");
                                    self.pos += adv;
                                    added += 1;
                                }
                                Ch::Other => {
                                    emit_escape_char(dst, cp);
                                    self.pos += adv;
                                    added += 1;
                                }
                            }
                        }
                    }
                }
                _ => return Err(()), // raw control byte (quote handled by at_end)
            }
        }
    }

    /// Read a quoted field whose opening quote is at `pos`, emitting its
    /// content onto `dst`.
    fn read_quoted(&mut self, dst: &mut Vec<u8>) -> Result<Ended, ()> {
        let open = self.pos;
        let open_line = self.line;
        let dst_start = dst.len();
        let (_, open_adv, _) = decode_at(self.b, self.pos)?;
        self.pos += open_adv;
        let mut closed = false;
        loop {
            if self.at_end() {
                break;
            }
            let (run, hi) = scan_run2(unsafe { self.b.get_unchecked(self.pos..) });
            self.hi |= hi;
            if run > 0 {
                let seg = unsafe { self.b.get_unchecked(self.pos..self.pos + run) };
                dst.extend_from_slice(seg);
                self.pos += run;
            }
            if self.at_end() {
                break;
            }
            match unsafe { *self.b.get_unchecked(self.pos) } {
                b',' => {
                    dst.push(b',');
                    self.pos += 1;
                }
                b'\\' => {
                    if self.pos + 1 >= self.b.len() {
                        return Err(());
                    }
                    match unsafe { *self.b.get_unchecked(self.pos + 1) } {
                        b'"' => {
                            // Doubled quote, or the closing quote.
                            let npos = self.pos + 2;
                            if npos + 1 < self.b.len()
                                && self.b[npos] == b'\\'
                                && self.b[npos + 1] == b'"'
                            {
                                dst.extend_from_slice(b"\\\"");
                                self.pos = npos + 2;
                            } else if npos < self.b.len()
                                && decode_at(self.b, npos)?.0 == Ch::Quote
                            {
                                let adv2 = decode_at(self.b, npos)?.1;
                                dst.extend_from_slice(b"\\\"");
                                self.pos = npos + adv2;
                            } else {
                                self.pos = npos;
                                closed = true;
                                break;
                            }
                        }
                        b'r' => {
                            dst.extend_from_slice(b"\\n");
                            self.line += 1;
                            let npos = self.pos + 2;
                            self.pos = npos;
                            if let Some(ladv) = self.lf_at(npos)? {
                                self.pos = npos + ladv;
                            }
                        }
                        b'n' => {
                            dst.extend_from_slice(b"\\n");
                            self.line += 1;
                            self.pos += 2;
                        }
                        b'\\' => {
                            dst.extend_from_slice(b"\\\\");
                            self.pos += 2;
                        }
                        b't' => {
                            dst.extend_from_slice(b"\\t");
                            self.pos += 2;
                        }
                        b'b' => {
                            dst.extend_from_slice(b"\\b");
                            self.pos += 2;
                        }
                        b'f' => {
                            dst.extend_from_slice(b"\\f");
                            self.pos += 2;
                        }
                        b'/' => {
                            dst.push(b'/');
                            self.pos += 2;
                        }
                        _ => {
                            let (ch, adv, cp) = decode_at(self.b, self.pos)?;
                            match ch {
                                Ch::Quote => {
                                    let npos = self.pos + adv;
                                    let mut doubled = false;
                                    if npos < self.b.len() {
                                        let (ch2, adv2, _) = decode_at(self.b, npos)?;
                                        if ch2 == Ch::Quote {
                                            dst.extend_from_slice(b"\\\"");
                                            self.pos = npos + adv2;
                                            doubled = true;
                                        }
                                    }
                                    if !doubled {
                                        self.pos = npos;
                                        closed = true;
                                        break;
                                    }
                                }
                                Ch::Cr => {
                                    dst.extend_from_slice(b"\\n");
                                    self.line += 1;
                                    let npos = self.pos + adv;
                                    self.pos = npos;
                                    if let Some(ladv) = self.lf_at(npos)? {
                                        self.pos = npos + ladv;
                                    }
                                }
                                Ch::Lf => {
                                    dst.extend_from_slice(b"\\n");
                                    self.line += 1;
                                    self.pos += adv;
                                }
                                Ch::Comma => {
                                    dst.push(b',');
                                    self.pos += adv;
                                }
                                Ch::Other => {
                                    emit_escape_char(dst, cp);
                                    self.pos += adv;
                                }
                            }
                        }
                    }
                }
                b'"' => break, // raw closing quote: unterminated (handled below)
                _ => return Err(()), // raw control byte
            }
        }
        if !closed {
            // The quote never closes: it is a literal character and the text
            // after it is read again as unquoted. Discard the quoted scan.
            dst.truncate(dst_start);
            self.pos = open + open_adv;
            self.line = open_line;
            self.err("unterminated_quote");
            dst.extend_from_slice(b"\\\"");
            let (ended, _) = self.read_unquoted(dst)?;
            return Ok(ended);
        }
        if self.at_end() {
            return Ok(Ended::End);
        }
        let (ch, adv, _) = decode_at(self.b, self.pos)?;
        match ch {
            Ch::Comma => {
                self.pos += adv;
                Ok(Ended::Comma)
            }
            Ch::Lf => {
                self.pos += adv;
                self.line += 1;
                Ok(Ended::LineBreak)
            }
            Ch::Cr => {
                let npos = self.pos + adv;
                if let Some(ladv) = self.lf_at(npos)? {
                    self.pos = npos + ladv;
                    self.line += 1;
                    Ok(Ended::LineBreak)
                } else {
                    self.read_junk(dst)
                }
            }
            _ => self.read_junk(dst),
        }
    }

    /// Junk after a closing quote: appended as it stands (unquoted rules);
    /// quote_junk when the field ends, if anything was appended.
    fn read_junk(&mut self, dst: &mut Vec<u8>) -> Result<Ended, ()> {
        let (ended, added) = self.read_unquoted(dst)?;
        if added > 0 {
            self.err("quote_junk");
        }
        Ok(ended)
    }
}

/// Per-case scratch for speculative transduction: records are emitted
/// before the header flag is known (the practice key order is text before
/// header), so the first kept record and the rest are staged separately and
/// framed by `assemble_case` once the object scan completes.
struct CaseScratch {
    first: Vec<u8>,
    extra: Vec<u8>,
    errors: Vec<u8>,
    patched: Vec<u8>,
    row_toks: Vec<usize>,
    have_first: bool,
    phase0_end: usize,
    /// Offset in `out` where this case's object starts, and where its rest
    /// records start (rest records are written straight into `out`).
    case_start: usize,
    rest_start: usize,
}

impl CaseScratch {
    fn new() -> Self {
        CaseScratch {
            first: Vec::new(),
            extra: Vec::new(),
            errors: Vec::new(),
            patched: Vec::new(),
            row_toks: Vec::new(),
            have_first: false,
            phase0_end: 0,
            case_start: 0,
            rest_start: 0,
        }
    }
}

/// Transduce one case's encoded text starting at `b[0]` (just after the
/// opening quote) without knowing where the string ends or the header flag.
/// The first kept record lands in `sc.first`; later records are written
/// straight into `out` behind an assumed header=true prefix
/// (`{"columns":<first>,"rows":[`), so assembling the case needs no copy
/// of the records — `assemble_case` only patches the prefix (a 5-byte
/// shift) when the header turns out false. Errors assume header=false
/// (kept record k -> row k-1). Returns the offset of the closing raw
/// quote in `b`. On Err the partial case in `out` is abandoned with the
/// whole byte path.
fn tr_case_speculative(b: &[u8], sc: &mut CaseScratch, out: &mut Vec<u8>) -> Result<usize, ()> {
    let mut t = Tr {
        b,
        pos: 0,
        line: 1,
        errors: core::mem::take(&mut sc.errors),
        err_count: 0,
        row_toks: core::mem::take(&mut sc.row_toks),
        rec_line: 1,
        rec_row: 0,
        hi: false,
    };
    t.errors.clear();
    t.row_toks.clear();
    let mut first = core::mem::take(&mut sc.first);
    first.clear();
    let mut extra = core::mem::take(&mut sc.extra);
    extra.clear();
    sc.have_first = false;
    sc.phase0_end = 0;
    sc.case_start = out.len();
    sc.rest_start = out.len();

    // Drop a leading U+FEFF (raw EF BB BF, or a \uFEFF escape).
    if b.len() >= 3 && b[0] == 0xEF && b[1] == 0xBB && b[2] == 0xBF {
        t.pos = 3;
    } else if b.len() >= 6 && b[0] == b'\\' && b[1] == b'u' {
        if let Ok(cp) = hex4(&b[2..]) {
            if cp == 0xFEFF {
                t.pos = 6;
            }
        }
    }

    let mut kept: usize = 0;
    let mut width: i64 = -1;

    loop {
        // Lines with no characters are no records.
        while !t.at_end() {
            let (ch, adv, _) = decode_at(t.b, t.pos)?;
            if ch == Ch::Lf {
                t.pos += adv;
                t.line += 1;
            } else if ch == Ch::Cr {
                match t.lf_at(t.pos + adv)? {
                    Some(ladv) => {
                        t.pos += adv + ladv;
                        t.line += 1;
                    }
                    None => break,
                }
            } else {
                break;
            }
        }
        if t.at_end() {
            break;
        }

        t.rec_line = t.line;
        t.rec_row = kept as i64;
        extra.clear();
        let to_first = !sc.have_first;
        let rec_start = if to_first { first.len() } else { out.len() };
        {
            let dst: &mut Vec<u8> = if to_first { &mut first } else { &mut *out };
            if !to_first && dst.len() > sc.rest_start {
                dst.push(b',');
            }
            dst.push(b'[');
        }
        let mut field_count = 0usize;
        let mut last_quoted = false;
        let mut first_field_empty = false;
        loop {
            // pos < len here unless the file ends mid-string (declined at
            // the end of this function); decode_at reads unchecked.
            let quoted = t.pos < t.b.len() && decode_at(t.b, t.pos)?.0 == Ch::Quote;
            last_quoted = quoted;
            let w = if width < 0 {
                usize::MAX
            } else {
                width as usize
            };
            let to_extra = field_count >= w;
            let dst: &mut Vec<u8> = if to_extra {
                &mut extra
            } else if to_first {
                &mut first
            } else {
                &mut *out
            };
            let sep_needed = if to_extra {
                field_count > w
            } else {
                field_count > 0
            };
            if sep_needed {
                dst.push(b',');
            }
            dst.push(b'"');
            let content_start = dst.len();
            let ended = if quoted {
                t.read_quoted(dst)?
            } else {
                t.read_unquoted(dst)?.0
            };
            if field_count == 0 {
                first_field_empty = dst.len() == content_start;
            }
            dst.push(b'"');
            field_count += 1;
            if ended != Ended::Comma {
                break;
            }
        }

        // A line that held only stray CRs is left with no characters: no
        // record (its stray_cr errors stay).
        if field_count == 1 && first_field_empty && !last_quoted {
            if to_first {
                first.truncate(rec_start);
            } else {
                out.truncate(rec_start);
            }
            continue;
        }

        if width < 0 {
            width = field_count as i64;
        }
        let w = width as usize;

        if to_first {
            first.push(b']');
            sc.have_first = true;
            sc.phase0_end = t.errors.len();
            // Assumed header=true prefix; rest records follow in `out`.
            out.extend_from_slice(b"{\"columns\":");
            out.extend_from_slice(&first);
            out.extend_from_slice(b",\"rows\":[");
            sc.rest_start = out.len();
        } else {
            if field_count < w {
                t.err_short(field_count);
                for _ in field_count..w {
                    out.extend_from_slice(b",\"\"");
                }
            } else if field_count > w {
                t.err_open("long_row");
                t.errors.extend_from_slice(b",\"extra\":[");
                t.errors.extend_from_slice(&extra);
                t.errors.extend_from_slice(b"]}");
            }
            out.push(b']');
        }
        kept += 1;
    }

    if !sc.have_first {
        sc.phase0_end = t.errors.len();
        out.extend_from_slice(b"{\"columns\":[],\"rows\":[");
        sc.rest_start = out.len();
    }
    sc.first = first;
    sc.extra = extra;
    sc.errors = t.errors;
    sc.row_toks = t.row_toks;
    if t.pos >= b.len() {
        // No closing quote: malformed JSON string, decline.
        return Err(());
    }
    // Content runs were copied verbatim without per-run validation; if any
    // held a high-bit byte, validate the whole span once now (escape bytes
    // are ASCII, so validating them along is harmless). Invalid UTF-8
    // declines the byte path; the JS fallback's JSON.parse semantics
    // (U+FFFD substitution) then decide the answer.
    if t.hi && !validate_utf8(&b[..t.pos]) {
        return Err(());
    }
    Ok(t.pos)
}

/// Finish a speculatively transduced case whose assumed header=true
/// prefix and rest records already sit in `out` (see tr_case_speculative).
/// header=true: just append the errors, patched (phase-0 row tokens ->
/// null, later rows -> assumed-1). header=false: the prefix becomes
/// `{"columns":null,"rows":[` + first + ',' — 5 bytes longer than the
/// assumed one — so the rest records shift right by 5 in place (or the
/// prefix is rewritten when there are no rest records), and the errors
/// go in as-is. phase0_end is errors.len() when the first kept record
/// completed: errors before it belong to discarded pre-first records or
/// to record 1.
fn assemble_case(sc: &mut CaseScratch, header: bool, out: &mut Vec<u8>) {
    if header {
        out.extend_from_slice(b"],\"errors\":[");
        // Patch row tokens: phase 0 -> null, later rows shift down by one.
        let mut patched = core::mem::take(&mut sc.patched);
        patched.clear();
        let mut last = 0usize;
        for &off in &sc.row_toks {
            patched.extend_from_slice(&sc.errors[last..off]);
            let mut e = off;
            while e < sc.errors.len() && sc.errors[e].is_ascii_digit() {
                e += 1;
            }
            if off < sc.phase0_end {
                patched.extend_from_slice(b"null");
            } else {
                let mut v: u32 = 0;
                for &d in &sc.errors[off..e] {
                    v = v * 10 + (d - b'0') as u32;
                }
                push_u32(&mut patched, v - 1);
            }
            last = e;
        }
        patched.extend_from_slice(&sc.errors[last..]);
        out.extend_from_slice(&patched);
        sc.patched = patched;
        out.extend_from_slice(b"]}");
    } else {
        if out.len() == sc.rest_start {
            // No rest records: rewrite the prefix in place.
            out.truncate(sc.case_start);
            out.extend_from_slice(b"{\"columns\":null,\"rows\":[");
            if sc.have_first {
                out.extend_from_slice(&sc.first);
            }
        } else {
            // Shift the rest records right by 5 and write the real prefix:
            // `{"columns":null,"rows":[` + first + ',' is 5 bytes longer
            // than the assumed `{"columns":` + first + `,"rows":[` (the
            // extra byte is the comma before the first rest record, which
            // is a row here, not the columns).
            let rest_len = out.len() - sc.rest_start;
            out.resize(out.len() + 5, 0);
            out.copy_within(sc.rest_start..sc.rest_start + rest_len, sc.rest_start + 5);
            let mut p = sc.case_start;
            let pre: &[u8] = b"{\"columns\":null,\"rows\":[";
            out[p..p + pre.len()].copy_from_slice(pre);
            p += pre.len();
            out[p..p + sc.first.len()].copy_from_slice(&sc.first);
            p += sc.first.len();
            out[p] = b',';
        }
        out.extend_from_slice(b"],\"errors\":[");
        out.extend_from_slice(&sc.errors);
        out.extend_from_slice(b"]}");
    }
}

// ---------------------------------------------------------------------------
// Byte path: scan the raw cases.json bytes, transduce each case's text, and
// emit the whole answers array. Returns Err(()) on any shape deviation
// (the JS side then falls back to the per-case path).
// ---------------------------------------------------------------------------

struct JsonScan<'a> {
    b: &'a [u8],
    pos: usize,
}

impl<'a> JsonScan<'a> {
    fn ws(&mut self) {
        while self.pos < self.b.len() {
            match self.b[self.pos] {
                b' ' | b'\t' | b'\n' | b'\r' => self.pos += 1,
                _ => break,
            }
        }
    }

    fn expect(&mut self, c: u8) -> Result<(), ()> {
        self.ws();
        if self.pos < self.b.len() && self.b[self.pos] == c {
            self.pos += 1;
            Ok(())
        } else {
            Err(())
        }
    }

    fn peek(&mut self) -> Result<u8, ()> {
        self.ws();
        if self.pos < self.b.len() {
            Ok(self.b[self.pos])
        } else {
            Err(())
        }
    }

    /// Scan a JSON string starting at the opening quote. Returns the raw
    /// content span (between quotes, escapes intact) and whether it contains
    /// a backslash. Advances past the closing quote.
    fn string_span(&mut self) -> Result<(usize, usize, bool), ()> {
        self.ws();
        if self.pos >= self.b.len() || self.b[self.pos] != b'"' {
            return Err(());
        }
        self.pos += 1;
        let start = self.pos;
        let mut esc = false;
        loop {
            if self.pos >= self.b.len() {
                return Err(());
            }
            let hit = find_delimiters(&self.b[self.pos..], [b'"', b'\\', b'"', b'\\']);
            self.pos += hit;
            if self.pos >= self.b.len() {
                return Err(());
            }
            match self.b[self.pos] {
                b'"' => {
                    let end = self.pos;
                    self.pos += 1;
                    return Ok((start, end, esc));
                }
                b'\\' => {
                    esc = true;
                    self.pos += 2; // skip the escaped char (validated on decode)
                    if self.pos > self.b.len() {
                        return Err(());
                    }
                }
                _ => return Err(()),
            }
        }
    }

    /// Skip any JSON value.
    fn skip_value(&mut self, depth: usize) -> Result<(), ()> {
        if depth > 64 {
            return Err(());
        }
        match self.peek()? {
            b'"' => {
                self.string_span()?;
            }
            b'{' => {
                self.pos += 1;
                if self.peek()? == b'}' {
                    self.pos += 1;
                    return Ok(());
                }
                loop {
                    self.string_span()?;
                    self.expect(b':')?;
                    self.skip_value(depth + 1)?;
                    match self.peek()? {
                        b',' => self.pos += 1,
                        b'}' => {
                            self.pos += 1;
                            return Ok(());
                        }
                        _ => return Err(()),
                    }
                }
            }
            b'[' => {
                self.pos += 1;
                if self.peek()? == b']' {
                    self.pos += 1;
                    return Ok(());
                }
                loop {
                    self.skip_value(depth + 1)?;
                    match self.peek()? {
                        b',' => self.pos += 1,
                        b']' => {
                            self.pos += 1;
                            return Ok(());
                        }
                        _ => return Err(()),
                    }
                }
            }
            _ => {
                // number / true / false / null: run to a delimiter.
                while self.pos < self.b.len() {
                    match self.b[self.pos] {
                        b',' | b']' | b'}' | b' ' | b'\t' | b'\n' | b'\r' => break,
                        _ => self.pos += 1,
                    }
                }
            }
        }
        Ok(())
    }
}

/// SIMD UTF-8 validation (Keiser-Lemire style): shuffle-table sequence
/// lengths from the three preceding bytes decide where continuation bytes
/// must appear; second-byte range checks reject overlong forms, surrogates
/// and values above U+10FFFF. Acceptance is exactly core::str::from_utf8's.
/// Pure-ASCII blocks at a sequence boundary skip the check ops. A trailing
/// zero block (or zero padding) makes a sequence truncated at the slice
/// end show up as a missing continuation.
fn validate_utf8(b: &[u8]) -> bool {
    // Sequence length starting with a byte of this high nibble (0 for a
    // continuation byte, 1 for ASCII, 2/3/4 for leads).
    const FIRST_LEN: [u8; 16] = [1, 1, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 2, 2, 3, 4];
    let len = b.len();
    let mut i = 0usize;
    unsafe {
        let table = v128_load(FIRST_LEN.as_ptr() as *const v128);
        let v0f = i8x16_splat(0x0F);
        let vc0 = i8x16_splat(0xC0u8 as i8);
        let v80 = i8x16_splat(0x80u8 as i8);
        let ve0 = i8x16_splat(0xE0u8 as i8);
        let va0 = i8x16_splat(0xA0u8 as i8);
        let vf0 = i8x16_splat(0xF0u8 as i8);
        let vfe = i8x16_splat(0xFEu8 as i8);
        let v74 = i8x16_splat(0x74); // 0xF4 ^ 0x80, for the unsigned > F4 test
        let v_ed = i8x16_splat(0xEDu8 as i8);
        let v_f4 = i8x16_splat(0xF4u8 as i8);
        let v2 = i8x16_splat(2);
        let v3 = i8x16_splat(3);
        let v4 = i8x16_splat(4);
        let mut prev = i8x16_splat(0);
        // FIRST_LEN of the previous block's bytes (for the shifted lookups).
        let mut prev_l = i8x16_splat(1);
        let mut err = i8x16_splat(0);
        // True when the byte before the current block is ASCII, i.e. the
        // block starts at a sequence boundary (given no error so far).
        let mut prev_ascii = true;
        // True when the previous block holds a byte >= 0xE0 (a lead whose
        // second byte has a restricted range may sit at its end).
        let mut prev_special = false;
        // One block of the check; `prev`/`prev_l` describe the block before
        // `cur`. The second-byte range checks run only when a special lead
        // (E0/ED/F0/F4, gated coarsely by >= 0xE0) is in play.
        macro_rules! check_block {
            ($cur:expr) => {{
                let cur = $cur;
                let l = i8x16_swizzle(table, v128_and(i8x16_shr(cur, 4), v0f));
                let len1 = i8x16_shuffle::<15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30>(prev_l, l);
                let len2 = i8x16_shuffle::<14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29>(prev_l, l);
                let len3 = i8x16_shuffle::<13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28>(prev_l, l);
                let expect = v128_or(
                    v128_or(i8x16_ge(len1, v2), i8x16_ge(len2, v3)),
                    i8x16_ge(len3, v4),
                );
                let is_cont = i8x16_eq(v128_and(cur, vc0), v80);
                let mut e = v128_xor(is_cont, expect);
                // Invalid leads: C0/C1 (overlong), F5..FF (above U+10FFFF;
                // unsigned compare via the sign-flip xor).
                e = v128_or(e, i8x16_eq(v128_and(cur, vfe), vc0));
                e = v128_or(e, i8x16_gt(v128_xor(cur, v80), v74));
                // Second-byte ranges: E0 -> A0..BF, ED -> 80..9F,
                // F0 -> 90..BF, F4 -> 80..8F.
                let cur_special = i8x16_bitmask(i8x16_ge(v128_xor(cur, v80), i8x16_splat(0x60))) != 0;
                if prev_special || cur_special {
                    let prev1 = i8x16_shuffle::<15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30>(prev, cur);
                    e = v128_or(e, v128_and(i8x16_eq(prev1, ve0), v128_not(i8x16_eq(v128_and(cur, ve0), va0))));
                    e = v128_or(e, v128_and(i8x16_eq(prev1, v_ed), v128_not(i8x16_eq(v128_and(cur, ve0), v80))));
                    e = v128_or(e, v128_and(i8x16_eq(prev1, vf0), i8x16_eq(v128_and(cur, vf0), v80)));
                    e = v128_or(e, v128_and(i8x16_eq(prev1, v_f4), v128_not(i8x16_eq(v128_and(cur, vf0), v80))));
                }
                err = v128_or(err, e);
                prev = cur;
                prev_l = l;
                prev_special = cur_special;
            }};
        }
        while i + 16 <= len {
            let cur = v128_load(b.as_ptr().add(i) as *const v128);
            if prev_ascii && i8x16_bitmask(cur) == 0 {
                // Pure-ASCII block at a sequence boundary: nothing to check.
                prev = cur;
                prev_l = i8x16_splat(1);
                prev_special = false;
            } else {
                check_block!(cur);
                prev_ascii = i8x16_extract_lane::<15>(cur) >= 0;
            }
            i += 16;
        }
        if i < len {
            let mut buf = [0u8; 16];
            buf[..len - i].copy_from_slice(&b[i..]);
            let cur = v128_load(buf.as_ptr() as *const v128);
            check_block!(cur);
        } else if !prev_ascii {
            // Truncation check: a zero block flags a sequence left open.
            check_block!(i8x16_splat(0));
        }
        i8x16_bitmask(err) == 0
    }
}

fn hex4(b: &[u8]) -> Result<u32, ()> {
    if b.len() < 4 {
        return Err(());
    }
    let mut v = 0u32;
    for &c in &b[..4] {
        v <<= 4;
        v |= match c {
            b'0'..=b'9' => (c - b'0') as u32,
            b'a'..=b'f' => (c - b'a' + 10) as u32,
            b'A'..=b'F' => (c - b'A' + 10) as u32,
            _ => return Err(()),
        };
    }
    Ok(v)
}

fn push_utf8(out: &mut Vec<u8>, cp: u32) {
    if cp < 0x80 {
        out.push(cp as u8);
    } else if cp < 0x800 {
        out.push(0xC0 | (cp >> 6) as u8);
        out.push(0x80 | (cp & 0x3F) as u8);
    } else if cp < 0x10000 {
        out.push(0xE0 | (cp >> 12) as u8);
        out.push(0x80 | ((cp >> 6) & 0x3F) as u8);
        out.push(0x80 | (cp & 0x3F) as u8);
    } else {
        out.push(0xF0 | (cp >> 18) as u8);
        out.push(0x80 | ((cp >> 12) & 0x3F) as u8);
        out.push(0x80 | ((cp >> 6) & 0x3F) as u8);
        out.push(0x80 | (cp & 0x3F) as u8);
    }
}

/// Decode a JSON string content span (escapes intact) into UTF-8 bytes.
/// Lone surrogates and invalid escapes are Err (caller falls back).
fn decode_json_string(raw: &[u8], out: &mut Vec<u8>) -> Result<(), ()> {
    let mut i = 0usize;
    loop {
        let hit = find_delimiters(&raw[i..], [b'\\', b'\\', b'\\', b'\\']);
        let seg_end = i + hit;
        if seg_end > i {
            out.extend_from_slice(&raw[i..seg_end]);
            i = seg_end;
        }
        if i >= raw.len() {
            return Ok(());
        }
        // raw[i] == '\\'
        i += 1;
        if i >= raw.len() {
            return Err(());
        }
        match raw[i] {
            b'"' => out.push(b'"'),
            b'\\' => out.push(b'\\'),
            b'/' => out.push(b'/'),
            b'b' => out.push(0x08),
            b'f' => out.push(0x0C),
            b'n' => out.push(b'\n'),
            b'r' => out.push(b'\r'),
            b't' => out.push(b'\t'),
            b'u' => {
                let cp = hex4(&raw[i + 1..])?;
                i += 4;
                if (0xD800..0xDC00).contains(&cp) {
                    // High surrogate: a \uDC00-\uDFFF must follow.
                    if i + 2 < raw.len() && raw[i + 1] == b'\\' && raw[i + 2] == b'u' {
                        let lo = hex4(&raw[i + 3..])?;
                        if !(0xDC00..0xE000).contains(&lo) {
                            return Err(());
                        }
                        i += 6;
                        push_utf8(out, 0x10000 + ((cp - 0xD800) << 10) + (lo - 0xDC00));
                    } else {
                        return Err(());
                    }
                } else if (0xDC00..0xE000).contains(&cp) {
                    return Err(());
                } else {
                    push_utf8(out, cp);
                }
            }
            _ => return Err(()),
        }
        i += 1;
    }
}

/// Decode a short key span into a stack buffer; returns the byte length.
fn decode_key(raw: &[u8], buf: &mut [u8; 32]) -> Result<usize, ()> {
    let mut v: Vec<u8> = Vec::new();
    decode_json_string(raw, &mut v)?;
    if v.len() > buf.len() {
        return Err(());
    }
    buf[..v.len()].copy_from_slice(&v);
    Ok(v.len())
}

/// The byte path over a whole cases.json array. Emits `[answer,...]`.
fn run_all_into(raw: &[u8], out: &mut Vec<u8>) -> Result<(), ()> {
    let mut s = JsonScan { b: raw, pos: 0 };
    let mut keybuf = [0u8; 32];
    let mut sc = CaseScratch::new();
    s.expect(b'[')?;
    out.push(b'[');
    let mut first = true;
    if s.peek()? == b']' {
        s.pos += 1;
        out.push(b']');
        return Ok(());
    }
    loop {
        // One case object: find its "input" object. The separating comma
        // goes in now because the case's records are written straight
        // into `out` during transduction; on any Err the whole byte path
        // is abandoned, so a stray comma never reaches the output.
        s.expect(b'{')?;
        if !first {
            out.push(b',');
        }
        first = false;
        let mut have_text = false;
        let mut header = false;
        let mut have_input = false;
        if s.peek()? == b'}' {
            s.pos += 1;
        } else {
            loop {
                let (ks, ke, _) = s.string_span()?;
                let klen = decode_key(&raw[ks..ke], &mut keybuf)?;
                let key = &keybuf[..klen];
                s.expect(b':')?;
                if key == b"input" {
                    have_input = true;
                    s.expect(b'{')?;
                    if s.peek()? == b'}' {
                        s.pos += 1;
                    } else {
                        loop {
                            let (ks2, ke2, _) = s.string_span()?;
                            let klen2 = decode_key(&raw[ks2..ke2], &mut keybuf)?;
                            let key2 = &keybuf[..klen2];
                            s.expect(b':')?;
                            if key2 == b"text" {
                                s.ws();
                                if s.pos >= raw.len() || raw[s.pos] != b'"' {
                                    return Err(());
                                }
                                let text_start = s.pos + 1;
                                if have_text {
                                    // Duplicate "text" key: last wins
                                    // (JSON.parse semantics) — drop the
                                    // earlier transduction from `out`.
                                    out.truncate(sc.case_start);
                                }
                                let end = tr_case_speculative(&raw[text_start..], &mut sc, out)?;
                                s.pos = text_start + end + 1;
                                have_text = true;
                            } else if key2 == b"header" {
                                s.ws();
                                if raw[s.pos..].starts_with(b"true") {
                                    header = true;
                                    s.pos += 4;
                                } else if raw[s.pos..].starts_with(b"false") {
                                    header = false;
                                    s.pos += 5;
                                } else {
                                    return Err(());
                                }
                            } else {
                                s.skip_value(0)?;
                            }
                            match s.peek()? {
                                b',' => s.pos += 1,
                                b'}' => {
                                    s.pos += 1;
                                    break;
                                }
                                _ => return Err(()),
                            }
                        }
                    }
                } else {
                    s.skip_value(0)?;
                }
                match s.peek()? {
                    b',' => s.pos += 1,
                    b'}' => {
                        s.pos += 1;
                        break;
                    }
                    _ => return Err(()),
                }
            }
        }
        if !have_input || !have_text {
            return Err(());
        }
        assemble_case(&mut sc, header, out);
        match s.peek()? {
            b',' => s.pos += 1,
            b']' => {
                s.pos += 1;
                break;
            }
            _ => return Err(()),
        }
    }
    out.push(b']');
    Ok(())
}

// ---------------------------------------------------------------------------
// Exports (JS side)
// ---------------------------------------------------------------------------

/// Ensure the input buffer holds at least `len` bytes; returns its pointer.
/// JS writes the case's UTF-8 bytes there, then calls `run_case`. The
/// length is set without zero-filling: JS overwrites every byte it later
/// asks the module to read (run_case/run_all slice by explicit length).
#[no_mangle]
pub extern "C" fn alloc_input(len: u32) -> u32 {
    unsafe {
        let input = &mut *core::ptr::addr_of_mut!(INPUT);
        if input.capacity() < len as usize {
            input.reserve(len as usize - input.capacity());
        }
        input.set_len(len as usize);
        input.as_mut_ptr() as u32
    }
}

/// Parse the first `len` input bytes as one case; emit its answer JSON into
/// the output buffer and return its length.
#[no_mangle]
pub extern "C" fn run_case(len: u32, header: u32) -> u32 {
    unsafe {
        let input = &*core::ptr::addr_of!(INPUT);
        let output = &mut *core::ptr::addr_of_mut!(OUTPUT);
        output.clear();
        let bytes: &[u8] = core::slice::from_raw_parts(input.as_ptr(), len as usize);
        run_case_into(bytes, header != 0, output);
        output.len() as u32
    }
}

/// Byte path: the first `len` input bytes are a whole cases.json array.
/// Emits the answers array JSON into the output buffer and returns its
/// length, or u32::MAX when the input is not the expected shape (the JS
/// side then falls back to the per-case path).
#[no_mangle]
pub extern "C" fn run_all(len: u32) -> u32 {
    unsafe {
        let input = &*core::ptr::addr_of!(INPUT);
        let output = &mut *core::ptr::addr_of_mut!(OUTPUT);
        output.clear();
        // Answers run slightly larger than the input; reserve headroom so
        // the output Vec does not grow (and copy) mid-emission.
        let want = len as usize + (len as usize) / 2;
        if output.capacity() < want {
            output.reserve(want - output.capacity());
        }
        let bytes: &[u8] = core::slice::from_raw_parts(input.as_ptr(), len as usize);
        match run_all_into(bytes, output) {
            Ok(()) => output.len() as u32,
            Err(()) => u32::MAX,
        }
    }
}

#[no_mangle]
pub extern "C" fn output_ptr() -> u32 {    unsafe {
        let output = &*core::ptr::addr_of!(OUTPUT);
        output.as_ptr() as u32
    }
}
