#!/usr/bin/env node
// Aggregate the EMI shards' calibration reports into one Markdown job summary.
// A shard that died before writing one is named in the output, so the totals
// cannot read as full coverage when they are not.
import { existsSync, readFileSync, readdirSync } from 'node:fs';
import { join } from 'node:path';

const root = process.argv[2] ?? 'shards';
const dirs = existsSync(root) ? readdirSync(root).sort() : [];

const total = { scanned: 0, eligible: 0, sites: 0, excluded: 0, findings: 0 };
const buckets = new Map<string, number>();
const findings: string[] = [];
const missing: string[] = [];

function count(text: string, pattern: RegExp): number {
  const match = text.match(pattern);
  return match ? Number(match[1]) : 0;
}

for (const dir of dirs) {
  const path = join(root, dir, 'calibration.txt');
  if (!existsSync(path)) {
    missing.push(dir);
    continue;
  }
  const text = readFileSync(path, 'utf8');
  total.scanned += count(text, /^fixtures scanned: (\d+)$/m);
  total.eligible += count(text, /^eligible: (\d+)/m);
  total.sites += count(text, /^eligible: \d+ \((\d+) injection sites\)$/m);
  total.excluded += count(text, /^excluded: (\d+)$/m);
  total.findings += count(text, /^findings: (\d+)$/m);

  let bucket: string | null = null;
  for (const line of text.split('\n')) {
    const header = line.match(/^=== (.+?) \((\d+)\) ===$/);
    if (header) {
      buckets.set(header[1], (buckets.get(header[1]) ?? 0) + Number(header[2]));
      bucket = null;
      continue;
    }
    if (line === '=== findings ===') {
      bucket = 'findings';
      continue;
    }
    if (bucket === 'findings' && line.trim() !== '') {
      findings.push(line);
    }
  }
}

const out: string[] = ['## EMI calibration', ''];
out.push('| Metric | Value |', '| --- | --- |');
out.push(`| fixtures scanned | ${total.scanned} |`);
out.push(`| eligible | ${total.eligible} (${total.sites} injection sites) |`);
out.push(`| excluded | ${total.excluded} |`);
out.push(`| findings | ${total.findings} |`);
out.push('');

if (findings.length > 0) {
  out.push('### Findings', '', '```', ...findings, '```', '');
}

if (buckets.size > 0) {
  out.push('### Exclusions', '', '| Reason | Count |', '| --- | --- |');
  for (const [name, n] of [...buckets].sort((a, b) => b[1] - a[1])) {
    out.push(`| ${name} | ${n} |`);
  }
  out.push('');
}

if (missing.length > 0) {
  out.push('### Shards that reported nothing', '', ...missing.map((d) => `- ${d}`), '');
}

process.stdout.write(`${out.join('\n')}\n`);
