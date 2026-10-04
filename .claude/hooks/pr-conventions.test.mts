// mise run test-hooks
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";

import { decide, titleTypes } from "./pr-conventions.mts";

const SKILL = readFileSync(new URL("../skills/pull-request/SKILL.md", import.meta.url), "utf8");

const mcp = (title: string) => JSON.stringify({ tool_name: "mcp__github__create_pull_request", tool_input: { title } });
const bash = (command: string) => JSON.stringify({ tool_name: "Bash", tool_input: { command } });

test("the skill's title list parses", () => {
  assert.deepEqual(titleTypes(SKILL).sort(), ["chore", "docs", "feat", "fix", "perf", "refactor"]);
});

test("a bullet outside `## Title` is no type", () => {
  const skill = "## Title\n\n- `feat`\n\n## Description\n\n- `oops`: a bullet\n";
  assert.deepEqual(titleTypes(skill), ["feat"]);
});

test("a skill whose title list is gone denies", () => {
  assert.equal(decide(mcp("feat: x"), "## Titles\n\n- `feat`\n")?.allow, false);
});

for (const input of [
  mcp("feat: add x"),
  mcp("fix(lsp)!: drop y"),
  bash('gh pr create --title "docs: z" --body b'),
  bash("gh pr create --title=chore:\\ z"),
  bash('gh pr create -t "perf: z"'),
  bash('gh pr edit 12 --title "refactor: z"'),
]) {
  test(`allows ${input}`, () => assert.equal(decide(input, SKILL)?.allow, true));
}

for (const input of [
  mcp("update stuff"),
  mcp("feature: x"),
  bash('gh pr create --title "update stuff"'),
  bash("gh pr create --fill"),
  bash('cd x && gh pr create -t "wip"'),
  bash('gh pr edit 12 --title "wip"'),
]) {
  test(`denies ${input}`, () => assert.equal(decide(input, SKILL)?.allow, false));
}

for (const input of [bash("gh pr view 12"), bash("gh pr edit 12 --body b"), bash("git log")]) {
  test(`ignores ${input}`, () => assert.equal(decide(input, SKILL), null));
}
