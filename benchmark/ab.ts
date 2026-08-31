// Two-arm comparison for Wado benchmark logs.
//
// Usage: node ab.ts --base b1.log b2.log b3.log --head h1.log h2.log h3.log
//
// Per row: each arm's best ms/iter, the delta, and a verdict. The verdict is
// range overlap, not the delta — a row whose two arms' [min, max] overlap is
// noise however far apart their bests happen to land, and one whose ranges are
// disjoint is real however small the gap. Reading the deltas alone is what
// turns a 6% swing on a 5 ms benchmark into a regression that is not there.
//
// The reference rows (C, Rust, JavaScript) run the same binary in both arms, so
// they are the control: a `SLOWER`/`FASTER` among them means the host was not
// idle and the Wado rows cannot be read either.

import { group, labelOf, type Row } from "./logs.ts";

// `min` is also the arm's best: every run of a row does the same fixed work, so
// the shortest time is the highest true throughput.
type Arm = { min: number; max: number };

function arm(rows: Row[]): Arm {
  const ms = rows.map((r) => r.ms);
  return { min: Math.min(...ms), max: Math.max(...ms) };
}

function splitArgs(argv: string[]): { base: string[]; head: string[] } {
  const base: string[] = [];
  const head: string[] = [];
  let into: string[] | null = null;
  for (const a of argv) {
    if (a === "--base") into = base;
    else if (a === "--head") into = head;
    else if (into) into.push(a);
    else {
      console.error(`unexpected argument before --base/--head: ${a}`);
      process.exit(1);
    }
  }
  return { base, head };
}

function main(): void {
  const { base, head } = splitArgs(process.argv.slice(2));
  // One run per arm leaves each range a point, and two points never overlap —
  // every row would read as real. The verdict needs a spread to compare.
  if (base.length < 2 || head.length < 2) {
    console.error(
      "usage: node ab.ts --base <run.log>... --head <run.log>...  (2+ runs per arm)",
    );
    process.exit(1);
  }

  const baseRows = group(base);
  const headRows = group(head);

  type Verdict = { label: string; b: Arm; h: Arm; delta: number; real: boolean };
  const out: Verdict[] = [];
  for (const [key, rows] of headRows) {
    const other = baseRows.get(key);
    if (!other) {
      process.stderr.write(`# warning: only in head: ${labelOf(rows[0])}\n`);
      continue;
    }
    const b = arm(other);
    const h = arm(rows);
    out.push({
      label: labelOf(rows[0]),
      b,
      h,
      // Positive is faster: the row spends less time per iteration.
      delta: ((b.min - h.min) / b.min) * 100,
      real: h.min > b.max || b.min > h.max,
    });
  }
  for (const [key, rows] of baseRows) {
    if (!headRows.has(key)) {
      process.stderr.write(`# warning: only in base: ${labelOf(rows[0])}\n`);
    }
  }

  // Regressions first: what a tuning run has to explain before it lands.
  out.sort((x, y) => Number(y.real) - Number(x.real) || x.delta - y.delta);
  for (const v of out) {
    const verdict = !v.real ? "noise" : v.delta > 0 ? "FASTER" : "SLOWER";
    console.log(
      [
        `${v.delta >= 0 ? "+" : ""}${v.delta.toFixed(1)}%`.padStart(7),
        verdict.padEnd(6),
        `${v.b.min.toFixed(3)} -> ${v.h.min.toFixed(3)} ms`.padStart(24),
        `[${v.b.min.toFixed(3)}-${v.b.max.toFixed(3)}] [${v.h.min.toFixed(3)}-${v.h.max.toFixed(3)}]`.padStart(
          31,
        ),
        v.label,
      ].join("  "),
    );
  }
}

main();
