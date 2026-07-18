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
const decoder = new TextDecoder();

let exports = null;
let session = 0;
// Messages that arrive before the wasm finishes instantiating are buffered here
// and flushed once the engine is ready, so a pre-`ready` send is never dropped.
const pending = [];

// --- parse a self-contained buffer of Content-Length frames → postMessage.
// wado_lsp_send always returns whole frames, so there is no cross-call state. ---

function emitFrames(buf) {
  let off = 0;
  while (off < buf.length) {
    const headerEnd = findHeaderEnd(buf, off);
    if (headerEnd < 0) {
      postMessage({ type: "error", text: `reply without Content-Length terminator` });
      return;
    }
    const header = decoder.decode(buf.subarray(off, headerEnd));
    const m = /Content-Length:\s*(\d+)/i.exec(header);
    if (!m) {
      postMessage({ type: "error", text: `reply frame without Content-Length: ${header}` });
      return;
    }
    const bodyStart = headerEnd + 4;
    const bodyEnd = bodyStart + Number(m[1]);
    const body = decoder.decode(buf.subarray(bodyStart, bodyEnd));
    off = bodyEnd;
    try {
      postMessage({ type: "message", msg: JSON.parse(body) });
    } catch (err) {
      postMessage({ type: "error", text: `bad JSON from server: ${err}` });
    }
  }
}

function findHeaderEnd(buf, start) {
  for (let i = start; i + 3 < buf.length; i++) {
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
  if (framed.length > 0) emitFrames(framed);
}

function handle(msg) {
  try {
    send(msg);
  } catch (err) {
    postMessage({ type: "error", text: String(err?.stack ?? err) });
  }
}

self.onmessage = (e) => {
  if (e.data.type !== "send") return;
  if (exports) handle(e.data.msg);
  else pending.push(e.data.msg);
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
  for (const msg of pending.splice(0)) handle(msg);
}

main().catch((err) => postMessage({ type: "error", text: String(err?.stack ?? err) }));
