// Transpile a Wado component to JS with the released jco.
//
// Usage: node transpile-released.mjs <component.wasm> [output-dir]
//
// Run the result on Node 26+ (stable JSPI, no flag needed):
//   node -e "import('<dir>/<name>.js').then(m => m.run.run())"

import { transpile } from "@bytecodealliance/jco";
import { appendFile, copyFile, readdir, readFile, writeFile, mkdir, symlink, rm } from "node:fs/promises";
import { basename, dirname, join } from "node:path";

const wasmPath = process.argv[2];
if (!wasmPath) {
  console.error("Usage: node transpile-released.mjs <component.wasm> [output-dir]");
  process.exit(1);
}
const here = dirname(new URL(import.meta.url).pathname);
const name = basename(wasmPath).replace(/\.wasm$/, "");
const outDir = process.argv[3] ?? wasmPath.replace(/\.wasm$/, "-jco-released");

// jco's own WASI shim (`preview3-shim`) serves the `wasi:*` imports, and
// `package-web`'s glue each `wado-lang:web/*` import, copied beside an output
// that imports it. A glue module exports one object per interface it serves.
const WEB_PACKAGE = "wado-lang:web";
const glueDir = join(here, "../../package-web/glue");
const glue = (await readdir(glueDir)).filter((file) => file.endsWith(".js"));
const kebab = (camel) => camel.replace(/[A-Z]/g, (c) => `-${c.toLowerCase()}`);
const map = {};
for (const file of glue) {
  const source = await readFile(join(glueDir, file), "utf8");
  for (const [, iface] of source.matchAll(/^export const (\w+) = \{/gm)) {
    map[`${WEB_PACKAGE}/${kebab(iface)}`] = `./web-${file}#${iface}`;
  }
}
const { files } = await transpile(await readFile(wasmPath), { name, map });

for (const [file, bytes] of Object.entries(files)) {
  const p = join(outDir, file);
  await mkdir(dirname(p), { recursive: true });
  await writeFile(p, bytes);
}
const entry = new TextDecoder().decode(files[`${name}.js`]);
const used = glue.filter((file) => entry.includes(`./web-${file}`));
for (const file of used) {
  await copyFile(join(glueDir, file), join(outDir, `web-${file}`));
}
// The output hands the glue its callback export once instantiated: importing it
// from the glue would close a cycle the output's top-level reads do not survive.
const callback = entry.match(/export \{[^}]*?\b(\w+) as 'wado:callback\/callback'/);
if (callback) {
  const connects = used.map(
    (file, i) => `import { $connect as $connect${i} } from "./web-${file}";\n$connect${i}(${callback[1]});\n`,
  );
  await appendFile(join(outDir, `${name}.js`), connects.join(""));
}
await writeFile(join(outDir, "package.json"), '{"type":"module"}\n');
// The output imports the shim by bare specifier, and `outDir` is usually a temp
// directory with no `node_modules` above it.
const deps = join(here, "node_modules");
await rm(join(outDir, "node_modules"), { force: true, recursive: true });
await symlink(deps, join(outDir, "node_modules"), "dir");
console.error(`transpiled (released jco) → ${outDir}`);
