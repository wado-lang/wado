#!/usr/bin/env -S mise x -- node
// PreToolUse hook for `mcp__github__create_pull_request` and for Bash: holds a
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

/** The value `gh pr create` / `gh pr edit` takes for its title, "" when the
 * command sets none, or null when it is not one of those. */
function ghTitle({ name, args }: Command): string | null {
  const words = args.map((a) => a.value);
  if (name !== "gh" || words[0] !== "pr" || (words[1] !== "create" && words[1] !== "edit")) {
    return null;
  }
  for (let i = 2; i < words.length; i++) {
    const w = words[i];
    if (w === "--title" || w === "-t") return words[i + 1] ?? "";
    if (w.startsWith("--title=")) return w.slice("--title=".length);
    if (w.startsWith("-t") && !w.startsWith("--")) return w.slice(2);
  }
  return words[1] === "create" ? "" : null;
}

/** Every title the tool call would set. */
export function titles(input: string): string[] {
  const payload = JSON.parse(input);
  if (payload.tool_name === "Bash") {
    return commands(payloadCommand(input))
      .map(ghTitle)
      .filter((t): t is string => t !== null);
  }
  return [payload.tool_input?.title ?? ""];
}

export function decide(input: string, skill: string): Decision | null {
  const set = titles(input);
  if (set.length === 0) return null;
  const types = titleTypes(skill);
  if (types.length === 0) {
    return { allow: false, reason: `pr-conventions.mts parsed no title types from ${SKILL}; fix the hook or the skill's list.` };
  }
  const pattern = new RegExp(`^(${types.join("|")})(\\([^)]+\\))?!?: .+`);
  const bad = set.find((t) => !pattern.test(t));
  if (bad === undefined) return { allow: true, context: conventions(skill) };
  return {
    allow: false,
    reason: `PR title is not Conventional Commits: "${bad}". Pass it with --title.\n\n${conventions(skill)}`,
  };
}

if (import.meta.main) {
  const input = await readStdin();
  // A guard that throws must not let the pull request through.
  let decision: Decision | null;
  try {
    decision = decide(input, readFileSync(SKILL, "utf8"));
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
