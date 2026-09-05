// mise run test-hooks
import assert from "node:assert/strict";
import { test } from "node:test";

import { followUpContext } from "./code-review-followup.mts";

// What names a skill is skill-invocation's own test; this one is the reminder.
const prompt = (text: string) => ({ hook_event_name: "UserPromptSubmit", prompt: text });

test("a /code-review run asks for the response skill", () => {
  assert.match(followUpContext(prompt("/code-review --fix high")) ?? "", /code-review-response/);
});

test("anything else is left alone", () => {
  for (const payload of [prompt("/code-review-response"), prompt("review the diff"), {}, null]) {
    assert.equal(followUpContext(payload), null);
  }
});
