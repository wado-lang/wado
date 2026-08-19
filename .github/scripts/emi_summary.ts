#!/usr/bin/env node
// Aggregate the EMI shards' stage reports into one Markdown job summary.
// A shard that did not write a report is named in the output, so the totals
// cannot read as full coverage when they are not.
import { existsSync, readFileSync, readdirSync } from 'node:fs';
import { join } from 'node:path';

const root = process.argv[2] ?? 'shards';
const dirs = existsSync(root) ? readdirSync(root).sort() : [];

type Stage = {
  total: { scanned: number; eligible: number; sites: number; excluded: number; findings: number };
  buckets: Map<string, number>;
  findings: string[];
  missing: string[];
};

function count(text: string, pattern: RegExp): number {
  const match = text.match(pattern);
  return match ? Number(match[1]) : 0;
}

function read(file: string): Stage {
  const stage: Stage = {
    total: { scanned: 0, eligible: 0, sites: 0, excluded: 0, findings: 0 },
    buckets: new Map(),
    findings: [],
    missing: [],
  };
  for (const dir of dirs) {
    const path = join(root, dir, file);
    if (!existsSync(path)) {
      stage.missing.push(dir);
      continue;
    }
    const text = readFileSync(path, 'utf8');
    stage.total.scanned += count(text, /^fixtures scanned: (\d+)$/m);
    stage.total.eligible += count(text, /^eligible: (\d+)/m);
    stage.total.sites += count(text, /^eligible: \d+ \((\d+) injection sites\)$/m);
    stage.total.excluded += count(text, /^excluded: (\d+)$/m);
    stage.total.findings += count(text, /^findings: (\d+)$/m);

    let inFindings = false;
    for (const line of text.split('\n')) {
      const header = line.match(/^=== (.+?) \((\d+)\) ===$/);
      if (header) {
        stage.buckets.set(header[1], (stage.buckets.get(header[1]) ?? 0) + Number(header[2]));
        inFindings = false;
        continue;
      }
      if (line === '=== findings ===') {
        inFindings = true;
        continue;
      }
      if (inFindings && line.trim() !== '') {
        stage.findings.push(line);
      }
    }
  }
  return stage;
}

function render(title: string, stage: Stage): string[] {
  const out = [`## ${title}`, ''];
  if (stage.missing.length > 0) {
    out.push(
      `> ${stage.missing.length} of ${dirs.length} shards reported nothing for this stage.`,
      '> The totals below cover the rest.',
      '',
    );
  }
  out.push('| Metric | Value |', '| --- | --- |');
  out.push(`| fixtures scanned | ${stage.total.scanned} |`);
  out.push(`| eligible | ${stage.total.eligible} (${stage.total.sites} injection sites) |`);
  out.push(`| excluded | ${stage.total.excluded} |`);
  out.push(`| findings | ${stage.total.findings} |`);
  out.push('');

  if (stage.findings.length > 0) {
    out.push('### Findings', '', '```', ...stage.findings, '```', '');
  }
  if (stage.buckets.size > 0) {
    out.push('### Exclusions', '', '| Reason | Count |', '| --- | --- |');
    for (const [name, n] of [...stage.buckets].sort((a, b) => b[1] - a[1])) {
      out.push(`| ${name} | ${n} |`);
    }
    out.push('');
  }
  if (stage.missing.length > 0) {
    out.push('### Shards that reported nothing', '', ...stage.missing.map((d) => `- ${d}`), '');
  }
  return out;
}

process.stdout.write(
  `${[
    ...render('EMI calibration', read('calibration.txt')),
    ...render('EMI mutation', read('mutation.txt')),
  ].join('\n')}\n`,
);
