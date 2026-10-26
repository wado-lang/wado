// SZZ census over wado-lang/wado: for each fix PR merged into main, blame the
// lines it changed back to the PR that introduced them.
// Usage: node szz.mjs <repo> <out.json>
// Needs main's whole history: `git fetch --unshallow origin main` in a shallow clone.
import { execFileSync } from "node:child_process";
import { writeFileSync } from "node:fs";

const repo = process.argv[2];
const out = process.argv[3];
const git = (...args) =>
  execFileSync("git", ["-C", repo, ...args], { encoding: "utf8", maxBuffer: 1 << 30 });

// Merge commits on main's first-parent chain = merged PRs.
const merges = git("log", "--first-parent", "--merges", "--reverse", "--format=%H%x00%ad%x00%s%x00%b%x1e", "--date=short", "origin/main")
  .split("\x1e").map((r) => r.trim()).filter(Boolean).map((r) => {
    const [sha, date, subject, body] = r.split("\x00");
    const num = Number((subject.match(/#(\d+)/) || [])[1] || 0);
    const title = (body || "").split("\n")[0].trim();
    return { sha, date, num, title };
  });
console.error(`PRs: ${merges.length}`);

// commit -> PR number, for commits brought in by each merge.
const commitPr = new Map();
for (const m of merges) {
  for (const c of git("rev-list", `${m.sha}^1..${m.sha}^2`).split("\n").filter(Boolean)) {
    if (!commitPr.has(c)) commitPr.set(c, m.num);
  }
  commitPr.set(m.sha, m.num);
}
// Direct (non-merge) commits on first parent belong to no PR.

const generated = (paths) => {
  if (paths.length === 0) return new Set();
  const res = execFileSync("git", ["-C", repo, "check-attr", "--stdin", "linguist-generated", "linguist-vendored"], {
    input: paths.join("\n"), encoding: "utf8",
  });
  const s = new Set();
  for (const line of res.split("\n")) {
    const m = line.match(/^(.*): linguist-(generated|vendored): (set|true)$/);
    if (m) s.add(m[1]);
  }
  return s;
};

const isTest = (p) =>
  p.startsWith("wado-compiler/tests/") || /_test\.wado$/.test(p) || /\/tests?\//.test(p) || /(^|\/)test_[^/]*\.rs$/.test(p);
const isDoc = (p) => p.endsWith(".md");

const fixType = /^fix(\([^)]*\))?!?:/;
const results = [];
const fixes = merges.filter((m) => fixType.test(m.title));
console.error(`fix PRs: ${fixes.length}`);
let i = 0;
for (const m of fixes) {
  i++;
  if (i % 50 === 0) console.error(`${i}/${fixes.length}`);
  const base = `${m.sha}^1`;
  const names = git("diff", "--name-status", "-M", base, m.sha).split("\n").filter(Boolean).map((l) => l.split("\t"));
  const paths = names.map((n) => n[n.length - 1]);
  const gen = generated(paths);
  const addsTest = names.some((n) => !gen.has(n[n.length - 1]) && isTest(n[n.length - 1]));
  const src = names.filter((n) => !gen.has(n[n.length - 1]) && !isTest(n[n.length - 1]) && !isDoc(n[n.length - 1]) && n[0][0] !== "A");
  const blamed = new Map(); // PR num -> lines
  let oldLines = 0;
  for (const n of src) {
    const oldPath = n[1];
    const newPath = n[n.length - 1];
    const diff = git("diff", "-U0", "-w", "-M", base, m.sha, "--", oldPath, newPath);
    const ranges = [];
    for (const h of diff.matchAll(/^@@ -(\d+)(?:,(\d+))? \+/gm)) {
      const start = Number(h[1]);
      const count = h[2] === undefined ? 1 : Number(h[2]);
      if (count > 0) ranges.push([start, start + count - 1]);
    }
    if (ranges.length === 0) continue;
    const args = ["blame", "-w", "--porcelain"];
    for (const [a, b] of ranges) args.push("-L", `${a},${b}`);
    let blame;
    try {
      blame = git(...args, base, "--", oldPath);
    } catch {
      continue;
    }
    for (const line of blame.split("\n")) {
      const mm = line.match(/^([0-9a-f]{40}) \d+ \d+/);
      if (!mm) continue;
      oldLines++;
      const pr = commitPr.get(mm[1]) ?? 0;
      blamed.set(pr, (blamed.get(pr) || 0) + 1);
    }
  }
  const ranked = [...blamed.entries()].sort((a, b) => b[1] - a[1]);
  results.push({
    pr: m.num, date: m.date, title: m.title, addsTest, oldLines,
    blamed: ranked.map(([pr, n]) => ({ pr, n })),
  });
}
writeFileSync(out, JSON.stringify({ prs: merges.length, results }, null, 1));
console.error("done");
