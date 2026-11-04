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
