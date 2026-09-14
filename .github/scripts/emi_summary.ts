#!/usr/bin/env node
// Aggregate the EMI shards' stage reports into one Markdown job summary.
// A shard that did not write a report is named in the output, so the totals
// cannot read as full coverage when they are not.
import { existsSync, readFileSync, readdirSync } from 'node:fs';
import { join } from 'node:path';

const root = process.argv[2] ?? 'shards';
const dirs = existsSync(root) ? readdirSync(root).sort() : [];

// The one section the summary leads with: findings are why a run fails.
const FINDINGS = 'findings';

type Stage = {
  total: { scanned: number; eligible: number; sites: number; excluded: number; findings: number };
  buckets: Map<string, number>;
  shapes: Map<string, { sources: number; sites: number }>;
  // Every other section's lines, concatenated across shards, so a section the
  // report gains reaches the summary without being named here.
  sections: Map<string, string[]>;
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
    shapes: new Map(),
    sections: new Map(),
    missing: [],
  };
  for (const dir of dirs) {
    const path = join(root, dir, file);
    if (!existsSync(path)) {
      stage.missing.push(dir);
      continue;
    }
    const text = readFileSync(path, 'utf8');
    stage.total.scanned += count(text, /^sources scanned: (\d+)$/m);
    stage.total.eligible += count(text, /^eligible: (\d+)/m);
    stage.total.sites += count(text, /^eligible: \d+ \((\d+) injection sites\)$/m);
    stage.total.excluded += count(text, /^excluded: (\d+)$/m);
    stage.total.findings += count(text, /^findings: (\d+)$/m);

    let section = '';
    for (const line of text.split('\n')) {
      const counted = line.match(/^=== (.+?) \((\d+)\) ===$/);
      if (counted) {
        stage.buckets.set(counted[1], (stage.buckets.get(counted[1]) ?? 0) + Number(counted[2]));
        section = '';
        continue;
      }
      if (line.startsWith('=== ')) {
        section = line.slice(4, -4).trim();
        continue;
      }
      if (section === 'guard shapes') {
        const shape = line.match(/^(\S+): (\d+) source\(s\) \((\d+) sites\)$/);
        if (shape) {
          const seen = stage.shapes.get(shape[1]) ?? { sources: 0, sites: 0 };
          stage.shapes.set(shape[1], {
            sources: seen.sources + Number(shape[2]),
            sites: seen.sites + Number(shape[3]),
          });
        }
        continue;
      }
      if (section !== '' && line.trim() !== '') {
        const lines = stage.sections.get(section) ?? [];
        lines.push(line);
        stage.sections.set(section, lines);
      }
    }
  }
  return stage;
}

function table(title: string, columns: string[], rows: string[][]): string[] {
  if (rows.length === 0) {
    return [];
  }
  return [
    ...(title ? [`### ${title}`, ''] : []),
    `| ${columns.join(' | ')} |`,
    `| ${columns.map(() => '---').join(' | ')} |`,
    ...rows.map((row) => `| ${row.join(' | ')} |`),
    '',
  ];
}

// A section the summary has no table for, kept verbatim under its own heading.
function verbatim(name: string, lines: string[]): string[] {
  const heading = name.charAt(0).toUpperCase() + name.slice(1);
  return [`### ${heading}`, '', '```', ...lines, '```', ''];
}

function render(title: string, stage: Stage): string[] {
  const out = [`## ${title}`, ''];
  if (stage.missing.length > 0) {
    out.push(
      `> ${stage.missing.length} of ${dirs.length} shards reported nothing; these totals cover the rest.`,
      '',
    );
  }
  out.push(
    ...table(
      '',
      ['Metric', 'Value'],
      [
        ['sources scanned', `${stage.total.scanned}`],
        ['eligible', `${stage.total.eligible} (${stage.total.sites} injection sites)`],
        ['excluded', `${stage.total.excluded}`],
        ['findings', `${stage.total.findings}`],
      ],
    ),
  );

  const findings = stage.sections.get(FINDINGS);
  if (findings) {
    out.push(...verbatim(FINDINGS, findings));
  }
  out.push(
    ...table(
      'Guard shapes',
      ['Shape', 'Sources', 'Sites'],
      [...stage.shapes].map(([name, { sources, sites }]) => [name, `${sources}`, `${sites}`]),
    ),
    ...table(
      'Exclusions',
      ['Reason', 'Count'],
      [...stage.buckets].sort((a, b) => b[1] - a[1]).map(([name, n]) => [name, `${n}`]),
    ),
  );
  for (const [name, lines] of stage.sections) {
    if (name !== FINDINGS) {
      out.push(...verbatim(name, lines));
    }
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
