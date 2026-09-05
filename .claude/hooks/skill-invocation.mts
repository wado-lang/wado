// A skill reaches a session two ways: the user types `/name` (UserPromptSubmit),
// or the model calls it (PostToolUse > Skill). Both name the skill here.

export type HookPayload = {
  hook_event_name?: string;
  session_id?: string;
  stop_hook_active?: boolean;
  prompt?: string;
  tool_name?: string;
  tool_input?: { skill?: string };
};

// A typed name runs to the end of its word, so `/distill/foo` names no skill.
const TYPED = /^\s*\/([\w:.-]+)(?=\s|$)/;

// The payload is parsed JSON, so a declared field is a claim about its shape.
// Anything but a string names no skill, and a plugin prefix is not part of one.
const named = (value: unknown): string | null =>
  typeof value === "string" ? value.replace(/^.*:/, "") : null;

export function invokesSkill(payload: HookPayload | null, skill: string): boolean {
  switch (payload?.hook_event_name) {
    case "UserPromptSubmit":
      return (
        typeof payload.prompt === "string" && named(TYPED.exec(payload.prompt)?.[1]) === skill
      );
    case "PostToolUse":
      return payload.tool_name === "Skill" && named(payload.tool_input?.skill) === skill;
    default:
      return false;
  }
}

export async function readPayload(): Promise<HookPayload> {
  let input = "";
  for await (const chunk of process.stdin) input += chunk;
  try {
    return JSON.parse(input) ?? {};
  } catch {
    return {};
  }
}
