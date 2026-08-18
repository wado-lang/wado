// Build and run the ANTLR4 generate benchmark: compile Antlr4GenBench against
// the jar's bundled Tool, over the same grammars the Gale row uses. The jar is
// cached under ~/.cache/gale; skips without java/javac.

import { execFileSync } from 'node:child_process';
import { mkdtempSync, copyFileSync, rmSync, existsSync, mkdirSync } from 'node:fs';
import { tmpdir, homedir } from 'node:os';
import { join, dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const GRAMMARS = resolve(here, '..', '..', 'package-gale', 'tests', 'grammars');
const G4 = ['RustLexer.g4', 'RustParser.g4'];
const HARNESS = join(here, 'Antlr4GenBench.java');
const ANTLR_VERSION = '4.13.2';
const JAR = join(homedir(), '.cache', 'gale', `antlr-${ANTLR_VERSION}-complete.jar`);
const JAR_URL = `https://www.antlr.org/download/antlr-${ANTLR_VERSION}-complete.jar`;
const SEP = process.platform === 'win32' ? ';' : ':';

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

if (!has('java', ['-version']) || !has('javac', ['-version'])) {
  console.log('SKIP: java/javac not found (needed to build the ANTLR4 generate benchmark)');
  process.exit(0);
}
if (!ensureJar()) {
  console.log(`SKIP: cannot obtain ${JAR_URL}`);
  process.exit(0);
}

const work = mkdtempSync(join(tmpdir(), 'antlr-gen-'));
try {
  for (const f of G4) copyFileSync(join(GRAMMARS, f), join(work, f));
  copyFileSync(HARNESS, join(work, 'Antlr4GenBench.java'));
  mkdirSync(join(work, 'out'), { recursive: true });
  execFileSync('javac', ['-cp', JAR, 'Antlr4GenBench.java'], { cwd: work, stdio: 'inherit' });
  execFileSync('java', ['-cp', `.${SEP}${JAR}`, 'Antlr4GenBench', 'out', ...G4], {
    cwd: work,
    stdio: 'inherit',
  });
} finally {
  rmSync(work, { recursive: true, force: true });
}
