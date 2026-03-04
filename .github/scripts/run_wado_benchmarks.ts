#!/usr/bin/env node
// Run Wado-only runtime benchmarks and output JSON for github-action-benchmark.
import { execFileSync } from 'node:child_process';

const WADO = './target/release/wado';

type BenchResult = { name: string; unit: string; value: number };

function runBench(src: string): string {
  return execFileSync(WADO, ['run', '-O2', src], { encoding: 'utf8' });
}

function parseMs(output: string, pattern: RegExp = /Elapsed: (\d+) ms/): number {
  const m = output.match(pattern);
  if (!m) throw new Error(`Pattern ${pattern} not found in: ${JSON.stringify(output)}`);
  return parseInt(m[1], 10);
}

const benchmarks: BenchResult[] = [];

let output = runBench('benchmark/count_prime/count_prime.wado');
benchmarks.push({ name: 'count_prime', unit: 'ms', value: parseMs(output) });

output = runBench('benchmark/mandelbrot/mandelbrot.wado');
benchmarks.push({ name: 'mandelbrot', unit: 'ms', value: parseMs(output) });

output = runBench('benchmark/sieve/sieve.wado');
benchmarks.push({ name: 'sieve', unit: 'ms', value: parseMs(output) });

output = runBench('benchmark/fts/fts.wado');
benchmarks.push({ name: 'fts', unit: 'ms', value: parseMs(output) });

output = runBench('benchmark/zlib/zlib_bench.wado');
benchmarks.push({ name: 'zlib/compress', unit: 'ms', value: parseMs(output, /Compress: (\d+) ms/) });
benchmarks.push({ name: 'zlib/decompress', unit: 'ms', value: parseMs(output, /Decompress: (\d+) ms/) });

process.stdout.write(JSON.stringify(benchmarks, null, 2) + '\n');
