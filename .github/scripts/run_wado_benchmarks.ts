#!/usr/bin/env node
// Run Wado-only runtime benchmarks and output JSON for github-action-benchmark.
// Each benchmark is run at -O1, -O2, and -O3 optimization levels.
//
// `core:benchmark` reports throughput plus a per-iteration time, formatted as:
//   <rate> <unit>/s   (<ms> ms/iter, <n> iter)
// and labelled phases (zlib) as `  <label>: <rate>   (<ms> ms/iter, …)`.
// We record the per-iteration time (ms/iter) so the metric stays
// smaller-is-better, matching the workflow's `customSmallerIsBetter` tool.
import { execFileSync } from 'node:child_process';
import { resolve } from 'node:path';

const WADO = resolve('./target/release/wado');

type BenchResult = { name: string; unit: string; value: number };

function runBench(src: string, optLevel: string): string {
  return execFileSync(WADO, ['run', optLevel, src], { encoding: 'utf8', cwd: 'benchmark' });
}

// Parse the per-iteration time (ms/iter) from a `core:benchmark` line. When
// `label` is given (multi-phase benchmarks such as zlib) the matching phase
// line is selected; `\b` keeps `compress` from also matching `decompress`.
function parsePerIterMs(output: string, label?: string): number {
  const pattern = label
    ? new RegExp(`\\b${label}:.*?\\(([\\d.]+) ms/iter`)
    : /\(([\d.]+) ms\/iter,/;
  const m = output.match(pattern);
  if (!m) {
    throw new Error(
      `per-iter ms${label ? ` for '${label}'` : ''} not found in: ${JSON.stringify(output)}`,
    );
  }
  return parseFloat(m[1]);
}

const OPT_LEVELS = ['-O1', '-O2', '-O3'] as const;

const benchmarks: BenchResult[] = [];

for (const opt of OPT_LEVELS) {
  const label = opt;

  let output = runBench('count_prime/count_prime.wado', opt);
  benchmarks.push({ name: `count_prime (${label})`, unit: 'ms/iter', value: parsePerIterMs(output) });

  output = runBench('mandelbrot/mandelbrot.wado', opt);
  benchmarks.push({ name: `mandelbrot (${label})`, unit: 'ms/iter', value: parsePerIterMs(output) });

  output = runBench('sieve/sieve.wado', opt);
  benchmarks.push({ name: `sieve (${label})`, unit: 'ms/iter', value: parsePerIterMs(output) });

  output = runBench('fts/fts.wado', opt);
  benchmarks.push({ name: `fts (${label})`, unit: 'ms/iter', value: parsePerIterMs(output) });

  output = runBench('zlib/zlib_bench.wado', opt);
  benchmarks.push({ name: `zlib/compress (${label})`, unit: 'ms/iter', value: parsePerIterMs(output, 'compress') });
  benchmarks.push({ name: `zlib/decompress (${label})`, unit: 'ms/iter', value: parsePerIterMs(output, 'decompress') });

  output = runBench('json_twitter/json_twitter.wado', opt);
  benchmarks.push({ name: `json/twitter (${label})`, unit: 'ms/iter', value: parsePerIterMs(output) });

  output = runBench('json_canada/json_canada.wado', opt);
  benchmarks.push({ name: `json/canada (${label})`, unit: 'ms/iter', value: parsePerIterMs(output) });

  output = runBench('json_catalog/json_catalog.wado', opt);
  benchmarks.push({ name: `json/catalog (${label})`, unit: 'ms/iter', value: parsePerIterMs(output) });

  output = runBench('sqlite_parse/sqlite_parse.wado', opt);
  benchmarks.push({ name: `sqlite_parse (${label})`, unit: 'ms/iter', value: parsePerIterMs(output) });

  output = runBench('syntax_highlight/syntax_highlight.wado', opt);
  benchmarks.push({ name: `syntax_highlight (${label})`, unit: 'ms/iter', value: parsePerIterMs(output) });
}

process.stdout.write(JSON.stringify(benchmarks, null, 2) + '\n');
