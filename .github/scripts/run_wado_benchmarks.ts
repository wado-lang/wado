#!/usr/bin/env node
// Run Wado-only runtime benchmarks and output JSON for github-action-benchmark.
// Each benchmark is run at -O1, -O2, and -O3 optimization levels.
import { execFileSync } from 'node:child_process';

const WADO = './target/release/wado';

type BenchResult = { name: string; unit: string; value: number };

function runBench(src: string, optLevel: string): string {
  return execFileSync(WADO, ['run', optLevel, src], { encoding: 'utf8' });
}

function parseMs(output: string, pattern: RegExp = /Elapsed: ([\d.]+) ms/): number {
  const m = output.match(pattern);
  if (!m) throw new Error(`Pattern ${pattern} not found in: ${JSON.stringify(output)}`);
  return Math.round(parseFloat(m[1]));
}

const OPT_LEVELS = ['-O1', '-O2', '-O3'] as const;

const benchmarks: BenchResult[] = [];

for (const opt of OPT_LEVELS) {
  const label = opt;

  let output = runBench('benchmark/count_prime/count_prime.wado', opt);
  benchmarks.push({ name: `count_prime (${label})`, unit: 'ms', value: parseMs(output) });

  output = runBench('benchmark/mandelbrot/mandelbrot.wado', opt);
  benchmarks.push({ name: `mandelbrot (${label})`, unit: 'ms', value: parseMs(output) });

  output = runBench('benchmark/sieve/sieve.wado', opt);
  benchmarks.push({ name: `sieve (${label})`, unit: 'ms', value: parseMs(output) });

  output = runBench('benchmark/fts/fts.wado', opt);
  benchmarks.push({ name: `fts (${label})`, unit: 'ms', value: parseMs(output) });

  output = runBench('benchmark/zlib/zlib_bench.wado', opt);
  benchmarks.push({ name: `zlib/compress (${label})`, unit: 'ms', value: parseMs(output, /Compress: ([\d.]+) ms/) });
  benchmarks.push({ name: `zlib/decompress (${label})`, unit: 'ms', value: parseMs(output, /Decompress: ([\d.]+) ms/) });

  output = runBench('benchmark/json_twitter/json_twitter.wado', opt);
  benchmarks.push({ name: `json/twitter (${label})`, unit: 'ms', value: parseMs(output) });

  output = runBench('benchmark/json_canada/json_canada.wado', opt);
  benchmarks.push({ name: `json/canada (${label})`, unit: 'ms', value: parseMs(output) });

  output = runBench('benchmark/json_catalog/json_catalog.wado', opt);
  benchmarks.push({ name: `json/catalog (${label})`, unit: 'ms', value: parseMs(output) });
}

process.stdout.write(JSON.stringify(benchmarks, null, 2) + '\n');
