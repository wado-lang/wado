// ANTLR4 `generate` timed over the same Rust grammar gale-gen uses — the
// apples-to-apples reference (Gale is ANTLR4-compatible; identical `.g4`).
// Throughput is over the shared `.g4` size. Needs `java`; the jar is cached
// under ~/.cache/gale and fetched on first use, else the row skips.

import { execFileSync } from 'node:child_process';
import { mkdtempSync, copyFileSync, rmSync, statSync, existsSync, mkdirSync } from 'node:fs';
import { tmpdir, homedir } from 'node:os';
import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const GRAMMARS = join(here, '..', '..', 'package-gale', 'tests', 'grammars');
const G4 = ['RustLexer.g4', 'RustParser.g4'];
const ANTLR_VERSION = '4.13.2';
const JAR = join(homedir(), '.cache', 'gale', `antlr-${ANTLR_VERSION}-complete.jar`);
const JAR_URL = `https://www.antlr.org/download/antlr-${ANTLR_VERSION}-complete.jar`;
const ITERS = 3;

function has(cmd, args) {
  try {
    execFileSync(cmd, args, { stdio: 'ignore' });
    return true;
  } catch {
    return false;
  }
}

function ensureJar() {
  if (existsSync(JAR)) return true;
  mkdirSync(dirname(JAR), { recursive: true });
  try {
    execFileSync('curl', ['-fsSL', '-o', JAR, JAR_URL], { stdio: 'ignore' });
    return existsSync(JAR);
  } catch {
    return false;
  }
}

function formatRate(bytes, secs) {
  const rate = bytes / secs;
  if (rate >= 1e6) return `${(rate / 1e6).toFixed(2)} MB/s`;
  if (rate >= 1e3) return `${(rate / 1e3).toFixed(2)} KB/s`;
  return `${rate.toFixed(2)} B/s`;
}

if (!has('java', ['-version'])) {
  console.log('SKIP: java not found (needed to run the ANTLR4 jar)');
  process.exit(0);
}
if (!ensureJar()) {
  console.log(`SKIP: cannot obtain ${JAR_URL}`);
  process.exit(0);
}

const g4Bytes = G4.reduce((n, f) => n + statSync(join(GRAMMARS, f)).size, 0);
const work = mkdtempSync(join(tmpdir(), 'antlr-gen-'));
try {
  for (const f of G4) copyFileSync(join(GRAMMARS, f), join(work, f));

  let best = Infinity;
  for (let i = 0; i < ITERS; i++) {
    const start = process.hrtime.bigint();
    execFileSync('java', ['-jar', JAR, '-Dlanguage=Java', ...G4], { cwd: work, stdio: 'ignore' });
    const secs = Number(process.hrtime.bigint() - start) / 1e9;
    if (secs < best) best = secs;
  }

  const ms = (best * 1000).toFixed(3);
  console.log(`antlr4 (generate): ${formatRate(g4Bytes, best)}   (${ms} ms/iter, 1 iter)`);
} finally {
  rmSync(work, { recursive: true, force: true });
}
