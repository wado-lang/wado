// WASI P3 CLI shim for jco transpilation
// Provides wasi:cli/stdout and wasi:cli/stderr for Node.js

const encoder = new TextEncoder();

// Stream write hook: jco calls this to write bytes to a stream
globalThis._jcoStreamWriteHook = (streamId, bytes) => {
  // streamId 0 = stdout, streamId 1 = stderr
  if (streamId === 0) {
    process.stdout.write(bytes);
  } else {
    process.stderr.write(bytes);
  }
  return bytes.length;
};

export function writeViaStream() {
  return 0; // stdout stream id
}

export const stdout = {
  writeViaStream() {
    return 0; // stdout stream id
  }
};

export const stderr = {
  writeViaStream() {
    return 1; // stderr stream id
  }
};
