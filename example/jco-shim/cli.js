// Minimal WASI P3 CLI shim for jco-transpiled Wado programs
//
// Uses a global stream write hook to intercept bulk data from stream.write,
// bypassing the jco byte-at-a-time rendezvous mechanism.

// Map: writable end waitable index -> output target
const _streamTargets = new Map();

// Global hook: called by patched jco's streamWrite when data is written
globalThis._jcoStreamWriteHook = (writableEndIdx, data) => {
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

// `write-via-stream` is an async WASI function, so jco lowers its result as a
// future and expects the host to return a Promise/Thenable. The actual bytes
// are delivered out-of-band through `_jcoStreamWriteHook`, so the returned
// promise only needs to resolve to the operation result.
export const stdout = {
  writeViaStream(_stream) {
    _defaultTarget = process.stdout;
    return Promise.resolve({ tag: "ok" });
  },
};
stdout.writeViaStream._isHostProvided = true;

export const stderr = {
  writeViaStream(_stream) {
    _defaultTarget = process.stderr;
    return Promise.resolve({ tag: "ok" });
  },
};
stderr.writeViaStream._isHostProvided = true;

// Default target for unregistered writable ends
let _defaultTarget = process.stdout;

// Override the hook to auto-register unknown writable ends
const _origHook = globalThis._jcoStreamWriteHook;
globalThis._jcoStreamWriteHook = (writableEndIdx, data) => {
  if (!_streamTargets.has(writableEndIdx) && _defaultTarget) {
    _streamTargets.set(writableEndIdx, _defaultTarget);
  }
  return _origHook(writableEndIdx, data);
};
