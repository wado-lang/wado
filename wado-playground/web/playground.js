// Client-side Wado playground: compile → transpile (jco) → import and run, all
// in the browser. See README.md for the pipeline.

import { transpileBytes } from "./vendor/jco-transpile.browser.js";
// Shared with the Node pipeline (staged by build.sh) to avoid drift on jco bumps.
import { postprocess } from "./vendor/postprocess.js";

const BASE = new URL(".", import.meta.url);

let _compilerInstance = null;
// Set for the duration of one wado_compile call; the wasm `wado_phase` import
// forwards phase names here so a slow (mobile) compile shows live progress.
let _onPhase = null;

async function loadCompiler() {
  if (_compilerInstance) return _compilerInstance;
  const wasmUrl = new URL("./wado-playground.wasm", BASE);
  const resp = await fetch(wasmUrl);
  if (!resp.ok) throw new Error(`failed to load ${wasmUrl}: HTTP ${resp.status}`);
  // The compiler calls env.wado_phase(ptr, len) once per phase while compiling.
  const imports = {
    env: {
      wado_phase: (ptr, len) => {
        if (!_onPhase) return;
        const mem = _compilerInstance.exports.memory;
        const name = new TextDecoder().decode(new Uint8Array(mem.buffer, ptr, len));
        _onPhase(name);
      },
    },
  };
  const { instance } = await WebAssembly.instantiateStreaming(resp, imports);
  _compilerInstance = instance;
  return instance;
}

/**
 * Compile Wado source → Component Model Wasm bytes (or throw with diagnostics).
 * `onPhase(name)`, if given, is called synchronously for each compiler phase
 * (`parse`, `monomorphize`, `codegen`, …) as compilation progresses.
 */
export async function compile(source, onPhase) {
  const { memory, wado_alloc, wado_compile, wado_free } = (await loadCompiler()).exports;
  const src = new TextEncoder().encode(source);
  const inPtr = wado_alloc(src.length);
  new Uint8Array(memory.buffer, inPtr, src.length).set(src);

  // wado_compile frees inPtr and returns an owned result buffer; copy, then
  // free. Pass reportPhases=0 when no listener wants progress, so the compiler
  // skips the per-phase debug stream entirely.
  _onPhase = onPhase ?? null;
  let outPtr;
  try {
    outPtr = wado_compile(inPtr, src.length, onPhase ? 1 : 0);
  } finally {
    _onPhase = null;
  }
  const header = new DataView(memory.buffer, outPtr, 8);
  const status = header.getUint32(0, true);
  const len = header.getUint32(4, true);
  const payload = new Uint8Array(memory.buffer, outPtr + 8, len).slice();
  wado_free(outPtr, 8 + len);

  if (status === 1) return payload;
  throw new Error(new TextDecoder().decode(payload));
}

let _missingIntrinsics = null;
async function missingIntrinsics() {
  if (_missingIntrinsics == null) {
    const url = new URL("./vendor/missing-intrinsics.js", BASE);
    const res = await fetch(url);
    if (!res.ok) throw new Error(`failed to load ${url}: HTTP ${res.status}`);
    _missingIntrinsics = await res.text();
  }
  return _missingIntrinsics;
}

/** Transpile a component to a single importable ES module URL (blob). */
export async function transpileToModule(componentBytes, name = "program") {
  const shim = (m) => `${new URL(`./shims/${m}`, BASE).href}#*`;
  const { files } = await transpileBytes(componentBytes, {
    name,
    wasiShim: false,
    // Inline every core Wasm as base64 so the output is a single JS module.
    base64Cutoff: 1 << 30,
    map: {
      "wasi:cli/*": shim("cli.js"),
      "wasi:random/*": shim("random.js"),
      "wasi:clocks/*": shim("clocks.js"),
    },
  });

  const decoder = new TextDecoder();
  const jsFiles = Object.entries(files).filter(([f]) => f.endsWith(".js"));
  if (jsFiles.length !== 1) {
    throw new Error(`expected a single JS file, got: ${jsFiles.map(([f]) => f).join(", ")}`);
  }
  const { js } = postprocess(decoder.decode(jsFiles[0][1]), await missingIntrinsics());

  const blob = new Blob([js], { type: "text/javascript" });
  return URL.createObjectURL(blob);
}

let _running = false;

/**
 * Compile + transpile + run, returning captured stdout/stderr text.
 * `onPhase(name)`, if given, receives each compiler phase name as it runs.
 */
export async function run(source, onPhase) {
  if (_running) throw new Error("a program is already running");
  _running = true;
  const out = { stdout: "", stderr: "" };
  const prevWrite = globalThis._wadoWrite;
  globalThis._wadoWrite = (kind, text) => { out[kind] += text; };
  try {
    const component = await compile(source, onPhase);
    const moduleUrl = await transpileToModule(component);
    try {
      const mod = await import(moduleUrl);
      // wasi:cli/command exports the `run` interface with a `run()` function.
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
    return out;
  } finally {
    globalThis._wadoWrite = prevWrite;
    _running = false;
  }
}
