#!/usr/bin/env node
/**
 * Break the compiler's span trace (`--log-level debug`) down into wall time
 * per phase, as a tree.
 *
 * Spans of one name are summed under their parent: a name is grouped by its
 * first word, so `nir/iteration 3` counts toward `nir/iteration` and
 * `load ./ir.wado` toward `load`. Each row shows the summed wall time, its
 * share of the run, the number of spans, and the peak RSS they reached.
 *
 * Runs directly on Node.js >= 23.6 (type stripping is on by default):
 *   ./analyze_spans.ts TRACE.log [--min SECONDS] [--each RE]
 *
 * `--min` hides a row below that many seconds (default 0.1). `--each RE`
 * also lists every span whose full name matches RE, in order, with its start
 * and duration: `--each '^nir/iteration'` shows how the fixed point converges.
 */
import { readFileSync } from "node:fs";
import { parseArgs } from "node:util";

interface Node {
  wall: number;
  count: number;
  peakMiB: number;
  children: Map<string, Node>;
}

const { values, positionals } = parseArgs({
  allowPositionals: true,
  options: {
    min: { type: "string", default: "0.1" },
    each: { type: "string" },
  },
});
if (positionals.length !== 1) {
  console.error("usage: analyze_spans.ts TRACE.log [--min SECONDS] [--each RE]");
  process.exit(2);
}
const min = Number(values.min);
const each = values.each === undefined ? null : new RegExp(values.each);

// `[hh:mm:ss.ffff] >> name · rss cur/peak MiB`; the RSS suffix is Linux only.
const line = /^\[(\d+):(\d+):(\d+\.\d+)\] (>>|<<) (.*?)(?: · rss (\d+)\/(\d+) MiB.*)?$/;
const newNode = (): Node => ({ wall: 0, count: 0, peakMiB: 0, children: new Map() });
const root = newNode();
const open: { name: string; start: number; node: Node }[] = [];
const listed: string[] = [];
let end = 0;

for (const text of readFileSync(positionals[0], "utf8").split("\n")) {
  const m = line.exec(text);
  if (m === null) continue;
  const at = Number(m[1]) * 3600 + Number(m[2]) * 60 + Number(m[3]);
  end = at;
  const name = m[5];
  if (m[4] === ">>") {
    const parent = open.length === 0 ? root : open[open.length - 1].node;
    const key = name.split(" ")[0];
    let node = parent.children.get(key);
    if (node === undefined) {
      node = newNode();
      parent.children.set(key, node);
    }
    open.push({ name, start: at, node });
    continue;
  }
  // The host keeps its RSS samples on a stack, so spans nest strictly.
  const span = open.pop();
  if (span === undefined || span.name !== name) {
    throw new Error(`span end '${name}' does not close the innermost open span '${span?.name}'`);
  }
  span.node.wall += at - span.start;
  span.node.count += 1;
  if (m[7] !== undefined) span.node.peakMiB = Math.max(span.node.peakMiB, Number(m[7]));
  if (each?.test(name)) {
    listed.push(`${span.start.toFixed(2).padStart(8)}s  ${(at - span.start).toFixed(2).padStart(7)}s  ${name}`);
  }
}

console.log(`run: ${end.toFixed(2)}s wall (last trace line)\n`);
console.log("    wall   share      n   peak  span");
const print = (node: Node, depth: number): void => {
  const rows = [...node.children].sort((a, b) => b[1].wall - a[1].wall);
  for (const [key, child] of rows) {
    if (child.wall < min) continue;
    console.log(
      `${child.wall.toFixed(2).padStart(7)}s ${((100 * child.wall) / end).toFixed(1).padStart(6)}% ${String(child.count).padStart(6)} ${String(child.peakMiB).padStart(6)}  ${"  ".repeat(depth)}${key}`,
    );
    print(child, depth + 1);
  }
};
print(root, 0);

if (each !== null) {
  console.log(`\nspans matching /${values.each}/:\n   start      wall  span`);
  for (const row of listed) console.log(row);
}
