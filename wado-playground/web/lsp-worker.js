// Web Worker hosting the wado-lsp engine, bundled into wado-playground.wasm
// (wasm32-unknown-unknown). The engine is driven as a plain library — one
// `wado_lsp_send(session, msg)` call per JSON-RPC message returns the framed
// replies — so there is no WASI shim, no JSPI, and no stdio loop. (The VS Code
// extension still runs the separate wasip1 stdio build; this is the browser path.)
//
// Wire protocol with the page: inbound `{type:"send", msg}` (a JSON-RPC object),
// outbound `{type:"ready"}`, `{type:"message", msg}` (one per framed reply),
// `{type:"error", text}`.

const encoder = new TextEncoder();

let exports = null;
let session = 0;

// --- stdout: Content-Length frame parser → postMessage (shared shape with the
// bytes wado_lsp_send returns, so the parser is transport-agnostic) ---

let stdoutBuf = new Uint8Array(0);

function appendStdout(data) {
  const merged = new Uint8Array(stdoutBuf.length + data.length);
  merged.set(stdoutBuf, 0);
  merged.set(data, stdoutBuf.length);
  stdoutBuf = merged;

  for (;;) {
    const headerEnd = findHeaderEnd(stdoutBuf);
    if (headerEnd < 0) return;
    const header = new TextDecoder().decode(stdoutBuf.subarray(0, headerEnd));
    const m = /Content-Length:\s*(\d+)/i.exec(header);
    if (!m) {
      postMessage({ type: "error", text: `reply frame without Content-Length: ${header}` });
      stdoutBuf = stdoutBuf.subarray(headerEnd + 4);
      continue;
    }
    const len = Number(m[1]);
    const bodyStart = headerEnd + 4;
    if (stdoutBuf.length < bodyStart + len) return;
    const body = new TextDecoder().decode(stdoutBuf.subarray(bodyStart, bodyStart + len));
    stdoutBuf = stdoutBuf.slice(bodyStart + len);
    try {
      postMessage({ type: "message", msg: JSON.parse(body) });
    } catch (err) {
      postMessage({ type: "error", text: `bad JSON from server: ${err}` });
    }
  }
}

function findHeaderEnd(buf) {
  for (let i = 0; i + 3 < buf.length; i++) {
    if (buf[i] === 13 && buf[i + 1] === 10 && buf[i + 2] === 13 && buf[i + 3] === 10) return i;
  }
  return -1;
}

// --- one JSON-RPC message → framed replies, all in-memory ---

function send(msg) {
  const { memory, wado_alloc, wado_free, wado_lsp_send } = exports;
  const body = encoder.encode(JSON.stringify(msg));
  const inPtr = wado_alloc(body.length);
  new Uint8Array(memory.buffer, inPtr, body.length).set(body);

  // wado_lsp_send frees inPtr and returns [len:u32 LE][framed reply bytes].
  const outPtr = wado_lsp_send(session, inPtr, body.length);
  const len = new DataView(memory.buffer, outPtr, 4).getUint32(0, true);
  const framed = new Uint8Array(memory.buffer, outPtr + 4, len).slice();
  wado_free(outPtr, 4 + len);
  if (framed.length > 0) appendStdout(framed);
}

self.onmessage = (e) => {
  if (e.data.type !== "send") return;
  try {
    send(e.data.msg);
  } catch (err) {
    postMessage({ type: "error", text: String(err?.stack ?? err) });
  }
};

async function main() {
  const wasmUrl = new URL("./wado-playground.wasm", import.meta.url);
  const resp = await fetch(wasmUrl);
  if (!resp.ok) throw new Error(`failed to load ${wasmUrl}: HTTP ${resp.status}`);
  // `wado_phase` is the compiler's progress import; the LSP path never calls it.
  const { instance } = await WebAssembly.instantiateStreaming(resp, {
    env: { wado_phase: () => {} },
  });
  exports = instance.exports;
  session = exports.wado_lsp_new();
  postMessage({ type: "ready" });
}

main().catch((err) => postMessage({ type: "error", text: String(err?.stack ?? err) }));
