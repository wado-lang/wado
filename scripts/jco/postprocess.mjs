// Supply the P3 runtime released jco (1.24.3) fails to emit, mirroring the fork
// patches (vendor/jco.patch). Each transform throws if it is needed but its
// anchor is missing, rather than silently producing a no-output component.

const INTERNAL_FUTURE = /class InternalFuture\s*\{/;
const STREAM_WRITE_FN = /async function streamWrite\s*\(/;
const STREAM_COPY = "const result = await streamEnd.copy({";

const STREAM_HOOK = `if (typeof globalThis._jcoStreamWriteHook === 'function' && streamEnd.isWritable()) {
        const count_ = count >>> 0;
        const data_ = new Uint8Array(getMemoryFn().buffer, ptr, count_).slice();
        if (globalThis._jcoStreamWriteHook(streamEndWaitableIdx, data_)) { return (count_ << 4) | 0; }
      }
      `;

/**
 * @param {string} js   the transpiled component JS (the main `<name>.js` file)
 * @param {string} classes  harvested future-end class definitions
 * @returns {{ js: string, applied: string[] }}
 */
export function postprocess(js, classes) {
  const applied = [];

  // (1) Inject the future-end classes when `InternalFuture` constructs them but
  // released jco did not emit their definitions.
  if (INTERNAL_FUTURE.test(js)) {
    if (/class FutureReadableEnd\b/.test(js)) {
      applied.push("future-end-classes-already-present");
    } else {
      const injected = js.replace(INTERNAL_FUTURE, (m) => `${classes}\n\n    ${m}`);
      if (injected === js) {
        throw new Error("postprocess: InternalFuture is present but the class-injection anchor did not match");
      }
      js = injected;
      applied.push("inject-future-end-classes");
    }
  }

  // (2) Insert the stream-write hook inside `streamWrite` — at the first
  // `streamEnd.copy` AFTER the streamWrite header, never the file's first copy
  // (which belongs to streamRead).
  const swMatch = STREAM_WRITE_FN.exec(js);
  if (swMatch) {
    const copyIdx = js.indexOf(STREAM_COPY, swMatch.index);
    if (copyIdx === -1) {
      throw new Error("postprocess: streamWrite is present but its streamEnd.copy anchor was not found");
    }
    js = js.slice(0, copyIdx) + STREAM_HOOK + js.slice(copyIdx);
    applied.push("stream-write-hook");
  }

  return { js, applied };
}
