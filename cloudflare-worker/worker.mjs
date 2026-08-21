// Serve a Wado `wasi:http/service` component from a Cloudflare Worker.
//
// `build.sh` compiles the program and transpiles it into `gen/`; this module
// bridges the Worker's `fetch` to the component's `handle`.

import core0 from "./gen/service.core.wasm";
import core1 from "./gen/service.core2.wasm";
import { instantiate } from "./gen/service.js";
import { types } from "./shims/http.js";
import * as cli from "./shims/cli.js";
import * as clocks from "./shims/clocks.js";

const { Fields, Request, Response: WasiResponse } = types;
const CORES = { "service.core.wasm": core0, "service.core2.wasm": core1 };

// One instance per request: workerd gives each request its own I/O context, and
// jco holds its async task state on the module, so a shared instance stops
// settling after the first.
function guest() {
  return instantiate((name) => CORES[name], {
    "./shims/cli.js": cli,
    "./shims/clocks.js": clocks,
    "./shims/http.js": { types },
  }).then((x) => x.handler ?? x["wasi:http/handler@0.3.0"]);
}

const METHODS = ["get", "head", "post", "put", "delete", "connect", "options", "trace", "patch"];
const asMethod = (m) =>
  METHODS.includes(m.toLowerCase()) ? { tag: m.toLowerCase() } : { tag: "other", val: m };

// The host owns these three, and the shim rejects them as guest-set headers.
const HOST_HEADERS = new Set(["host", "connection", "content-length"]);

function toWasiRequest(request, url) {
  const entries = [];
  for (const [name, value] of request.headers) {
    const lower = name.toLowerCase();
    if (!HOST_HEADERS.has(lower)) entries.push([lower, new TextEncoder().encode(value)]);
  }
  return [Fields.fromList(entries), url];
}

async function bodyOf(request) {
  if (!request.body) return null;
  const bytes = new Uint8Array(await request.arrayBuffer());
  if (!bytes.length) return null;
  return (async function* () {
    yield bytes;
  })();
}

async function collect(stream) {
  const chunks = [];
  let total = 0;
  for await (const chunk of stream) {
    chunks.push(chunk);
    total += chunk.length;
  }
  const out = new Uint8Array(total);
  let offset = 0;
  for (const chunk of chunks) {
    out.set(chunk, offset);
    offset += chunk.length;
  }
  return out;
}

export default {
  async fetch(request) {
    const handler = await guest();
    const url = new URL(request.url);
    const [headers] = toWasiRequest(request, url);

    const [wasiRequest] = Request.new(
      headers,
      await bodyOf(request),
      Promise.resolve(undefined),
      undefined,
    );
    wasiRequest.setMethod(asMethod(request.method));
    wasiRequest.setPathWithQuery(url.pathname + url.search);
    wasiRequest.setScheme({ tag: url.protocol === "https:" ? "HTTPS" : "HTTP" });
    wasiRequest.setAuthority(url.host);

    const response = await handler.handle(wasiRequest);

    const out = new Headers();
    for (const [name, value] of response.getHeaders().copyAll()) {
      out.append(name, new TextDecoder().decode(value));
    }
    const [body] = WasiResponse.consumeBody(response, Promise.resolve(undefined));
    return new Response(await collect(body), { status: response.getStatusCode(), headers: out });
  },
};
