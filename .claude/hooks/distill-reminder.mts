#!/usr/bin/env node
// Asks for a distill pass before the turn that finished the work ends.
//
// Three events: the two ways `distill` reaches a session record that it ran
// (the user types `/distill`, or the model calls the skill), and Stop decides.
// A session is asked once — a nudge repeated every turn is noise, not guidance.

import { mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { execFileSync } from "node:child_process";

const REMINDER =
  "This branch has not been distilled. Run the `distill` skill over the whole branch" +
  " — reuse what exists, drop duplication, dead code and wasted work, turn invariants into" +
  " asserts, delete the comments the code already speaks — then commit what it changes.";

const SLASH = /^\s*\/distill(\s|$)/;
const SKILL = /(^|:)distill$/;

type Payload = {
  hook_event_name?: string;
  session_id?: string;
  stop_hook_active?: boolean;
  prompt?: string;
  tool_name?: string;
  tool_input?: { skill?: string };
};

export type State = "unset" | "asked" | "done";
export type Action = "record-done" | "ask" | "ignore";

export function decide(payload: Payload, state: State, hasWork: boolean): Action {
  switch (payload?.hook_event_name) {
    case "UserPromptSubmit":
      return SLASH.test(payload.prompt ?? "") ? "record-done" : "ignore";
    case "PostToolUse":
      return payload.tool_name === "Skill" && SKILL.test(payload.tool_input?.skill ?? "")
        ? "record-done"
        : "ignore";
    case "Stop":
      if (payload.stop_hook_active || state !== "unset" || !hasWork) return "ignore";
      return "ask";
    default:
      return "ignore";
  }
}

function statePath(sessionId: string): string {
  const gitDir = execFileSync("git", ["rev-parse", "--git-dir"], { encoding: "utf8" }).trim();
  const dir = join(gitDir, "wado-hooks");
  mkdirSync(dir, { recursive: true });
  return join(dir, `distill.${sessionId.replace(/[^\w.-]/g, "_")}`);
}

function readState(path: string): State {
  try {
    return readFileSync(path, "utf8").trim() as State;
  } catch {
    return "unset";
  }
}

// Work to distill: anything this branch changed against its merge base, or
// anything still in the worktree. No merge base — no branch to judge.
function hasWork(): boolean {
  const git = (args: string[]) => execFileSync("git", args, { encoding: "utf8" }).trim();
  try {
    const base = git(["merge-base", "origin/main", "HEAD"]);
    return git(["status", "--porcelain"]) !== "" || git(["diff", "--name-only", base]) !== "";
  } catch {
    return false;
  }
}

if (import.meta.main) {
  let input = "";
  for await (const chunk of process.stdin) input += chunk;
  let payload: Payload = {};
  try {
    payload = JSON.parse(input) ?? {};
  } catch {
    payload = {};
  }
  const sessionId = payload.session_id;
  if (!sessionId) process.exit(0);

  let path: string;
  try {
    path = statePath(sessionId);
  } catch {
    process.exit(0); // Not a repository: nothing to distill.
  }
  const isStop = payload.hook_event_name === "Stop";
  const action = decide(payload, readState(path), isStop && hasWork());
  if (action === "record-done") writeFileSync(path, "done");
  if (action === "ask") {
    writeFileSync(path, "asked");
    process.stderr.write(REMINDER);
    process.exit(2);
  }
}
