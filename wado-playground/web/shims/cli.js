// Browser WASI P3 CLI shim: bytes from `_jcoStreamWriteHook` are decoded and
// delivered to `globalThis._wadoWrite(kind, text)`.
//
// jco identifies a write only by a writable-end index and recycles those, so a
// permanent index→target map mis-routes (stderr leaked into stdout). We target
// the most recently opened stream instead — `println`/`eprintln` open, write,
// and drop, so the last `writeViaStream` is the right target. Each open gets its
// own `TextDecoder`, so multi-byte glyphs don't corrupt or cross streams.

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
