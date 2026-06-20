// Post-process a jco-transpiled component module to supply the P3 runtime that
// released jco (1.24.3) fails to emit for Wado components. Two transforms,
// mirroring the Wado fork's runtime-affecting patches (see vendor/jco.patch):
//
//   1. Inject the future-end intrinsic classes (patch: missing intrinsic deps).
//      Released jco references `FutureReadableEnd` / `FutureWritableEnd` from
//      `InternalFuture` but does not emit their definitions on the future-drop
//      path. We splice the harvested classes in at module scope, right before
//      `InternalFuture`, so they share scope with `NESTED_FUTURE_SYMBOL`,
//      `getOrCreateAsyncState`, `FUTURES`, etc. (globalThis injection cannot,
//      because the classes reference those module-scoped bindings).
//
//   2. Insert the stream-write hook (patch: stream write hook). Lets a WASI shim
//      receive bulk stream data via `globalThis._jcoStreamWriteHook`, bypassing
//      jco's byte-at-a-time rendezvous. Required for the example/jco-shim cli.js
//      stdout path. The hook must go inside `streamWrite`, NOT at the first
//      `streamEnd.copy` in the file — `streamRead` is emitted first and has its
//      own `streamEnd.copy`, so anchoring on the first one would hook the read
//      path (a no-op, guarded by `isWritable()`) and leave stdout unhooked.
//
// Each transform throws if it is needed (its precondition is present) but its
// anchor is missing — a silent no-op would yield a clean-looking component that
// produces no output or crashes at runtime with no pointer back to here.
//
// Not handled: the async-non-void-export return fix (patch #3 in the fork). It
// rewrites emitted code jco shapes differently per export, so it is left to the
// fork. Void exports (CLI `run`, `test` blocks) do not need it; result-returning
// async exports (HTTP `handle`) do — but those do not transpile through released
// jco yet anyway (a separate WIT-representation error).

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
