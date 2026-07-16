// Fully client-side Wado playground: compile Wado source to a component with
// the wasm-sandboxed Wado compiler, transpile that component to JS with jco
// (also wasm, in-browser), then import and run the result — all in the browser.

import { transpileBytes } from "./vendor/jco-transpile.browser.js";

const BASE = new URL(".", import.meta.url);

let _compilerInstance = null;

async function loadCompiler() {
  if (_compilerInstance) return _compilerInstance;
  const wasmUrl = new URL("./wado-playground.wasm", BASE);
  const { instance } = await WebAssembly.instantiateStreaming(fetch(wasmUrl), {});
  _compilerInstance = instance;
  return instance;
}

/** Compile Wado source → Component Model Wasm bytes (or throw with diagnostics). */
export async function compile(source) {
  const { memory, wado_alloc, wado_compile } = (await loadCompiler()).exports;
  const src = new TextEncoder().encode(source);
  const inPtr = wado_alloc(src.length);
  new Uint8Array(memory.buffer, inPtr, src.length).set(src);

  const outPtr = wado_compile(inPtr, src.length);
  const header = new DataView(memory.buffer, outPtr, 8);
  const status = header.getUint32(0, true);
  const len = header.getUint32(4, true);
  const payload = new Uint8Array(memory.buffer, outPtr + 8, len).slice();

  if (status === 1) return payload;
  throw new Error(new TextDecoder().decode(payload));
}

// The released jco omits the P3 future-end classes and the stdout write hook;
// re-inject them exactly as scripts/jco/postprocess.mjs does for Node.
const INTERNAL_FUTURE = /class InternalFuture\s*\{/;
const STREAM_WRITE_FN = /async function streamWrite\s*\(/;
const STREAM_COPY = "const result = await streamEnd.copy({";
const STREAM_HOOK = `if (typeof globalThis._jcoStreamWriteHook === 'function' && streamEnd.isWritable()) {
        const count_ = count >>> 0;
        const data_ = new Uint8Array(getMemoryFn().buffer, ptr, count_).slice();
        if (globalThis._jcoStreamWriteHook(streamEndWaitableIdx, data_)) { return (count_ << 4) | 0; }
      }
      `;

function postprocess(js, classes) {
  if (INTERNAL_FUTURE.test(js) && !/class FutureReadableEnd\b/.test(js)) {
    js = js.replace(INTERNAL_FUTURE, (m) => `${classes}\n\n    ${m}`);
  }
  const sw = STREAM_WRITE_FN.exec(js);
  if (sw) {
    const copyIdx = js.indexOf(STREAM_COPY, sw.index);
    if (copyIdx === -1) throw new Error("postprocess: streamWrite copy anchor not found");
    js = js.slice(0, copyIdx) + STREAM_HOOK + js.slice(copyIdx);
  }
  return js;
}

let _missingIntrinsics = null;
async function missingIntrinsics() {
  if (_missingIntrinsics == null) {
    _missingIntrinsics = await (await fetch(new URL("./vendor/missing-intrinsics.js", BASE))).text();
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
  let js = decoder.decode(jsFiles[0][1]);
  js = postprocess(js, await missingIntrinsics());

  const blob = new Blob([js], { type: "text/javascript" });
  return URL.createObjectURL(blob);
}

/** Compile + transpile + run, returning captured stdout/stderr text. */
export async function run(source) {
  const out = { stdout: "", stderr: "" };
  globalThis._wadoWrite = (kind, text) => { out[kind] += text; };

  const component = await compile(source);
  const moduleUrl = await transpileToModule(component);
  const mod = await import(moduleUrl);

  // wasi:cli/command exports the `run` interface with a `run()` function.
  if (mod.run?.run) {
    await mod.run.run();
  } else if (typeof mod.run === "function") {
    await mod.run();
  } else {
    throw new Error(`no runnable export found (keys: ${Object.keys(mod).join(", ")})`);
  }
  return out;
}
