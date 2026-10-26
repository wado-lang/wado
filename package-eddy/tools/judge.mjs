// Asks a model whether each SZZ pair is real: the fix PR corrects a defect
// that the culprit PR introduced, visible in the culprit's diff.
// Usage: CLAUDE_API_KEY=... node judge.mjs <repo> <szz.json> <min-share> <out.json>
import { execFileSync } from "node:child_process";
import { readFileSync, writeFileSync } from "node:fs";

const [repo, szzPath, minShare, outPath] = process.argv.slice(2);
const MODEL = process.env.CLAUDE_MODEL || "claude-opus-5-5";
const LIMIT = 60_000;
const git = (...a) => execFileSync("git", ["-C", repo, ...a], { encoding: "utf8", maxBuffer: 1 << 30 });

const mergeOf = new Map();
for (const line of git("log", "--first-parent", "--merges", "--format=%H %s", "origin/main").split("\n")) {
  const m = line.match(/^([0-9a-f]{40}) Merge pull request #(\d+)/);
  if (m) mergeOf.set(Number(m[2]), m[1]);
}
const titleOf = (sha) => git("log", "-1", "--format=%b", sha).split("\n")[0].trim();
const clip = (s) => (s.length > LIMIT ? s.slice(0, LIMIT) + "\n[... truncated]" : s);

const { results } = JSON.parse(readFileSync(szzPath, "utf8"));
const pairs = results.filter((r) => r.oldLines > 0 && r.blamed[0].pr !== 0 && r.blamed[0].n / r.oldLines >= Number(minShare));

const SYSTEM = `You audit candidate entries for a code-review evaluation set built with the SZZ method.
Each entry pairs a FIX pull request with the CULPRIT pull request that git blame says last wrote most of the lines the fix changed.
Judge from the diffs alone. Answer with one JSON object and nothing else:
{"is_defect_fix": bool, "introduced_by_culprit": bool, "visible_in_diff": bool, "defect": "<one sentence: what was wrong>", "reason": "<one or two sentences>"}
- is_defect_fix: the fix corrects wrong behaviour (a crash, a wrong result, a missed diagnostic, a hang), not a refactor, a feature, a rename or a doc change.
- introduced_by_culprit: the wrong behaviour the fix corrects was introduced by lines the culprit added or changed, not merely moved or reformatted there.
- visible_in_diff: a careful reviewer reading only the culprit's diff could have pointed at the defect.`;

async function judge(r) {
  const fixSha = mergeOf.get(r.pr);
  const culSha = mergeOf.get(r.blamed[0].pr);
  if (!fixSha || !culSha) return { error: "merge not found" };
  const fixFiles = git("diff", "--name-only", `${fixSha}^1`, fixSha).split("\n").filter(Boolean);
  const fixDiff = git("diff", "-w", `${fixSha}^1`, fixSha, "--", ...fixFiles.filter((f) => !f.endsWith(".md")));
  const culDiff = git("diff", "-w", `${culSha}^1`, culSha, "--", ...fixFiles);
  const user = `FIX PR #${r.pr}: ${r.title}\n\n<fix_diff>\n${clip(fixDiff)}\n</fix_diff>\n\nCULPRIT PR #${r.blamed[0].pr}: ${titleOf(culSha)}\n(diff restricted to the files the fix touched)\n\n<culprit_diff>\n${clip(culDiff)}\n</culprit_diff>`;
  const res = await fetch("https://api.anthropic.com/v1/messages", {
    method: "POST",
    headers: { "x-api-key": process.env.CLAUDE_API_KEY, "anthropic-version": "2023-06-01", "content-type": "application/json" },
    body: JSON.stringify({ model: MODEL, max_tokens: 2000, system: SYSTEM, messages: [{ role: "user", content: user }] }),
  });
  const body = await res.json();
  if (!res.ok) return { error: JSON.stringify(body).slice(0, 300) };
  const text = body.content.filter((b) => b.type === "text").map((b) => b.text).join("");
  try {
    return JSON.parse(text.slice(text.indexOf("{"), text.lastIndexOf("}") + 1));
  } catch {
    return { error: `unparsed: ${text.slice(0, 300)}` };
  }
}

const out = [];
const queue = [...pairs];
async function worker() {
  while (queue.length) {
    const r = queue.shift();
    const verdict = await judge(r);
    out.push({ fix: r.pr, culprit: r.blamed[0].pr, share: +(r.blamed[0].n / r.oldLines).toFixed(2), lines: r.oldLines, addsTest: r.addsTest, title: r.title, ...verdict });
    console.error(`${out.length}/${pairs.length} #${r.pr}`);
  }
}
await Promise.all(Array.from({ length: 4 }, worker));
out.sort((a, b) => a.fix - b.fix);
writeFileSync(outPath, JSON.stringify({ model: MODEL, min_share: Number(minShare), pairs: out }, null, 1));
