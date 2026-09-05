#!/usr/bin/env node
// Asks for a distill pass before the turn that finished the work ends.
// A session is asked once: a nudge repeated every turn is noise, not guidance.

import { execFileSync } from "node:child_process";
import { mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";

import { type HookPayload, invokesSkill, readPayload } from "./skill-invocation.mts";

const REMINDER =
  "This branch has not been distilled. Run the `distill` skill over the whole branch" +
  " — reuse what exists, drop duplication, dead code and wasted work, turn invariants into" +
  " asserts, delete the comments the code already speaks — then commit what it changes.";

export type State = "unset" | "asked" | "done";
export type Action = "record-done" | "ask" | "ignore";

export function decide(payload: HookPayload, state: State, hasWork: boolean): Action {
  if (invokesSkill(payload, "distill")) return "record-done";
  if (payload?.hook_event_name !== "Stop") return "ignore";
  if (payload.stop_hook_active || state !== "unset" || !hasWork) return "ignore";
  return "ask";
}

const git = (...args: string[]) => execFileSync("git", args, { encoding: "utf8" }).trim();

function statePath(sessionId: string): string {
  const dir = join(git("rev-parse", "--git-dir"), "wado-hooks");
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

// Anything the branch changed against its merge base, or anything still in the
// worktree. Without a merge base there is no branch to judge.
function hasWork(): boolean {
  try {
    const base = git("merge-base", "origin/main", "HEAD");
    return git("status", "--porcelain") !== "" || git("diff", "--name-only", base) !== "";
  } catch {
    return false;
  }
}

if (import.meta.main) {
  const payload = await readPayload();
  if (!payload.session_id) process.exit(0);

  let path: string;
  try {
    path = statePath(payload.session_id);
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
