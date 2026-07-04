#!/usr/bin/env node
/**
 * Aggregate a Wado guest-Wasm profile (`wado run --profile guest,…`) into
 * per-function hot spots.
 *
 * The profile is Firefox-Profiler JSON. For every sample this walks the stack
 * from the leaf up through `stackTable.prefix`, counting:
 *
 *   - SELF (leaf): the function the sample landed *in* — the code actually
 *     burning cycles. This is what you optimize.
 *   - INCLUSIVE: every distinct function on the stack — the caller tree, so you
 *     can see which high-level operation the self-time rolls up into.
 *
 * Counts are combined across all threads (a guest CLI program is single-thread;
 * CM-async may add more). Function names carry monomorphization detail, so each
 * instantiation (`List<f64>^Serialize::serialize<…>`) is counted separately.
 *
 * Runs directly on Node.js >= 23.6 (type stripping is on by default):
 *   ./analyze_guest_profile.ts profile.json [--top N]
 */
import { readFileSync } from "node:fs";
import { parseArgs } from "node:util";

interface Thread {
  stringArray: string[];
  samples: { stack: (number | null)[]; length: number };
  stackTable: { frame: number[]; prefix: (number | null)[] };
  frameTable: { func: number[] };
  funcTable: { name: number[] };
}

const { values, positionals } = parseArgs({
  options: { top: { type: "string", short: "n", default: "25" } },
  allowPositionals: true,
});

const path = positionals[0];
if (!path) {
  console.error("usage: analyze_guest_profile.ts PROFILE.json [--top N]");
  process.exit(1);
}
const top = Number(values.top);

const data = JSON.parse(readFileSync(path, "utf8")) as { threads: Thread[] };

const self = new Map<string, number>();
const inclusive = new Map<string, number>();
let total = 0;

const bump = (m: Map<string, number>, k: string) => m.set(k, (m.get(k) ?? 0) + 1);

for (const t of data.threads) {
  const nameOf = (stackIdx: number): string => {
    const frame = t.stackTable.frame[stackIdx];
    const func = t.frameTable.func[frame];
    return t.stringArray[t.funcTable.name[func]];
  };
  for (const leaf of t.samples.stack) {
    if (leaf == null) continue;
    total++;
    bump(self, nameOf(leaf));
    const seen = new Set<string>();
    let cur: number | null = leaf;
    while (cur != null) {
      const nm = nameOf(cur);
      if (!seen.has(nm)) {
        seen.add(nm);
        bump(inclusive, nm);
      }
      cur = t.stackTable.prefix[cur];
    }
  }
}

const report = (title: string, m: Map<string, number>) => {
  console.log(`--- ${title} (top ${top}) ---`);
  const rows = [...m.entries()].sort((a, b) => b[1] - a[1]).slice(0, top);
  for (const [name, count] of rows) {
    const pct = ((100 * count) / total).toFixed(1).padStart(5);
    console.log(`${String(count).padStart(7)} ${pct}%  ${name}`);
  }
};

console.log(`total samples: ${total}`);
report("SELF (leaf)", self);
report("INCLUSIVE", inclusive);
