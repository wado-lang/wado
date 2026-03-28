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

// Track which readable end idx maps to which output target
const _pendingTargets = new Map();

export const types = {
  OutputStream: class OutputStream {},
};

export const stdout = {
  writeViaStream(stream) {
    // The stream's globalRep identifies the readable end in the global stream map.
    // We need the writable end's waitable idx. In jco's model, stream.new creates
    // both ends as consecutive waitable handles. The readable end's idx is passed
    // as arg0 to the lowered import. We register a "pending" target and resolve it
    // when streamWrite is called with any writable idx we haven't seen.
    //
    // Simple approach: mark that the NEXT stream write should go to stdout.
    _defaultTarget = process.stdout;
    return { tag: "ok" };
  },
};
stdout.writeViaStream._isHostProvided = true;

export const stderr = {
  writeViaStream(_stream) {
    _defaultTarget = process.stderr;
    return { tag: "ok" };
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
