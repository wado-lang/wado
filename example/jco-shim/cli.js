// Minimal WASI P3 CLI shim for jco-transpiled Wado programs
//
// Works around the jco P3 async stream deadlock by starting concurrent
// read loops that drain the stream while the wasm writes to it.

export const types = {
  OutputStream: class OutputStream {},
};

export const stdout = {
  writeViaStream(stream) {
    // Start reading from the stream concurrently
    // We DON'T await this - let it run in the background
    drainStream(stream, process.stdout);
    // Return a future-like result
    return { tag: "ok" };
  },
};
stdout.writeViaStream._isHostProvided = true;

export const stderr = {
  writeViaStream(stream) {
    drainStream(stream, process.stderr);
    return { tag: "ok" };
  },
};
stderr.writeViaStream._isHostProvided = true;

function drainStream(stream, output) {
  // Use setImmediate/queueMicrotask to start reading after current frame
  queueMicrotask(async () => {
    try {
      while (true) {
        const chunk = await stream.next();
        if (chunk === undefined) break;
        output.write(chunk);
      }
    } catch (_e) {
      // Stream closed or errored
    }
  });
}
