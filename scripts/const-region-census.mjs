// Census of the compile-time regions that reach the final IR, over the
// benchmark and wasm-size corpora and the packages written in Wado.
//
// Each row is one `remark:` the compiler emits for a block that computes a
// constant at run time (`remarks::collect_const_region_remarks`). The counts by
// type are what orders the format-coverage work: an `i32` row is integer
// formatting, an `f64` row is `fpfmt`. A corpus entry with no rows has nothing
// left for the engine to reach.
//
// Usage: node scripts/const-region-census.mjs [--bin <wado>]

import { execFile } from "node:child_process";
import { readdir } from "node:fs/promises";
import { join } from "node:path";
import { promisify } from "node:util";

const run = promisify(execFile);

const argv = process.argv.slice(2);
const binIndex = argv.indexOf("--bin");
const WADO = binIndex >= 0 ? argv[binIndex + 1] : "target/debug/wado";

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

/** The `remark:` lines a compile emits, or `null` when the file does not build. */
async function remarksFor(file) {
  try {
    const { stderr } = await run(
      WADO,
      ["compile", "-O2", "--log-level", "info", "-o", "/dev/null", file],
      { maxBuffer: 64 << 20 },
    );
    return stderr.split("\n").filter((l) => l.includes("remark:"));
  } catch {
    return null;
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
  const remarks = await remarksFor(file);
  if (remarks === null) {
    skipped.push(file);
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
  console.log(`\nNot compiled (not an entry point, or needs a package build):`);
  for (const file of skipped) console.log(`- \`${file}\``);
}
