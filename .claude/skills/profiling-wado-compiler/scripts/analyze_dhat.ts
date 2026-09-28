#!/usr/bin/env node
/**
 * Break a valgrind DHAT profile of the native `wado` binary down by what was
 * live at the heap's peak (t-gmax): the bytes that set peak memory.
 *
 * Reports the total live at the peak, the bytes the filters keep, the
 * allocation sites (first frames past the collection and allocator plumbing),
 * and the inclusive bytes per `wado` function (deduped per stack, so recursion
 * does not count twice).
 *
 * Runs directly on Node.js >= 23.6 (type stripping is on by default):
 *   ./analyze_dhat.ts DHAT.json [--top N] [--where RE]... [--not RE]... [--stacks N]
 *
 * `--where` keeps a program point whose stack matches every RE, `--not` drops
 * one whose stack matches any. `--stacks N` prints the N largest stacks whole.
 */
import { readFileSync } from "node:fs";
import { parseArgs } from "node:util";

interface ProgramPoint {
  tb: number; // bytes allocated over the run
  gb: number; // bytes live at t-gmax
  gbk: number; // blocks live at t-gmax
  fs: number[]; // frame indices, innermost first
}

interface Dhat {
  pps: ProgramPoint[];
  ftbl: string[];
}

const { values, positionals } = parseArgs({
  allowPositionals: true,
  options: {
    top: { type: "string", default: "30" },
    where: { type: "string", multiple: true, default: [] },
    not: { type: "string", multiple: true, default: [] },
    stacks: { type: "string", default: "0" },
  },
});
if (positionals.length !== 1) {
  console.error("usage: analyze_dhat.ts DHAT.json [--top N] [--where RE]... [--not RE]... [--stacks N]");
  process.exit(2);
}

const dhat: Dhat = JSON.parse(readFileSync(positionals[0], "utf8"));
const top = Number(values.top);
const where = values.where.map((re) => new RegExp(re));
const not = values.not.map((re) => new RegExp(re));

// The crate a frame's own function lives in: `<Vec<wado_compiler::X> as
// Clone>::clone` is `alloc`'s, whatever its type arguments name.
const crateOf = (name: string): string => name.replace(/^</, "").split(/::|<| /)[0];

// An allocation is credited to the first frames of `wado` code: the rest (std,
// the collection crates, inlined helpers like `try_allocate_in`) only move
// bytes around. A stack with none (wasmtime, cranelift) is credited to its
// first frames past the allocator instead.
const isWado = (name: string): boolean => crateOf(name).startsWith("wado");
const isAllocator = (name: string): boolean =>
  /^(malloc|calloc|realloc|memalign|posix_memalign|__rust|__rdl|__rg)/.test(name) ||
  ["alloc", "core", "std"].includes(crateOf(name));

// `0xADDR: name (file:line)` → name, without the hash suffix. valgrind writes
// the address in upper case and the `dhat` crate in lower.
const frameName = (i: number): string => {
  const entry = dhat.ftbl[i];
  const match = entry.match(/^0x[0-9a-f]+: (.*?)(?: \((?:in )?[^()]*\))?$/i);
  if (!match) throw new Error(`unrecognized DHAT frame: ${entry}`);
  return match[1].replace(/::h[0-9a-f]{16}\b/g, "");
};
// A frame in a library without debug info has no line to show.
const frameLabel = (i: number): string => {
  const line = dhat.ftbl[i].match(/\(([^()]*:\d+)\)$/);
  return line ? `${frameName(i)} (${line[1]})` : frameName(i);
};
const mib = (bytes: number): string => (bytes / 1048576).toFixed(1).padStart(7);

const stackText = (pp: ProgramPoint): string => pp.fs.map((i) => dhat.ftbl[i]).join("\n");
const kept = dhat.pps.filter((pp) => {
  if (!pp.gb) return false;
  const stack = stackText(pp);
  return where.every((re) => re.test(stack)) && !not.some((re) => re.test(stack));
});

const total = dhat.pps.reduce((sum, pp) => sum + pp.gb, 0);
const keptBytes = kept.reduce((sum, pp) => sum + pp.gb, 0);
const keptBlocks = kept.reduce((sum, pp) => sum + pp.gbk, 0);
const keptAllocated = kept.reduce((sum, pp) => sum + pp.tb, 0);
console.log(`live at t-gmax: ${mib(total).trim()} MiB`);
console.log(
  `kept by filters: ${mib(keptBytes).trim()} MiB in ${keptBlocks} blocks ` +
    `(${mib(keptAllocated).trim()} MiB allocated over the run)`,
);

const sites = new Map<string, number>();
const inclusive = new Map<string, number>();
for (const pp of kept) {
  const callers = pp.fs.filter((i) => isWado(frameName(i)));
  const siteFrames = callers.length ? callers : pp.fs.filter((i) => !isAllocator(frameName(i)));
  const site = siteFrames
    .slice(0, 3)
    .map(frameLabel)
    .join("\n            <- ");
  sites.set(site, (sites.get(site) ?? 0) + pp.gb);
  const seen = new Set<string>();
  for (const i of callers) {
    const name = frameName(i);
    if (seen.has(name)) continue;
    seen.add(name);
    inclusive.set(name, (inclusive.get(name) ?? 0) + pp.gb);
  }
}

const report = (title: string, rows: Map<string, number>): void => {
  console.log(`\n== ${title}`);
  for (const [key, bytes] of [...rows].sort((a, b) => b[1] - a[1]).slice(0, top)) {
    console.log(`${mib(bytes)} MiB  ${key}`);
  }
};
report("live at t-gmax by allocation site", sites);
report("live at t-gmax by wado function (inclusive)", inclusive);

const stacks = Number(values.stacks);
if (stacks > 0) {
  console.log("\n== largest stacks");
  for (const pp of [...kept].sort((a, b) => b.gb - a.gb).slice(0, stacks)) {
    console.log(`${mib(pp.gb)} MiB in ${pp.gbk} blocks`);
    for (const i of pp.fs) console.log(`    ${dhat.ftbl[i]}`);
  }
}
