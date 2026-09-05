#!/usr/bin/env node
// Hands the findings of a /code-review run to the skill that answers them.

import { type HookPayload, invokesSkill, readPayload } from "./skill-invocation.mts";

const REMINDER =
  "The findings of this review are claims to answer, not a task list. Invoke the" +
  " `code-review-response` skill once the review reports: verify each finding, fix the class" +
  " it names, and report what was skipped and why.";

export function followUpContext(payload: HookPayload | null): string | null {
  return invokesSkill(payload, "code-review") ? REMINDER : null;
}

if (import.meta.main) {
  const payload = await readPayload();
  const context = followUpContext(payload);
  if (context) {
    process.stdout.write(
      JSON.stringify({
        hookSpecificOutput: {
          hookEventName: payload.hook_event_name,
          additionalContext: context,
        },
      }),
    );
  }
}
