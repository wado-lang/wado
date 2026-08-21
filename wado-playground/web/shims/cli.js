// Browser WASI P3 CLI shim: each `write-via-stream` drains to
// `globalThis._wadoWrite(kind, text)`. The stream and its decoder are the
// call's own, so stdout and stderr cross neither channels nor glyphs.

async function drain(kind, streamReader) {
  const decoder = new TextDecoder();
  const emit = (text) => {
    if (text && typeof globalThis._wadoWrite === "function") globalThis._wadoWrite(kind, text);
  };
  try {
    for await (const chunk of streamReader) emit(decoder.decode(chunk, { stream: true }));
    emit(decoder.decode());
    return { tag: "ok" };
  } catch (err) {
    // `error-code` is an enum; an arbitrary string cannot be lowered.
    return { tag: "err", val: err?.code === "EPIPE" ? "pipe" : "io" };
  }
}

export const types = {
  OutputStream: class OutputStream {},
};

export const stdout = {
  writeViaStream(streamReader) {
    return drain("stdout", streamReader);
  },
};
stdout.writeViaStream._isHostProvided = true;

export const stderr = {
  writeViaStream(streamReader) {
    return drain("stderr", streamReader);
  },
};
stderr.writeViaStream._isHostProvided = true;
