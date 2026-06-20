// Transpile a Wado component to JS with the *released* jco (no Rust fork), then
// supply the missing P3 runtime via postprocess.mjs.
//
// Usage: node transpile-released.mjs <component.wasm> [output-dir]
//
// Run the result on Node 26+ (stable JSPI, no flag needed):
//   node -e "import('<dir>/<name>.js').then(m => m.run.run())"

import { transpile } from "@bytecodealliance/jco";
import { readFile, writeFile, mkdir } from "node:fs/promises";
import { basename, dirname, join } from "node:path";
import { postprocess } from "./postprocess.mjs";

const here = dirname(new URL(import.meta.url).pathname);
const shimDir = join(here, "..", "..", "example", "jco-shim");

const wasmPath = process.argv[2];
if (!wasmPath) {
  console.error("Usage: node transpile-released.mjs <component.wasm> [output-dir]");
  process.exit(1);
}
const name = basename(wasmPath).replace(/\.wasm$/, "");
const outDir = process.argv[3] ?? wasmPath.replace(/\.wasm$/, "-jco-released");

const classes = await readFile(join(here, "missing-intrinsics.js"), "utf8");

// --no-wasi-shim + --map so the example/jco-shim adapters win over jco's
// built-in WASI shim (which does not deliver stdout for Wado components).
const { files } = await transpile(await readFile(wasmPath), {
  name,
  wasiShim: false,
  map: [
    `wasi:cli/*=${shimDir}/cli.js#*`,
    `wasi:random/*=${shimDir}/random.js#*`,
    `wasi:clocks/*=${shimDir}/clocks.js#*`,
  ],
});

const enc = new TextEncoder();
for (const [file, bytes] of Object.entries(files)) {
  let data = bytes;
  if (file === `${name}.js`) {
    const { js, applied } = postprocess(new TextDecoder().decode(bytes), classes);
    console.error(`  post-process: ${applied.join(", ") || "(no transforms applied)"}`);
    data = enc.encode(js);
  }
  const p = join(outDir, file);
  await mkdir(dirname(p), { recursive: true });
  await writeFile(p, data);
}
await writeFile(join(outDir, "package.json"), '{"type":"module"}\n');
console.error(`transpiled (released jco) → ${outDir}`);
