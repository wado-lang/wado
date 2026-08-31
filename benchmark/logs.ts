// Parsing shared by `pick.ts` and `ab.ts`: one measured row out of the output
// of `mise run all` (or a single `mise run <task>`).

import { readFileSync } from "node:fs";

export type Row = {
  task: string;
  impl: string;
  phase: string;
  rate: string; // e.g. "1.88 GB/s"
  ms: number;
  iters: number;
  run: number;
};

// Mirrors the shared throughput formatter: byte rates scale as GB/MB/KB/B per
// second, every other unit takes an optional G/M/k prefix before its own name.
// Enumerating only the prefixes seen so far would silently drop the fastest run
// of a row the moment it crosses into the next prefix.
const RATE =
  /([\d.]+)\s+([GMK]?B\/s|(?:[GMk] )?[A-Za-z]+\/s)\s+\(([\d.]+) ms\/iter,\s*(\d+) iter\)/;

// A row is keyed by (task, implementation, phase), read from the mise task
// markers (`[json-twitter] $ ...`), the `=== Impl ===` headers, and the phase
// label on the throughput line (`Ser:` / `de:` / `Compress:` / bare) — not by
// line position, so a benchmark skipped or reordered in one run still lines up.
export function parse(text: string, run: number): Row[] {
  const rows: Row[] = [];
  let task = "";
  let impl = "";
  for (const line of text.split("\n")) {
    const taskMarker = line.match(/^\[([\w-]+)\] \$/);
    if (taskMarker) {
      task = taskMarker[1];
      continue;
    }
    const header = line.match(/^===\s*(.+?)\s*===\s*$/);
    if (header) {
      // Ignore build/setup banners so `impl` stays on the last real one.
      if (!/^(Compiling|Installing)/.test(header[1])) impl = header[1];
      continue;
    }
    const m = line.match(RATE);
    if (!m) continue;
    const before = line.slice(0, m.index);
    const label = before.match(/([A-Za-z][\w .()/-]*?):\s*$/);
    rows.push({
      task,
      impl,
      phase: label ? label[1].trim() : "",
      rate: `${m[1]} ${m[2]}`,
      ms: parseFloat(m[3]),
      iters: parseInt(m[4], 10),
      run,
    });
  }
  return rows;
}

export function keyOf(r: { task: string; impl: string; phase: string }): string {
  return [r.task, r.impl, r.phase].join(" ");
}

export function labelOf(r: { task: string; impl: string; phase: string }): string {
  return [r.task, r.impl, r.phase].filter(Boolean).join(" / ");
}

// Every row of every log, grouped by key, in first-seen order.
export function group(files: string[]): Map<string, Row[]> {
  const byKey = new Map<string, Row[]>();
  files.forEach((f, i) => {
    for (const r of parse(readFileSync(f, "utf8"), i + 1)) {
      const bucket = byKey.get(keyOf(r));
      if (bucket) bucket.push(r);
      else byKey.set(keyOf(r), [r]);
    }
  });
  return byKey;
}
