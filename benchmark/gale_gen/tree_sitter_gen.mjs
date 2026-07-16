// tree-sitter generate benchmark: time `tree-sitter generate` over the authored
// tree-sitter Rust grammar (grammar.js), the reference for gale-gen.
//
// This runs the real `tree-sitter` CLI end to end — it evaluates grammar.js
// through its JS DSL to grammar.json, then builds the LR/GLR parse tables and
// emits parser.c. That is the full "grammar file -> generated parser" job, the
// peer to `gale gen` reading a `.g4`. The heavy step (table construction) is
// native Rust; the throughput basis is the authored grammar.js size, matching
// how gale-gen reports over its `.g4` bytes.
//
// Requires the `tree-sitter` CLI on PATH (`cargo install tree-sitter-cli`) and
// `node`. Skips gracefully when the CLI is absent.

import { execFileSync } from 'node:child_process';
import { mkdtempSync, copyFileSync, rmSync, statSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const GRAMMAR = join(here, 'grammar.js');
const CONFIG = join(here, 'tree-sitter.json');
const ITERS = 3; // keep the fastest; generation is deterministic

function hasTreeSitter() {
  try {
    execFileSync('tree-sitter', ['--version'], { stdio: 'ignore' });
    return true;
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

if (!hasTreeSitter()) {
  console.log('SKIP: tree-sitter CLI not found (cargo install tree-sitter-cli)');
  process.exit(0);
}

const grammarBytes = statSync(GRAMMAR).size;
const work = mkdtempSync(join(tmpdir(), 'ts-gen-'));
try {
  copyFileSync(GRAMMAR, join(work, 'grammar.js'));
  copyFileSync(CONFIG, join(work, 'tree-sitter.json'));

  let best = Infinity;
  for (let i = 0; i < ITERS; i++) {
    const start = process.hrtime.bigint();
    execFileSync('tree-sitter', ['generate'], { cwd: work, stdio: 'ignore' });
    const secs = Number(process.hrtime.bigint() - start) / 1e9;
    if (secs < best) best = secs;
  }

  const ms = (best * 1000).toFixed(3);
  console.log(`tree-sitter (generate): ${formatRate(grammarBytes, best)}   (${ms} ms/iter, 1 iter)`);
} finally {
  rmSync(work, { recursive: true, force: true });
}
