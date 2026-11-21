// Fast drop-in for csv-reader 1.0.0: returns exactly the same {columns, rows, errors} for every
// input, but scans with indexOf/regex and slices whole field runs instead of building each field
// one character at a time.
//
// Policy (identical to the reference):
//   - a U+FEFF at the very start of the text is dropped;
//   - a record ends at LF or CRLF, and a line left with no characters is no record;
//   - outside a quoted field a CR not followed by LF is dropped (stray_cr);
//   - a field that opens with a quote runs to the next quote that is not doubled, "" inside it one
//     quote, and each CRLF or lone CR inside it is read as LF; what follows its closing quote, up
//     to the next comma or line break, is appended to it as it stands (quote_junk, when the field
//     ends); a quote that never closes is read as a literal character and the text after it is
//     read again as an unquoted field (unterminated_quote);
//   - a quote inside an unquoted field is a literal character; fields stay strings, never trimmed
//     or converted;
//   - columns is the first record when header is true, else null; rows holds the records after it
//     (every record when header is false), each padded with "" (short_row, with fields: its count)
//     or cut (long_row, with extra: the cut fields) to the first record's width;
//   - each error is {code, line, row}: line the 1-based line its record starts on (every LF, CRLF
//     and lone CR inside a quoted field counted), row its record's index in rows (null for the
//     header), in the order met, a field's quote_junk when the field ends and a record's
//     short_row or long_row when the record ends.
//
// Pure: no clock, no randomness, no locale, nothing kept from one call to the next.

const COMMA = 0;
const LINE_BREAK = 1;
const END = 2;

/** The reading of one CSV input, {text, header}: {columns, rows, errors}. */
export function run(input) {
  const text = String(input.text);
  const header = input.header === true;
  const n = text.length;
  const errors = [];
  const rows = [];
  let columns = null;
  let width = -1;

  let pos = 0; // the next character to read
  let line = 1; // the line that character is on
  if (text.charCodeAt(0) === 0xfeff) pos = 1;

  // The record being read: the line it starts on and the index in rows it takes (null: the header).
  let recordLine = 1;
  let recordRow = null;

  // Reads unquoted characters onto `value` up to the next comma or line break: a CR not followed
  // by LF is dropped (stray_cr). Returns [value, ended, added]; `added` counts the characters kept.
  function readUnquoted(value) {
    let added = 0;
    let start = pos;
    let pieces = null;
    let i = pos;
    for (;;) {
      while (i < n) {
        const c = text.charCodeAt(i);
        if (c === 44 || c === 10 || c === 13) break;
        i += 1;
      }
      if (i >= n) {
        const tail = text.slice(start);
        added += tail.length;
        pos = n;
        return [pieces === null ? value + tail : value + pieces.join('') + tail, END, added];
      }
      const c = text.charCodeAt(i);
      if (c === 44) {
        // ','
        const tail = text.slice(start, i);
        added += tail.length;
        pos = i + 1;
        return [pieces === null ? value + tail : value + pieces.join('') + tail, COMMA, added];
      }
      if (c === 10) {
        // '\n'
        const tail = text.slice(start, i);
        added += tail.length;
        pos = i + 1;
        line += 1;
        return [pieces === null ? value + tail : value + pieces.join('') + tail, LINE_BREAK, added];
      }
      // '\r'
      if (text.charCodeAt(i + 1) === 10) {
        const tail = text.slice(start, i);
        added += tail.length;
        pos = i + 2;
        line += 1;
        return [pieces === null ? value + tail : value + pieces.join('') + tail, LINE_BREAK, added];
      }
      errors.push({ code: 'stray_cr', line: recordLine, row: recordRow });
      if (pieces === null) pieces = [];
      pieces.push(text.slice(start, i));
      added += i - start;
      i += 1;
      start = i;
    }
  }

  // Reads a quoted field whose opening quote is at pos. Returns [value, ended].
  function readQuoted() {
    const open = pos;
    const openLine = line;
    // Find the closing quote: the next quote that is not doubled.
    let scan = pos + 1;
    let close = -1;
    for (;;) {
      const q = text.indexOf('"', scan);
      if (q === -1) break;
      if (text.charCodeAt(q + 1) === 34) {
        scan = q + 2;
        continue;
      }
      close = q;
      break;
    }
    if (close === -1) {
      // The quote never closes: it is a literal character, and the text after it is read again as unquoted.
      pos = open + 1;
      line = openLine;
      errors.push({ code: 'unterminated_quote', line: recordLine, row: recordRow });
      const r = readUnquoted('"');
      return [r[0], r[1]];
    }
    // The raw content is text[open + 1, close). Inside it, "" is one quote and each CRLF or lone
    // CR is one LF; every LF, CRLF and lone CR counts a line.
    const segStart = open + 1;
    let value;
    const seg = text.slice(segStart, close);
    if (!seg.includes('\r') && !seg.includes('""')) {
      // Fast path: nothing to rewrite; only count the LFs.
      value = seg;
      let li = 0;
      for (;;) {
        li = seg.indexOf('\n', li);
        if (li === -1) break;
        line += 1;
        li += 1;
      }
    } else {
      // Slow path: rewrite doubled quotes and CRs, counting lines as we go.
      const pieces = [];
      let start = segStart;
      let i = segStart;
      while (i < close) {
        const c = text.charCodeAt(i);
        if (c === 34) {
          // A doubled quote (a lone one here is the closing quote, which ends the segment).
          pieces.push(text.slice(start, i), '"');
          i += 2;
          start = i;
        } else if (c === 13) {
          pieces.push(text.slice(start, i), '\n');
          line += 1;
          i += text.charCodeAt(i + 1) === 10 ? 2 : 1;
          start = i;
        } else {
          if (c === 10) line += 1;
          i += 1;
        }
      }
      pieces.push(text.slice(start, close));
      value = pieces.join('');
    }
    pos = close + 1;
    if (pos >= n) return [value, END];
    const c = text.charCodeAt(pos);
    if (c === 44) {
      pos += 1;
      return [value, COMMA];
    }
    if (c === 10) {
      pos += 1;
      line += 1;
      return [value, LINE_BREAK];
    }
    if (c === 13 && text.charCodeAt(pos + 1) === 10) {
      pos += 2;
      line += 1;
      return [value, LINE_BREAK];
    }
    // What follows the closing quote is appended as it stands, up to the next comma or line break.
    const junk = readUnquoted(value);
    if (junk[2] > 0) errors.push({ code: 'quote_junk', line: recordLine, row: recordRow });
    return [junk[0], junk[1]];
  }

  for (;;) {
    // Lines with no characters are no records.
    while (pos < n) {
      const c = text.charCodeAt(pos);
      if (c === 10) {
        pos += 1;
        line += 1;
      } else if (c === 13 && text.charCodeAt(pos + 1) === 10) {
        pos += 2;
        line += 1;
      } else {
        break;
      }
    }
    if (pos >= n) break;

    recordLine = line;
    recordRow = header && columns === null ? null : rows.length;
    const fields = [];
    let ended;
    let quoted;
    do {
      quoted = text.charCodeAt(pos) === 34;
      const field = quoted ? readQuoted() : readUnquoted('');
      fields.push(field[0]);
      ended = field[1];
    } while (ended === COMMA);

    // A line that held only stray CRs is left with no characters: no record (its stray_cr errors stay).
    if (fields.length === 1 && fields[0] === '' && !quoted) continue;

    if (width < 0) width = fields.length;
    if (header && columns === null) {
      columns = fields;
      continue;
    }
    if (fields.length < width) {
      errors.push({ code: 'short_row', line: recordLine, row: recordRow, fields: fields.length });
      while (fields.length < width) fields.push('');
    } else if (fields.length > width) {
      errors.push({ code: 'long_row', line: recordLine, row: recordRow, extra: fields.splice(width) });
    }
    rows.push(fields);
  }
  return { columns: header ? (columns ?? []) : null, rows, errors };
}
