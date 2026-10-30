// The fuel benchmarks: every `test "fuel"` region in benchmark/, as series.
//
// Usage (from the repository root): node benchmark/fuel.ts series|check|write
//
//   series  prints the series at every level of RECORDED_LEVELS
//   check   fails on any difference from benchmark/fuel.json, naming each
//           row that moved
//   write   rewrites benchmark/fuel.json
//
// Each mode first measures, under `wado test --report-fuel` (the dev build,
// through scripts/dev-bin.sh). Only `-O2` is held to a baseline: it is the
// level a performance change answers for, and the others are for reference. A series is named
// `<benchmark directory>/<region label> (<level>)` and holds the region's
// fuel, in github-action-benchmark's `customSmallerIsBetter` form. Fuel does
// not depend on the host, so a difference is the program's or wasmtime's,
// never noise.

import { execFileSync } from "node:child_process";
import { readFileSync, writeFileSync } from "node:fs";
import { basename, dirname } from "node:path";

const RECORDED_LEVELS = ["-O1", "-O2", "-O3"];
const BASELINE_LEVELS = ["-O2"];
const REPORT = "build/fuel.json";
const BASELINE = "benchmark/fuel.json";

type Region = { label: string; fuel: number };
type Entry = { file: string; test: string; fuel: number; regions: Region[] };
type Series = { name: string; unit: string; value: number };

// `freelist` is the allocator a shipped program gets. The test world's
// default, `debug`, never reuses memory, and its cost is not the program's.
function measure(level: string): Series[] {
  execFileSync(
    "scripts/dev-bin.sh",
    ["wado", "test", level, "--allocator", "freelist", "--report-fuel", "--test-name", "fuel", "benchmark"],
    { stdio: ["ignore", process.stderr, "inherit"] },
  );
  const entries = JSON.parse(readFileSync(REPORT, "utf8")) as Entry[];
  const series = entries
    .filter((entry) => entry.test === "fuel")
    .flatMap((entry) =>
      entry.regions.map((region) => ({
        name: `${basename(dirname(entry.file))}/${region.label} (${level})`,
        unit: "fuel",
        value: region.fuel,
      })),
    );
  if (series.length === 0) {
    throw new Error(`${REPORT} holds no fuel region at ${level}`);
  }
  return series;
}

function measureAll(levels: string[]): Series[] {
  const series = levels.flatMap(measure);
  if (new Set(series.map((s) => s.name)).size !== series.length) {
    throw new Error("two regions share a series name");
  }
  return series.sort((a, b) => a.name.localeCompare(b.name));
}

function render(series: Series[]): string {
  return JSON.stringify(series, null, 2) + "\n";
}

function differs(current: Series[], baseline: Series[]): boolean {
  const before = new Map(baseline.map((s) => [s.name, s.value]));
  const after = new Map(current.map((s) => [s.name, s.value]));
  let moved = false;
  for (const name of new Set([...before.keys(), ...after.keys()])) {
    const b = before.get(name);
    const a = after.get(name);
    if (a === b) continue;
    moved = true;
    const ratio = a !== undefined && b !== undefined ? ` (${((a / b - 1) * 100).toFixed(2)}%)` : "";
    console.error(`${name}: ${b ?? "absent"} -> ${a ?? "absent"}${ratio}`);
  }
  if (moved) {
    console.error(`\nfuel differs from ${BASELINE}: run \`mise run update-baselines\` and commit it`);
  }
  return moved;
}

function main(): void {
  const mode = process.argv[2];
  if (mode !== "series" && mode !== "check" && mode !== "write") {
    console.error("usage: node benchmark/fuel.ts series|check|write");
    process.exit(1);
  }
  const current = measureAll(mode === "series" ? RECORDED_LEVELS : BASELINE_LEVELS);
  if (mode === "series") {
    process.stdout.write(render(current));
  } else if (mode === "write") {
    writeFileSync(BASELINE, render(current));
  } else if (differs(current, JSON.parse(readFileSync(BASELINE, "utf8")) as Series[])) {
    process.exit(1);
  }
}

main();
