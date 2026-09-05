// Census of the compile-time regions that reach the final IR, over the benchmark
// and wasm-size corpora and the packages written in Wado, grouped by cause.
//
// Usage: node scripts/const-region-census.mjs [--bin <wado>]

import { execFile } from "node:child_process";
import { access, readdir } from "node:fs/promises";
import { join } from "node:path";
import { promisify } from "node:util";

const run = promisify(execFile);

const argv = process.argv.slice(2);
const binIndex = argv.indexOf("--bin");
if (binIndex >= 0 && argv[binIndex + 1] === undefined) {
  throw new Error("--bin needs the path to a wado binary");
}
const WADO = binIndex >= 0 ? argv[binIndex + 1] : "target/debug/wado";
// Checked once, so a missing binary is one error rather than a run in which
// every file fails to compile and the census prints a confident zero.
await access(WADO);

const CORPUS_DIRS = ["benchmark", "wasm-size"];

/** Every `.wado` entry point in the corpus directories, plus the packages. */
async function corpusFiles() {
  const files = [];
  for (const root of CORPUS_DIRS) {
    for (const entry of await readdir(root, { withFileTypes: true })) {
      if (!entry.isDirectory()) continue;
      const dir = join(root, entry.name);
      for (const file of await readdir(dir)) {
        if (file.endsWith(".wado")) files.push(join(dir, file));
      }
    }
  }
  for (const entry of await readdir(".", { withFileTypes: true })) {
    if (!entry.isDirectory() || !entry.name.startsWith("package-")) continue;
    const src = join(entry.name, "src");
    for (const file of await readdir(src).catch(() => [])) {
      if (file.endsWith(".wado")) files.push(join(src, file));
    }
  }
  return files.sort();
}

/** The `remark:` lines a compile emits, or the reason it did not build. */
async function remarksFor(file) {
  try {
    const { stderr } = await run(
      WADO,
      ["compile", "-O2", "--log-level", "info", "-o", "/dev/null", file],
      { maxBuffer: 64 << 20 },
    );
    return { remarks: stderr.split("\n").filter((l) => l.includes("remark:")) };
  } catch (e) {
    // Carry the diagnostic: a file that is not an entry point and one the
    // compiler rejects both land here, and only the first is expected.
    const first = (e.stderr ?? "").split("\n").find((l) => l.includes("error:"));
    return { error: first?.trim() ?? e.message };
  }
}

/** What a const-region remark blames: the surviving calls, or the refusal. */
function causeOf(remark) {
  const m = remark.match(/computes a constant at run time: (.*)$/);
  if (!m) return [];
  const cause = m[1].trim();
  return cause.endsWith("still runs here")
    ? [...cause.matchAll(/`([^`]+)`/g)].map((c) => c[1])
    : [cause];
}

const files = await corpusFiles();
const byType = new Map();
const byFile = new Map();
const skipped = [];

for (const file of files) {
  const { remarks, error } = await remarksFor(file);
  if (error !== undefined) {
    skipped.push([file, error]);
    continue;
  }
  const regions = remarks.filter((r) => r.includes("computes a constant at run time"));
  if (regions.length > 0) byFile.set(file, regions.length);
  for (const remark of regions) {
    for (const type of causeOf(remark)) {
      byType.set(type, (byType.get(type) ?? 0) + 1);
    }
  }
}

const total = [...byFile.values()].reduce((a, b) => a + b, 0);
console.log(`# Compile-time regions surviving to the final IR\n`);
console.log(`${total} across ${byFile.size} of ${files.length - skipped.length} files.\n`);

console.log(`## By cause\n`);
console.log(`| Cause | Regions |`);
console.log(`| ----- | ------- |`);
for (const [type, n] of [...byType].sort((a, b) => b[1] - a[1])) {
  console.log(`| \`${type}\` | ${n} |`);
}

console.log(`\n## By file\n`);
console.log(`| File | Regions |`);
console.log(`| ---- | ------- |`);
for (const [file, n] of [...byFile].sort((a, b) => b[1] - a[1])) {
  console.log(`| \`${file}\` | ${n} |`);
}

if (skipped.length > 0) {
  console.log(`\n## Not compiled\n`);
  for (const [file, error] of skipped) console.log(`- \`${file}\` — ${error}`);
}
