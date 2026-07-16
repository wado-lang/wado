// Web Worker hosting wado-lsp.wasm (wasm32-wasip1, stdio JSON-RPC) behind a
// minimal WASI preview1 shim.
//
// stdin's `fd_read` is a `WebAssembly.Suspending` import: JSPI suspends the
// server's blocking read loop until the page posts the next message, so the
// unmodified stdio binary becomes event-driven — no SharedArrayBuffer (GitHub
// Pages cannot set COOP/COEP) and no Asyncify build.
//
// Wire protocol with the page: inbound `{type:"send", msg}` (a JSON-RPC
// object, framed here), outbound `{type:"message", msg}` (parsed from framed
// stdout), `{type:"stderr", text}`, `{type:"exit", code}`, `{type:"error", text}`.

const ERRNO = { SUCCESS: 0, BADF: 8, INVAL: 28, NOENT: 44, NOSYS: 52, SPIPE: 70 };

const encoder = new TextEncoder();
const stderrDecoder = new TextDecoder();

let memory = null;
const view = () => new DataView(memory.buffer);
const bytes = () => new Uint8Array(memory.buffer);

// --- stdin: queued frames + a waiter resolved on arrival ---

const stdinChunks = [];
let stdinWaiter = null;

function pushStdin(chunk) {
  stdinChunks.push(chunk);
  if (stdinWaiter) {
    const wake = stdinWaiter;
    stdinWaiter = null;
    wake();
  }
}

self.onmessage = (e) => {
  const { type, msg } = e.data;
  if (type !== "send") return;
  const body = encoder.encode(JSON.stringify(msg));
  const frame = new Uint8Array(body.length + 64);
  const header = encoder.encode(`Content-Length: ${body.length}\r\n\r\n`);
  frame.set(header, 0);
  frame.set(body, header.length);
  pushStdin(frame.subarray(0, header.length + body.length));
};

async function readStdin(iovsPtr, iovsLen, nreadPtr) {
  while (stdinChunks.length === 0) {
    await new Promise((resolve) => (stdinWaiter = resolve));
  }
  const dv = view();
  let nread = 0;
  for (let i = 0; i < iovsLen && stdinChunks.length > 0; i++) {
    const base = dv.getUint32(iovsPtr + i * 8, true);
    const cap = dv.getUint32(iovsPtr + i * 8 + 4, true);
    let filled = 0;
    while (filled < cap && stdinChunks.length > 0) {
      const chunk = stdinChunks[0];
      const take = Math.min(cap - filled, chunk.length);
      bytes().set(chunk.subarray(0, take), base + filled);
      filled += take;
      if (take === chunk.length) stdinChunks.shift();
      else stdinChunks[0] = chunk.subarray(take);
    }
    nread += filled;
  }
  view().setUint32(nreadPtr, nread, true);
  return ERRNO.SUCCESS;
}

// --- stdout: Content-Length frame parser → postMessage ---

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
      postMessage({ type: "error", text: `stdout frame without Content-Length: ${header}` });
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

// --- WASI preview1 imports ---

function gather(iovsPtr, iovsLen) {
  const dv = view();
  const parts = [];
  let total = 0;
  for (let i = 0; i < iovsLen; i++) {
    const base = dv.getUint32(iovsPtr + i * 8, true);
    const len = dv.getUint32(iovsPtr + i * 8 + 4, true);
    parts.push(bytes().slice(base, base + len));
    total += len;
  }
  return { parts, total };
}

function fdWrite(fd, iovsPtr, iovsLen, nwrittenPtr) {
  const { parts, total } = gather(iovsPtr, iovsLen);
  for (const part of parts) {
    if (fd === 1) appendStdout(part);
    else if (fd === 2) {
      const text = stderrDecoder.decode(part, { stream: true });
      if (text) postMessage({ type: "stderr", text });
    } else return ERRNO.BADF;
  }
  view().setUint32(nwrittenPtr, total, true);
  return ERRNO.SUCCESS;
}

const nowNanos = () => BigInt(Math.round((performance.timeOrigin + performance.now()) * 1e6));

class ProcExit {
  constructor(code) {
    this.code = code;
  }
}

const wasiImports = {
  fd_read: new WebAssembly.Suspending(async (fd, iovsPtr, iovsLen, nreadPtr) =>
    fd === 0 ? readStdin(iovsPtr, iovsLen, nreadPtr) : ERRNO.BADF,
  ),
  fd_write: fdWrite,
  fd_close: () => ERRNO.SUCCESS,
  fd_fdstat_get: (fd, ptr) => {
    if (fd > 2) return ERRNO.BADF;
    const dv = view();
    dv.setUint8(ptr, 2); // filetype: character_device
    dv.setUint16(ptr + 2, 0, true);
    dv.setBigUint64(ptr + 8, 0xffff_ffff_ffff_ffffn, true);
    dv.setBigUint64(ptr + 16, 0xffff_ffff_ffff_ffffn, true);
    return ERRNO.SUCCESS;
  },
  fd_fdstat_set_flags: () => ERRNO.SUCCESS,
  fd_filestat_get: () => ERRNO.BADF,
  fd_seek: () => ERRNO.SPIPE,
  fd_prestat_get: () => ERRNO.BADF, // no preopens: the stdlib is embedded in the compiler
  fd_prestat_dir_name: () => ERRNO.BADF,
  path_open: () => ERRNO.NOENT,
  path_filestat_get: () => ERRNO.NOENT,
  path_readlink: () => ERRNO.NOENT,
  path_remove_directory: () => ERRNO.NOENT,
  path_unlink_file: () => ERRNO.NOENT,
  path_create_directory: () => ERRNO.NOSYS,
  fd_readdir: () => ERRNO.BADF,
  args_sizes_get: (argcPtr, bufSizePtr) => {
    view().setUint32(argcPtr, 0, true);
    view().setUint32(bufSizePtr, 0, true);
    return ERRNO.SUCCESS;
  },
  args_get: () => ERRNO.SUCCESS,
  environ_sizes_get: (countPtr, bufSizePtr) => {
    view().setUint32(countPtr, 0, true);
    view().setUint32(bufSizePtr, 0, true);
    return ERRNO.SUCCESS;
  },
  environ_get: () => ERRNO.SUCCESS,
  clock_time_get: (_id, _precision, timePtr) => {
    view().setBigUint64(timePtr, nowNanos(), true);
    return ERRNO.SUCCESS;
  },
  clock_res_get: (_id, resPtr) => {
    view().setBigUint64(resPtr, 1000000n, true);
    return ERRNO.SUCCESS;
  },
  random_get: (ptr, len) => {
    const out = bytes().subarray(ptr, ptr + len);
    for (let i = 0; i < out.length; i += 65536) {
      crypto.getRandomValues(out.subarray(i, Math.min(i + 65536, out.length)));
    }
    return ERRNO.SUCCESS;
  },
  sched_yield: () => ERRNO.SUCCESS,
  poll_oneoff: () => ERRNO.NOSYS,
  proc_exit: (code) => {
    throw new ProcExit(code);
  },
};

const warned = new Set();
function stub(name) {
  return (...args) => {
    if (!warned.has(name)) {
      warned.add(name);
      console.warn(`wado-lsp worker: unimplemented WASI call ${name}(${args.join(", ")})`);
    }
    return ERRNO.NOSYS;
  };
}

async function main() {
  const wasmUrl = new URL("./wado-lsp.wasm", import.meta.url);
  const resp = await fetch(wasmUrl);
  if (!resp.ok) throw new Error(`failed to load ${wasmUrl}: HTTP ${resp.status}`);
  const module = await WebAssembly.compileStreaming(resp);

  const imports = { wasi_snapshot_preview1: {} };
  for (const { module: m, name } of WebAssembly.Module.imports(module)) {
    if (m !== "wasi_snapshot_preview1") throw new Error(`unexpected import module: ${m}`);
    imports[m][name] = wasiImports[name] ?? stub(name);
  }

  const instance = await WebAssembly.instantiate(module, imports);
  memory = instance.exports.memory;
  postMessage({ type: "ready" });

  try {
    await WebAssembly.promising(instance.exports._start)();
    postMessage({ type: "exit", code: 0 });
  } catch (err) {
    if (err instanceof ProcExit) postMessage({ type: "exit", code: err.code });
    else throw err;
  }
}

main().catch((err) => postMessage({ type: "error", text: String(err?.stack ?? err) }));
