// Fully client-side Wado playground: compile Wado source to a component with
// the wasm-sandboxed Wado compiler, transpile that component to JS with jco
// (also wasm, in-browser), then import and run the result — all in the browser.

import { transpileBytes } from "./vendor/jco-transpile.browser.js";
// Same P3-runtime re-injection the Node pipeline uses; staged by build.sh so
// the browser and Node paths share one source of truth (no drift on jco bumps).
import { postprocess } from "./vendor/postprocess.js";

const BASE = new URL(".", import.meta.url);

let _compilerInstance = null;

async function loadCompiler() {
  if (_compilerInstance) return _compilerInstance;
  const wasmUrl = new URL("./wado-playground.wasm", BASE);
  const resp = await fetch(wasmUrl);
  if (!resp.ok) throw new Error(`failed to load ${wasmUrl}: HTTP ${resp.status}`);
  const { instance } = await WebAssembly.instantiateStreaming(resp, {});
  _compilerInstance = instance;
  return instance;
}

/** Compile Wado source → Component Model Wasm bytes (or throw with diagnostics). */
export async function compile(source) {
  const { memory, wado_alloc, wado_compile, wado_free } = (await loadCompiler()).exports;
  const src = new TextEncoder().encode(source);
  const inPtr = wado_alloc(src.length);
  new Uint8Array(memory.buffer, inPtr, src.length).set(src);

  // wado_compile consumes (frees) the input buffer and returns a result buffer
  // we own: [status:u32][len:u32][payload…]. Copy the payload out, then free it.
  const outPtr = wado_compile(inPtr, src.length);
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

/** Compile + transpile + run, returning captured stdout/stderr text. */
export async function run(source) {
  if (_running) throw new Error("a program is already running");
  _running = true;
  const out = { stdout: "", stderr: "" };
  const prevWrite = globalThis._wadoWrite;
  globalThis._wadoWrite = (kind, text) => { out[kind] += text; };
  try {
    const component = await compile(source);
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
