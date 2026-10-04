#!/usr/bin/env -S mise x -- node
// PreToolUse hook for the GitHub MCP pull request tools and for Bash: holds a
// pull request title to Conventional Commits wherever it is set, whether or not
// the model invoked the pull-request skill. The allowed types and the reminder
// are read from that skill, so the hook never drifts from it. A valid title
// passes with the skill body as a reminder; anything else is denied, including
// an input or a skill this hook cannot read.

import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

import { type Command, commands, denial, payloadCommand, readStdin } from "./shell-commands.mts";

const SKILL = join(dirname(fileURLToPath(import.meta.url)), "../skills/pull-request/SKILL.md");

type Decision = { allow: true; context: string } | { allow: false; reason: string };

/** The skill body, frontmatter stripped. */
export function conventions(skill: string): string {
  return skill.replace(/^---\n[\s\S]*?\n---\n/, "").trim();
}

/** The `- \`type\`` bullets of the skill's `## Title` section only, so a bullet
 * elsewhere never becomes a type. */
export function titleTypes(skill: string): string[] {
  const section = skill.split(/^## /m).find((s) => s.startsWith("Title\n")) ?? "";
  return [...section.matchAll(/^- `([a-z]+)!?`/gm)].map((m) => m[1]);
}

/** A title the call sets, or why the hook cannot tell which one it sets. */
type Title = { title: string } | { problem: string };

const ONE_TITLE = "Spell the title as one `--title <value>`, the only form this hook reads.";

/** What `gh pr create|new|edit` sets as its title, or null when it sets none.
 * gh's flag grammar (short clusters, `=` forms, the last repeat winning) is not
 * modelled: any spelling but one `--title <value>` is refused instead. */
function ghTitle({ name, args }: Command): Title | null {
  const words = args.map((a) => a.value);
  const sub = words[1];
  if (name !== "gh" || words[0] !== "pr" || !["create", "new", "edit"].includes(sub)) return null;
  const rest = words.slice(2);
  const other = rest.some((w) => w.startsWith("--title=") || /^-[A-Za-z]*t/.test(w));
  const at = rest.flatMap((w, i) => (w === "--title" ? [i] : []));
  if (other || at.length > 1) return { problem: ONE_TITLE };
  if (at.length === 1) return { title: rest[at[0] + 1] ?? "" };
  return sub === "edit" ? null : { problem: `\`gh pr ${sub}\` without --title. ${ONE_TITLE}` };
}

/** Every title the tool call would set. */
export function titles(input: string): Title[] {
  const payload = JSON.parse(input);
  if (payload.tool_name === "Bash") {
    return commands(payloadCommand(input))
      .map(ghTitle)
      .filter((t): t is Title => t !== null);
  }
  const fields = payload.tool_input ?? {};
  // An update that leaves the title alone sets none.
  if (payload.tool_name === "mcp__github__update_pull_request" && !("title" in fields)) return [];
  return [{ title: fields.title ?? "" }];
}

export function decide(set: Title[], skill: string): Decision {
  const types = titleTypes(skill);
  if (types.length === 0) {
    return { allow: false, reason: `pr-conventions.mts parsed no title types from ${SKILL}; fix the hook or the skill's list.` };
  }
  const pattern = new RegExp(`^(${types.join("|")})(\\([^)]+\\))?!?: [^\\n]+$`);
  for (const t of set) {
    if ("problem" in t) return { allow: false, reason: `${t.problem}\n\n${conventions(skill)}` };
    if (!pattern.test(t.title)) {
      return {
        allow: false,
        reason: `PR title is not a one-line Conventional Commit: "${t.title}".\n\n${conventions(skill)}`,
      };
    }
  }
  return { allow: true, context: conventions(skill) };
}

if (import.meta.main) {
  const input = await readStdin();
  // A guard that throws must not let the pull request through. The skill is
  // read only for a call that sets a title, so a missing one blocks no other.
  let decision: Decision | null;
  try {
    const set = titles(input);
    decision = set.length === 0 ? null : decide(set, readFileSync(SKILL, "utf8"));
  } catch (error) {
    decision = { allow: false, reason: `pr-conventions.mts failed (${(error as Error).message}), so the call is denied.` };
  }
  if (decision?.allow) {
    process.stdout.write(
      JSON.stringify({ hookSpecificOutput: { hookEventName: "PreToolUse", additionalContext: decision.context } }),
    );
  } else if (decision) {
    process.stdout.write(denial(decision.reason));
  }
}
