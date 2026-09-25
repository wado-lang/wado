#!/usr/bin/env -S mise x -- node
// PreToolUse hook for the Bash tool: deny the command words below, read the way
// a shell reads them, so one is caught wherever it runs.

import { commandNames, payloadCommand, readStdin } from "./shell-commands.mts";

const FORBIDDEN = [
  {
    pattern: /^(sed|awk|perl|python(3(\.\d+)?)?)$/,
    reason:
      "sed, awk, perl, python and python3 are forbidden (AGENTS.md > Tooling): a rewrite keeps" +
      " matching where it was not aimed. Edit with the editing tools, one call per change" +
      " site; script in Node.js.",
  },
  {
    pattern: /^(nohup|setsid|disown)$/,
    reason:
      "nohup, setsid and disown are forbidden (AGENTS.md > Tooling): a detached job notifies" +
      " nobody when it exits. Run a long job through the harness's background mechanism.",
  },
];

/** Why the command is denied, or null when it runs nothing forbidden. */
export function denialReason(command: string): string | null {
  for (const name of commandNames(command)) {
    const ban = FORBIDDEN.find(({ pattern }) => pattern.test(name));
    if (ban) return ban.reason;
  }
  return null;
}

if (import.meta.main) {
  const input = await readStdin();
  // A guard that throws must not let the command through.
  let reason: string | null;
  try {
    reason = denialReason(payloadCommand(input));
  } catch (error) {
    reason = `this command could not be read (${(error as Error).message}), so it is denied. Rephrase it, or report the input if it is an ordinary one.`;
  }
  if (reason) {
    process.stdout.write(
      JSON.stringify({
        hookSpecificOutput: {
          hookEventName: "PreToolUse",
          permissionDecision: "deny",
          permissionDecisionReason: reason,
        },
      }),
    );
  }
}
