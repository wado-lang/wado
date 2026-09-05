// mise run test-hooks
import assert from "node:assert/strict";
import { test } from "node:test";

import { decide } from "./distill-reminder.mts";

// What names a skill is skill-invocation's own test; this one is the decision.
const stop = (extra: object = {}) => ({ hook_event_name: "Stop", ...extra });
const work = () => true;
const noWork = () => false;

test("a distill run is recorded, whichever way it was invoked", () => {
  for (const payload of [
    { hook_event_name: "UserPromptSubmit", prompt: "/distill the branch" },
    { hook_event_name: "PostToolUse", tool_name: "Skill", tool_input: { skill: "distill" } },
  ]) {
    assert.equal(decide(payload, "unset", work), "record-done");
  }
});

test("a turn that changed the branch is asked once", () => {
  assert.equal(decide(stop(), "unset", work), "ask");
  assert.equal(decide(stop(), "asked", work), "ignore");
  assert.equal(decide(stop(), "done", work), "ignore");
});

test("nothing to distill, nothing to say", () => {
  assert.equal(decide(stop(), "unset", noWork), "ignore");
});

test("a stop the hook itself caused never blocks again", () => {
  assert.equal(decide(stop({ stop_hook_active: true }), "unset", work), "ignore");
});

test("anything else is left alone", () => {
  for (const payload of [
    { hook_event_name: "UserPromptSubmit", prompt: "explain distill" },
    { hook_event_name: "SessionEnd" },
    {},
  ]) {
    assert.equal(decide(payload, "unset", work), "ignore");
  }
});
