// Transpile a Wado component to JS with the released jco.
//
// Usage: node transpile-released.mjs <component.wasm> [output-dir]
//
// Run the result on Node 26+ (stable JSPI, no flag needed):
//   node -e "import('<dir>/<name>.js').then(m => m.run.run())"

import { transpile } from "@bytecodealliance/jco";
import { readFile, writeFile, mkdir, symlink, rm } from "node:fs/promises";
import { basename, dirname, join } from "node:path";

const wasmPath = process.argv[2];
if (!wasmPath) {
  console.error("Usage: node transpile-released.mjs <component.wasm> [output-dir]");
  process.exit(1);
}
const here = dirname(new URL(import.meta.url).pathname);
const name = basename(wasmPath).replace(/\.wasm$/, "");
const outDir = process.argv[3] ?? wasmPath.replace(/\.wasm$/, "-jco-released");

// jco's own WASI shim (`preview3-shim`) serves every import a Wado program
// makes, so the transpile is the plain one.
const { files } = await transpile(await readFile(wasmPath), { name });

for (const [file, bytes] of Object.entries(files)) {
  const p = join(outDir, file);
  await mkdir(dirname(p), { recursive: true });
  await writeFile(p, bytes);
}
await writeFile(join(outDir, "package.json"), '{"type":"module"}\n');
// The output imports the shim by bare specifier, and `outDir` is usually a temp
// directory with no `node_modules` above it.
const deps = join(here, "node_modules");
await rm(join(outDir, "node_modules"), { force: true, recursive: false }).catch(() => {});
await symlink(deps, join(outDir, "node_modules"), "dir");
console.error(`transpiled (released jco) → ${outDir}`);
