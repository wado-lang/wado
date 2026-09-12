// The Rust reading the static gates share: where the comments and literals are,
// and which files a rule covers.

import { execFileSync } from "node:child_process";

/** Index past the char literal at `at`, or -1 when the quote opens a lifetime. */
function charLiteralEnd(source, at) {
  if (source[at + 1] === "\\") {
    let i = at + 3;
    while (i < source.length && source[i] !== "'") i++;
    return i < source.length ? i + 1 : -1;
  }
  return source[at + 2] === "'" ? at + 3 : -1;
}

/** Index past the string literal at `at`, or -1 when none starts there. */
function stringLiteralEnd(source, at) {
  let i = at;
  if (source[i] === "b" || source[i] === "c") i++;
  const raw = source[i] === "r";
  if (raw) i++;
  let hashes = 0;
  while (raw && source[i] === "#") {
    hashes++;
    i++;
  }
  if (source[i] !== '"') return -1;
  if (raw) {
    const close = `"${"#".repeat(hashes)}`;
    const end = source.indexOf(close, i + 1);
    return end < 0 ? source.length : end + close.length;
  }
  i++;
  while (i < source.length) {
    if (source[i] === "\\") i += 2;
    else if (source[i] === '"') return i + 1;
    else i++;
  }
  return source.length;
}

/**
 * Every comment and literal in `source`, in order, as `{ kind, start, end }`.
 * What lies between them is code.
 */
export function* lexSpans(source) {
  let i = 0;
  while (i < source.length) {
    const pair = source.slice(i, i + 2);
    if (pair === "//") {
      const newline = source.indexOf("\n", i);
      const end = newline < 0 ? source.length : newline;
      yield { kind: "comment", start: i, end };
      i = end;
    } else if (pair === "/*") {
      let depth = 1;
      let j = i + 2;
      while (j < source.length && depth > 0) {
        const inner = source.slice(j, j + 2);
        if (inner === "/*") {
          depth++;
          j += 2;
        } else if (inner === "*/") {
          depth--;
          j += 2;
        } else {
          j++;
        }
      }
      yield { kind: "comment", start: i, end: j };
      i = j;
    } else if (source[i] === "'") {
      const end = charLiteralEnd(source, i);
      if (end < 0) {
        i++;
      } else {
        yield { kind: "char", start: i, end };
        i = end;
      }
    } else {
      const end = stringLiteralEnd(source, i);
      if (end < 0) {
        i++;
      } else {
        yield { kind: "string", start: i, end };
        i = end;
      }
    }
  }
}

/**
 * The source with comments and literals blanked out, every other byte and every
 * newline in place, so an offset into the result is an offset into the source.
 */
export function stripNonCode(source) {
  const out = source.split("");
  for (const { start, end } of lexSpans(source)) {
    for (let i = start; i < end; i++) {
      if (out[i] !== "\n") out[i] = " ";
    }
  }
  return out.join("");
}

/**
 * Every string literal `source` writes as code, as `{ line, text, content }`.
 * One inside a comment is text, not a literal.
 */
export function* stringLiterals(source) {
  let line = 1;
  let at = 0;
  const advance = (to) => {
    for (; at < to; at++) {
      if (source[at] === "\n") line++;
    }
  };
  for (const span of lexSpans(source)) {
    advance(span.start);
    if (span.kind === "string") {
      const text = source.slice(span.start, span.end);
      const open = text.indexOf('"');
      yield { line, text, content: text.slice(open + 1, text.lastIndexOf('"')) };
    }
    advance(span.end);
  }
}

/** Every tracked Rust file, which is the corpus a rule covers. */
export function rustFiles() {
  const listed = execFileSync("git", ["ls-files", "-z", "*.rs"], { encoding: "utf8" });
  return listed.split("\0").filter(Boolean);
}
