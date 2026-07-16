// Collect example programs the playground can load: single-file programs
// from example/*.wado whose imports the browser pipeline can satisfy (no
// relative imports — the playground compiles one file) and that export a
// `run` entry point. Emits web/examples.json, consumed by the playground
// page's example picker and run as a test-browser.mjs case.

import { readdir, readFile, writeFile } from "node:fs/promises";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";

const WEB = dirname(fileURLToPath(import.meta.url));
const EXAMPLES_DIR = join(WEB, "..", "..", "example");

// core:* is embedded in the compiler; wasi:clocks and wasi:random have
// browser shims (shims/clocks.js, shims/random.js). Everything else —
// wasi:filesystem, wasi:http, wasi:cli's Stdin — has no browser backing.
const runnable = (spec) =>
  spec.startsWith("core:") || spec === "wasi:clocks" || spec === "wasi:random";

const examples = [];
for (const file of (await readdir(EXAMPLES_DIR)).filter((f) => f.endsWith(".wado")).sort()) {
  const source = await readFile(join(EXAMPLES_DIR, file), "utf8");
  const specs = [...source.matchAll(/from\s+"([^"]+)"/g)].map((m) => m[1]);
  if (!specs.every(runnable)) continue;
  if (!/export\s+fn\s+run\b/.test(source)) continue;
  examples.push({ name: file, source });
}

examples.sort((a, b) =>
  a.name === "hello.wado" ? -1 : b.name === "hello.wado" ? 1 : a.name.localeCompare(b.name),
);

const outfile = join(WEB, "examples.json");
await writeFile(outfile, `${JSON.stringify(examples, null, 1)}\n`);
console.log(`examples.json: ${examples.length} programs (${examples.map((e) => e.name).join(", ")})`);
