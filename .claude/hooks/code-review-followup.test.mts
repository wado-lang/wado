// mise run test-hooks
import assert from "node:assert/strict";
import { test } from "node:test";

import { followUpContext } from "./code-review-followup.mts";

const prompt = (text: string) => ({ hook_event_name: "UserPromptSubmit", prompt: text });
const skill = (name: string) => ({
  hook_event_name: "PostToolUse",
  tool_name: "Skill",
  tool_input: { skill: name },
});

test("a /code-review run asks for the response skill", () => {
  for (const payload of [
    prompt("/code-review"),
    prompt("  /code-review --fix high"),
    skill("code-review"),
    skill("wado:code-review"),
  ]) {
    assert.match(followUpContext(payload) ?? "", /code-review-response/);
  }
});

test("anything else is left alone", () => {
  for (const payload of [
    prompt("/code-review-response"),
    prompt("review the diff"),
    prompt("run /distill"),
    skill("code-review-response"),
    skill("distill"),
    { hook_event_name: "PostToolUse", tool_name: "Bash", tool_input: { command: "/code-review" } },
    { hook_event_name: "PreToolUse", tool_name: "Skill", tool_input: { skill: "code-review" } },
    {},
    null,
  ]) {
    assert.equal(followUpContext(payload), null);
  }
});
