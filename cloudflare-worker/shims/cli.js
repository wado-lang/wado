// wasi:cli for a Worker: each `write-via-stream` drains to `console`.
//
// A program that writes after `task return` — an access log, say — does not
// reach here: once `handle` resolves the Worker returns, and the guest's
// remaining work is never pumped.

async function drain(kind, streamReader) {
  const decoder = new TextDecoder();
  let text = "";
  try {
    for await (const chunk of streamReader) text += decoder.decode(chunk, { stream: true });
    text += decoder.decode();
    if (text) console.log(`[${kind}] ${text.trimEnd()}`);
    return { tag: "ok" };
  } catch (err) {
    return { tag: "err", val: String(err?.message ?? err) };
  }
}

export const types = { OutputStream: class OutputStream {} };

export const stdout = { writeViaStream: (stream) => drain("stdout", stream) };
stdout.writeViaStream._isHostProvided = true;

export const stderr = { writeViaStream: (stream) => drain("stderr", stream) };
stderr.writeViaStream._isHostProvided = true;
