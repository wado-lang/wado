// wasi:cli for a Worker: each `write-via-stream` drains to `console`.
// A write issued after `task return` lands only while `worker.mjs` holds the
// request open, which it does by waiting on `settled()`.

const inFlight = new Set();

async function drain(kind, streamReader) {
  const decoder = new TextDecoder();
  let text = "";
  try {
    for await (const chunk of streamReader) text += decoder.decode(chunk, { stream: true });
    text += decoder.decode();
    if (text) console.log(`[${kind}] ${text.trimEnd()}`);
    return { tag: "ok" };
  } catch (err) {
    // `error-code` is an enum; an arbitrary string cannot be lowered.
    return { tag: "err", val: err?.code === "EPIPE" ? "pipe" : "io" };
  }
}

function track(kind, stream) {
  const done = drain(kind, stream);
  inFlight.add(done);
  return done.finally(() => inFlight.delete(done));
}

/// Every write that has started, once each has landed.
export function settled() {
  return Promise.allSettled([...inFlight]);
}

export const types = { OutputStream: class OutputStream {} };

export const stdout = { writeViaStream: (stream) => track("stdout", stream) };
stdout.writeViaStream._isHostProvided = true;

export const stderr = { writeViaStream: (stream) => track("stderr", stream) };
stderr.writeViaStream._isHostProvided = true;
