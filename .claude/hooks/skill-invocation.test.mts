// mise run test-hooks
import assert from "node:assert/strict";
import { test } from "node:test";

import { type HookPayload, invokesSkill } from "./skill-invocation.mts";

// A payload is parsed JSON, so these hand it fields the declared type forbids.
const parsed = (fields: object) => fields as HookPayload;
const typed = (prompt: unknown) => parsed({ hook_event_name: "UserPromptSubmit", prompt });
const called = (skill: unknown) =>
  parsed({ hook_event_name: "PostToolUse", tool_name: "Skill", tool_input: { skill } });

test("names the skill a payload invokes", () => {
  for (const payload of [typed("/distill"), typed("  /distill the branch"), called("distill")]) {
    assert.ok(invokesSkill(payload, "distill"));
  }
});

test("a plugin prefix is not part of the name", () => {
  assert.ok(invokesSkill(called("wado:distill"), "distill"));
});

test("a typed name ends at its word", () => {
  for (const prompt of ["/distill-something", "/distill/foo", "/distill?x", "/distill.md"]) {
    assert.equal(invokesSkill(typed(prompt), "distill"), false, prompt);
  }
});

test("a field of the wrong type names no skill", () => {
  // An array is the coercion trap: `String(["/distill"])` is `"/distill"`.
  for (const payload of [
    called(7),
    called(null),
    called({ name: "distill" }),
    called(["distill"]),
    typed(7),
    typed(["/distill"]),
  ]) {
    assert.equal(invokesSkill(payload, "distill"), false);
  }
});

test("only the two invocation events count", () => {
  for (const event of ["PreToolUse", "Stop", "SessionStart"]) {
    assert.equal(invokesSkill({ hook_event_name: event, prompt: "/distill" }, "distill"), false);
  }
  assert.equal(invokesSkill(null, "distill"), false);
});
