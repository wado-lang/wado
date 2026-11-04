// `text` as a Wado string literal, everything outside printable ASCII escaped.
export function wadoLiteral(text) {
  let out = '"';
  for (const c of text) {
    const code = c.codePointAt(0);
    if (c === '"' || c === "\\") {
      out += `\\${c}`;
    } else if (code >= 0x20 && code < 0x7f) {
      out += c;
    } else {
      out += `\\u{${code.toString(16).toUpperCase()}}`;
    }
  }
  return `${out}"`;
}

// A pattern as a Wado raw string, so that it reads as the pattern it is. One
// holding a control character, which a raw string would carry unseen, is
// escaped instead.
export function wadoPattern(text) {
  if (/[\u0000-\u001f\u007f]/.test(text)) {
    return wadoLiteral(text);
  }
  let hashes = "#";
  while (text.includes(`"${hashes}`)) {
    hashes += "#";
  }
  return `${hashes}"${text}"${hashes}`;
}
