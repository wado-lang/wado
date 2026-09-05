#!/usr/bin/env node
// Hands the findings of a /code-review run to the skill that answers them.
// Two events, because a skill reaches the session two ways: the user types
// `/code-review` (UserPromptSubmit), or the model calls it (PostToolUse > Skill).

const REMINDER =
  "The findings of this review are claims to answer, not a task list. Invoke the" +
  " `code-review-response` skill once the review reports: verify each finding, fix the class" +
  " it names, and report what was skipped and why.";

const SLASH = /^\s*\/code-review(\s|$)/;
const SKILL = /(^|:)code-review$/;

export function followUpContext(payload: unknown): string | null {
  const event = payload as {
    hook_event_name?: string;
    prompt?: string;
    tool_name?: string;
    tool_input?: { skill?: string };
  };
  switch (event?.hook_event_name) {
    case "UserPromptSubmit":
      return SLASH.test(event.prompt ?? "") ? REMINDER : null;
    case "PostToolUse":
      return event.tool_name === "Skill" && SKILL.test(event.tool_input?.skill ?? "")
        ? REMINDER
        : null;
    default:
      return null;
  }
}

if (import.meta.main) {
  let input = "";
  for await (const chunk of process.stdin) input += chunk;
  let payload: unknown;
  try {
    payload = JSON.parse(input);
  } catch {
    payload = null;
  }
  const context = followUpContext(payload);
  if (context) {
    const hookEventName = (payload as { hook_event_name: string }).hook_event_name;
    process.stdout.write(
      JSON.stringify({ hookSpecificOutput: { hookEventName, additionalContext: context } }),
    );
  }
}
