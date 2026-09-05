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

const TYPED = /^\s*\/([\w:.-]+)/;
const bare = (skill: string) => skill.replace(/^.*:/, "");

export function invokesSkill(payload: HookPayload | null, skill: string): boolean {
  switch (payload?.hook_event_name) {
    case "UserPromptSubmit": {
      const typed = TYPED.exec(payload.prompt ?? "")?.[1];
      return typed !== undefined && bare(typed) === skill;
    }
    case "PostToolUse":
      return payload.tool_name === "Skill" && bare(payload.tool_input?.skill ?? "") === skill;
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
