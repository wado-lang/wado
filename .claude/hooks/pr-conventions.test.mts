// mise run test-hooks
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";

import { decide, titles, titleTypes } from "./pr-conventions.mts";

const SKILL = readFileSync(new URL("../skills/pull-request/SKILL.md", import.meta.url), "utf8");

const create = (title: string) => JSON.stringify({ tool_name: "mcp__github__create_pull_request", tool_input: { title } });
const update = (fields: object) => JSON.stringify({ tool_name: "mcp__github__update_pull_request", tool_input: fields });
const bash = (command: string) => JSON.stringify({ tool_name: "Bash", tool_input: { command } });
const verdict = (input: string) => {
  const set = titles(input);
  return set.length === 0 ? null : decide(set, SKILL).allow;
};

test("the skill's title list parses", () => {
  assert.deepEqual(titleTypes(SKILL).sort(), ["chore", "docs", "feat", "fix", "perf", "refactor"]);
});

test("a bullet outside `## Title` is no type", () => {
  const skill = "## Title\n\n- `feat`\n\n## Description\n\n- `oops`: a bullet\n";
  assert.deepEqual(titleTypes(skill), ["feat"]);
});

test("a skill whose title list is gone denies", () => {
  assert.equal(decide([{ title: "feat: x" }], "## Titles\n\n- `feat`\n").allow, false);
});

for (const input of [
  create("feat: add x"),
  create("fix(lsp)!: drop y"),
  update({ pullNumber: 1, title: "docs: z" }),
  bash('gh pr create --title "docs: z" --body b'),
  bash('gh pr new --title "perf: z"'),
  bash('gh pr edit 12 --title "refactor: z"'),
  bash("gh pr create --body \"$(cat <<'EOF'\nit's done\nEOF\n)\" --title \"feat: x\""),
  bash('gh pr -R owner/repo create --title "fix: z"'),
  bash("gh pr create --title 'feat: costs $5'"),
]) {
  test(`allows ${input}`, () => assert.equal(verdict(input), true));
}

for (const input of [
  create("update stuff"),
  create("feature: x"),
  create("feat: x\nsecond line"),
  update({ pullNumber: 1, title: "wip" }),
  bash('gh pr create --title "update stuff"'),
  bash('gh pr new --title "bad"'),
  bash("gh pr create --fill"),
  bash('cd x && gh pr create --title "wip"'),
  bash('gh pr edit 12 --title "wip"'),
  bash('gh pr create --title "feat: x" --title "wip"'),
  bash('gh pr create -t "feat: x"'),
  bash('gh pr create -dt "feat: x"'),
  bash("gh pr create --title=feat:\\ x"),
  bash("gh pr create --title \"feat: x$(printf '\\nsecond')\""),
  bash("gh pr create --title \"feat: $T\""),
  bash('gh pr -R owner/repo create --title "wip"'),
  bash('gh pr --unknown create --title "feat: x"'),
  create("feat: x\ry"),
  create("feat: x\u2028y"),
]) {
  test(`denies ${input}`, () => assert.equal(verdict(input), false));
}

for (const input of [
  update({ pullNumber: 1, body: "b" }),
  bash("gh pr view 12"),
  bash("gh pr list --search create"),
  bash("gh pr edit 12 --body b"),
  bash("git log"),
]) {
  test(`ignores ${input}`, () => assert.equal(verdict(input), null));
}
