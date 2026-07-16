// Build and run the ANTLR4 Java SQLite parser benchmark.
//
// Generates a Java parser from the SAME SQLite.g4 the Gale row uses, compiles it
// with Antlr4SqliteBench.java against the ANTLR4 runtime (bundled in the
// complete jar), and runs the bench over queries.sql. The jar is cached under
// ~/.cache/gale (shared with package-gale's oracle and the gale-gen reference)
// and fetched on first use. Skips gracefully when java/javac are missing or the
// jar cannot be obtained.

import { execFileSync } from 'node:child_process';
import { mkdtempSync, copyFileSync, rmSync, existsSync, mkdirSync } from 'node:fs';
import { tmpdir, homedir } from 'node:os';
import { join, dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const GRAMMAR = resolve(here, '..', '..', 'package-gale', 'tests', 'grammars', 'SQLite.g4');
const HARNESS = join(here, 'Antlr4SqliteBench.java');
const QUERIES = join(here, 'queries.sql');
const ANTLR_VERSION = '4.13.2';
const JAR = join(homedir(), '.cache', 'gale', `antlr-${ANTLR_VERSION}-complete.jar`);
const JAR_URL = `https://www.antlr.org/download/antlr-${ANTLR_VERSION}-complete.jar`;

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
  console.log('SKIP: java/javac not found (needed to build the ANTLR4 Java parser)');
  process.exit(0);
}
if (!ensureJar()) {
  console.log(`SKIP: cannot obtain ${JAR_URL}`);
  process.exit(0);
}

const work = mkdtempSync(join(tmpdir(), 'antlr-sqlite-'));
try {
  copyFileSync(GRAMMAR, join(work, 'SQLite.g4'));
  copyFileSync(HARNESS, join(work, 'Antlr4SqliteBench.java'));
  execFileSync('java', ['-jar', JAR, '-Dlanguage=Java', '-no-listener', 'SQLite.g4'], { cwd: work, stdio: 'inherit' });
  execFileSync('javac', ['-cp', JAR, ...['SQLiteLexer.java', 'SQLiteParser.java', 'Antlr4SqliteBench.java']], { cwd: work, stdio: 'inherit' });
  execFileSync('java', ['-cp', `.${process.platform === 'win32' ? ';' : ':'}${JAR}`, 'Antlr4SqliteBench', QUERIES], { cwd: work, stdio: 'inherit' });
} finally {
  rmSync(work, { recursive: true, force: true });
}
