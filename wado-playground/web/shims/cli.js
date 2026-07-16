// Browser WASI P3 CLI shim for jco-transpiled Wado programs.
//
// stdout/stderr bytes captured via `_jcoStreamWriteHook` are decoded and
// delivered to `globalThis._wadoWrite(kind, text)` (kind = "stdout" | "stderr").
//
// jco's write hook identifies a stream only by its writable-end index, and jco
// recycles those indices once a stream is dropped — so a permanent
// index→target map mis-routes a later stream that reuses an old index (this is
// how stderr bytes leaked into stdout). We instead track the most recently
// opened stream: `println`/`eprintln` open a stream, write, and drop it, so the
// last `writeViaStream` before a write is always the right target. Each open
// gets its own `TextDecoder`, so a multi-byte glyph split across writes on one
// stream decodes intact and never bleeds into the other stream.

function _sink(kind) {
  const decoder = new TextDecoder();
  return {
    write(data) {
      const text = decoder.decode(data, { stream: true });
      if (text && typeof globalThis._wadoWrite === "function") globalThis._wadoWrite(kind, text);
    },
  };
}

let _current = _sink("stdout");

globalThis._jcoStreamWriteHook = (_writableEndIdx, data) => {
  _current.write(data);
  return true;
};

export const types = {
  OutputStream: class OutputStream {},
};

export const stdout = {
  writeViaStream(_stream) {
    _current = _sink("stdout");
    return Promise.resolve({ tag: "ok" });
  },
};
stdout.writeViaStream._isHostProvided = true;

export const stderr = {
  writeViaStream(_stream) {
    _current = _sink("stderr");
    return Promise.resolve({ tag: "ok" });
  },
};
stderr.writeViaStream._isHostProvided = true;
