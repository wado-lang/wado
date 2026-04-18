#!/usr/bin/env node
// Compile Wado programs at -Os and measure wasm binary sizes.
import { execFileSync } from 'node:child_process';
import { mkdtempSync, statSync, unlinkSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

const WADO = './target/release/wado';

type SizeResult = { name: string; unit: string; value: number };

const PROGRAMS: [string, string][] = [
  ['hello_world', 'wasm-size/hello_world/hello_world.wado'],
  ['pi_approx', 'wasm-size/pi_approx/pi_approx.wado'],
  ['zlib', 'wasm-size/zlib/zlib.wado'],
  ['sqlite_highlight', 'wasm-size/sqlite_highlight/sqlite_highlight.wado'],
];

const tmp = mkdtempSync(join(tmpdir(), 'wasm-size-'));

function measureSize(name: string, src: string): SizeResult {
  const wasm = join(tmp, `${name}.wasm`);
  try {
    execFileSync(WADO, ['compile', '-Os', '-o', wasm, src], { stdio: 'ignore' });
    const { size } = statSync(wasm);
    return { name, unit: 'bytes', value: size };
  } finally {
    try { unlinkSync(wasm); } catch { /* ignore */ }
  }
}

const results = PROGRAMS.map(([name, src]) => measureSize(name, src));

process.stdout.write(JSON.stringify(results, null, 2) + '\n');
