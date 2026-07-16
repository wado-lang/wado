// Worker running one Wado program: compile → transpile → run, streaming
// stdout/stderr back as it is written. One run per worker instance; the page
// terminates the worker to stop a runaway program.
//
// Inbound: `{type:"run", source}`. Outbound: `{type:"status", phase}`,
// `{type:"stdout"|"stderr", text}`, `{type:"done", ms}`, `{type:"error", text}`.

import { compile, transpileToModule } from "./playground.js";

self.onmessage = async (e) => {
  const { type, source } = e.data;
  if (type !== "run") return;
  const t0 = performance.now();
  globalThis._wadoWrite = (kind, text) => postMessage({ type: kind, text });
  try {
    postMessage({ type: "status", phase: "compiling" });
    const component = await compile(source);
    postMessage({ type: "status", phase: "transpiling" });
    const moduleUrl = await transpileToModule(component);
    postMessage({ type: "status", phase: "running" });
    try {
      const mod = await import(moduleUrl);
      if (mod.run?.run) {
        await mod.run.run();
      } else if (typeof mod.run === "function") {
        await mod.run();
      } else {
        throw new Error(`no runnable export found (keys: ${Object.keys(mod).join(", ")})`);
      }
    } finally {
      URL.revokeObjectURL(moduleUrl);
    }
    postMessage({ type: "done", ms: Math.round(performance.now() - t0) });
  } catch (err) {
    postMessage({ type: "error", text: String(err?.message ?? err) });
  }
};
