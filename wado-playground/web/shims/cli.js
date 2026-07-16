// Browser WASI P3 CLI shim for jco-transpiled Wado programs.
//
// Mirrors example/jco-shim/cli.js but targets the browser: stdout/stderr bytes
// captured via `_jcoStreamWriteHook` are decoded and delivered to
// `globalThis._wadoWrite(kind, text)` (kind = "stdout" | "stderr") instead of
// Node's process streams.

const _decoder = new TextDecoder();

function _sink(kind) {
  return {
    write(data) {
      const text = _decoder.decode(data, { stream: true });
      if (typeof globalThis._wadoWrite === "function") globalThis._wadoWrite(kind, text);
    },
  };
}

const _streamTargets = new Map();
let _defaultTarget = _sink("stdout");

globalThis._jcoStreamWriteHook = (writableEndIdx, data) => {
  if (!_streamTargets.has(writableEndIdx) && _defaultTarget) {
    _streamTargets.set(writableEndIdx, _defaultTarget);
  }
  const target = _streamTargets.get(writableEndIdx);
  if (target) {
    target.write(data);
    return true;
  }
  return false;
};

export const types = {
  OutputStream: class OutputStream {},
};

export const stdout = {
  writeViaStream(_stream) {
    _defaultTarget = _sink("stdout");
    return Promise.resolve({ tag: "ok" });
  },
};
stdout.writeViaStream._isHostProvided = true;

export const stderr = {
  writeViaStream(_stream) {
    _defaultTarget = _sink("stderr");
    return Promise.resolve({ tag: "ok" });
  },
};
stderr.writeViaStream._isHostProvided = true;
