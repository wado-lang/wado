// A `crate::` or `super::` path belongs in a `use` item, not inline where the
// item is read (AGENTS.md > General Rules, which also says why clippy is not
// the gate).
//
// This is the edit-time guard's scanner only (`.claude/hooks/`), which has to
// answer inside a keystroke. The rule's authority is
// `package-gale/tools/rust_inline_paths.wado`, which parses with the Gale Rust
// grammar and owns `rust-inline-paths.json`; run it through
// `scripts/check-rust-paths.sh`. The two agree over the whole corpus — where
// they ever disagree, the parser is right.

// A Rust identifier runs over `XID_Continue`, so `αcrate` is one name and not
// a path root. `$crate` is macro hygiene, which no `use` can replace, and
// `$use` is a macro metavariable rather than an import.
const INLINE_PATH = /(?<![\p{XID_Continue}#$])(?:crate|super)::/gu;
const USE_KEYWORD = /(?<![\p{XID_Continue}#$])use(?![\p{XID_Continue}])/gu;

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
 * The source with comments and literals blanked out, every other byte and every
 * newline in place, so an offset into the result is an offset into the source.
 */
export function stripNonCode(source) {
  const out = source.split("");
  const blank = (from, to) => {
    for (let i = from; i < to; i++) {
      if (out[i] !== "\n") out[i] = " ";
    }
  };
  let i = 0;
  while (i < source.length) {
    const pair = source.slice(i, i + 2);
    if (pair === "//") {
      const newline = source.indexOf("\n", i);
      const end = newline < 0 ? source.length : newline;
      blank(i, end);
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
      blank(i, j);
      i = j;
    } else if (source[i] === "'") {
      const end = charLiteralEnd(source, i);
      if (end < 0) {
        i++;
      } else {
        blank(i, end);
        i = end;
      }
    } else {
      const end = stringLiteralEnd(source, i);
      if (end < 0) {
        i++;
      } else {
        blank(i, end);
        i = end;
      }
    }
  }
  return out.join("");
}

/** The first non-whitespace character at or after `at`, or "" past the end. */
function nextNonSpace(code, at) {
  let i = at;
  while (i < code.length && /\s/.test(code[i])) i++;
  return code[i] ?? "";
}

/** The half-open span of every `use` item, which may name a path freely. */
function useItemSpans(code) {
  const spans = [];
  for (const match of code.matchAll(USE_KEYWORD)) {
    // `impl Trait + use<'a, T>` is a capture list, not an import. Rust allows
    // any whitespace before the `<`, so the scan cannot be given a width.
    if (nextNonSpace(code, match.index + 3) === "<") continue;
    const semicolon = code.indexOf(";", match.index);
    spans.push([match.index, semicolon < 0 ? code.length : semicolon]);
  }
  return spans;
}

/** Every `crate::` / `super::` written outside a `use` item, in source order. */
export function findInlinePaths(source) {
  const code = stripNonCode(source);
  const spans = useItemSpans(code);
  const lineStarts = [0];
  for (let i = 0; i < code.length; i++) {
    if (code[i] === "\n") lineStarts.push(i + 1);
  }
  const hits = [];
  let line = 1;
  for (const match of code.matchAll(INLINE_PATH)) {
    while (lineStarts[line] !== undefined && lineStarts[line] <= match.index) line++;
    if (spans.some(([from, to]) => match.index >= from && match.index < to)) continue;
    hits.push({ line, column: match.index - lineStarts[line - 1] + 1, text: match[0] });
  }
  return hits;
}
