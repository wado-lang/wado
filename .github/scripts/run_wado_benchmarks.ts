#!/usr/bin/env node
// Run Wado-only runtime benchmarks and output JSON for github-action-benchmark.
// Each benchmark runs at -O1, -O2, and -O3.
//
// We record throughput (work per second; higher is better) so the workflow's
// `customBiggerIsBetter` tool tracks performance directly. `core:benchmark`
// prints a throughput line per phase:
//   <rate> <unit>/s   (<ms> ms/iter, <n> iter)
// and labelled phases (zlib) as `  <label>: <rate>   (<ms> ms/iter, …)`.
// The rate is SI-scaled by the formatter, so we normalize to a stable unit —
// byte rates to MB/s, everything else to `M <unit>/s` (matching README) — so a
// prefix change between runs can't shift a series by 1000x.
import { execFileSync } from 'node:child_process';
import { resolve } from 'node:path';

const WADO = resolve('./target/release/wado');

type BenchResult = { name: string; unit: string; value: number };

function runBench(src: string, optLevel: string): string {
  return execFileSync(WADO, ['run', optLevel, src], { encoding: 'utf8', cwd: 'benchmark' });
}

function escapeRegExp(s: string): string {
  return s.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
}

// Parse a throughput line into a normalized (value, unit). When `label` is
// given (multi-phase benchmarks such as zlib) the matching phase line is
// selected; `\b` keeps `compress` from also matching `decompress`.
function parseRate(output: string, label?: string): { value: number; unit: string } {
  const head = label ? `\\b${escapeRegExp(label)}:\\s+` : `\\s`;
  // <num> then either "<PREFIX>B" / "B" (bytes) or "[<prefix> ]<unit>" before "/s".
  const re = new RegExp(`${head}([\\d.]+)\\s+([A-Za-z]+)(?:\\s+([A-Za-z]+))?/s`);
  const m = output.match(re);
  if (!m) {
    throw new Error(`throughput${label ? ` for '${label}'` : ''} not found in: ${JSON.stringify(output)}`);
  }
  const num = parseFloat(m[1]);
  const [, , t1, t2] = m;
  const si = (p: string): number => (p === 'G' ? 1e9 : p === 'M' ? 1e6 : p === 'k' || p === 'K' ? 1e3 : 1);

  if (t2) {
    // Non-byte rate with a separate SI prefix: t1 = k|M|G, t2 = base unit.
    return { value: (num * si(t1)) / 1e6, unit: `M ${t2}/s` };
  }
  const byte = t1.match(/^([KMG]?)B$/);
  if (byte) {
    return { value: (num * si(byte[1])) / 1e6, unit: 'MB/s' };
  }
  // Bare non-byte unit (rate < 1000): still report in millions for a stable unit.
  return { value: num / 1e6, unit: `M ${t1}/s` };
}

const OPT_LEVELS = ['-O1', '-O2', '-O3'] as const;

const benchmarks: BenchResult[] = [];

function push(name: string, output: string, label?: string): void {
  const { value, unit } = parseRate(output, label);
  benchmarks.push({ name, unit, value });
}

for (const opt of OPT_LEVELS) {
  const label = opt;

  push(`count_prime (${label})`, runBench('count_prime/count_prime.wado', opt));
  push(`mandelbrot (${label})`, runBench('mandelbrot/mandelbrot.wado', opt));
  push(`sieve (${label})`, runBench('sieve/sieve.wado', opt));
  push(`fts (${label})`, runBench('fts/fts.wado', opt));

  const zlib = runBench('zlib/zlib_bench.wado', opt);
  push(`zlib/compress (${label})`, zlib, 'compress');
  push(`zlib/decompress (${label})`, zlib, 'decompress');

  push(`json/twitter (${label})`, runBench('json_twitter/json_twitter.wado', opt));
  push(`json/canada (${label})`, runBench('json_canada/json_canada.wado', opt));
  push(`json/catalog (${label})`, runBench('json_catalog/json_catalog.wado', opt));
  push(`sqlite_parse (${label})`, runBench('sqlite_parse/sqlite_parse.wado', opt));
  push(`syntax_highlight (${label})`, runBench('syntax_highlight/syntax_highlight.wado', opt));
}

process.stdout.write(JSON.stringify(benchmarks, null, 2) + '\n');
