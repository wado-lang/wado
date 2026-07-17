// Bundle jco's `transpileBytes` for the browser.
//
// `@bytecodealliance/jco-transpile` targets Node by default: its dependency
// graph statically imports a handful of `node:*` builtins. The actual
// bytes-in/files-out transpile path never executes the filesystem ones, so we
// stub every `node:*` specifier — `node:path` with real (pure) string impls,
// the rest with minimal shapes that only need to satisfy the named imports.
//
// The bindgen glue resolves its two `.core.wasm` files via
// `new URL('./…core.wasm', import.meta.url)`, so the output must sit next to
// them in web/vendor/.

import { fileURLToPath, pathToFileURL } from "node:url";
import { dirname, join } from "node:path";

const here = dirname(fileURLToPath(import.meta.url));
// jco-transpile + esbuild live under scripts/jco/node_modules.
const jcoDir = join(here, "..", "..", "scripts", "jco");
const { build } = await import(
  pathToFileURL(join(jcoDir, "node_modules", "esbuild", "lib", "main.js")).href
);
const outfile = join(here, "vendor", "jco-transpile.browser.js");

const NODE_STUBS = {
  "node:path": `
    export const sep = "/";
    export function basename(p, ext) { let b = String(p).split("/").pop() ?? ""; if (ext && b.endsWith(ext)) b = b.slice(0, -ext.length); return b; }
    export function extname(p) { const b = String(p).split("/").pop() ?? ""; const i = b.lastIndexOf("."); return i > 0 ? b.slice(i) : ""; }
    export function dirname(p) { const s = String(p).split("/"); s.pop(); return s.join("/") || "."; }
    export function join(...xs) { return xs.filter(Boolean).join("/").replace(/\\/+/g, "/"); }
    export function resolve(...xs) { return "/" + xs.filter(Boolean).join("/").replace(/\\/+/g, "/").replace(/^\\/+/, ""); }
    export function normalize(p) { return String(p).replace(/\\/+/g, "/"); }
  `,
  "node:url": `export function fileURLToPath(u) { return String(u).replace(/^file:\\/\\//, ""); }`,
  "node:buffer": `export const Buffer = globalThis.Buffer ?? { from: (x) => new Uint8Array(x) };`,
  "node:process": `export const platform = "browser"; export const argv0 = "browser";`,
  "node:util": `export const TextEncoder = globalThis.TextEncoder; export const TextDecoder = globalThis.TextDecoder; export function promisify(f) { return f; }`,
  "node:os": `export function tmpdir() { return "/tmp"; }`,
  "node:fs/promises": `
    const nope = async () => { throw new Error("node:fs/promises is not available in the browser transpile path"); };
    export const readFile = nope, writeFile = nope, rm = nope, mkdtemp = nope, mkdir = nope;
  `,
  "node:fs": `export function readFileSync() { throw new Error("node:fs is not available in the browser transpile path"); }`,
  "node:child_process": `export function spawn() { throw new Error("node:child_process is not available in the browser transpile path"); }`,
};

const nodeStubPlugin = {
  name: "node-stub",
  setup(b) {
    const filter = /^node:/;
    b.onResolve({ filter }, (args) => ({ path: args.path, namespace: "node-stub" }));
    b.onLoad({ filter: /.*/, namespace: "node-stub" }, (args) => {
      const contents = NODE_STUBS[args.path];
      if (!contents) throw new Error(`no stub for ${args.path}`);
      return { contents, loader: "js" };
    });
  },
};

await build({
  stdin: {
    contents: `export { transpileBytes } from "@bytecodealliance/jco-transpile";`,
    resolveDir: jcoDir,
    loader: "js",
  },
  bundle: true,
  format: "esm",
  platform: "browser",
  outfile,
  plugins: [nodeStubPlugin],
  logLevel: "info",
});

console.log("bundled →", outfile);
