#!/usr/bin/env -S mise x -- node
// PreToolUse hook for the Bash tool: rewrite the command before it runs. One
// hook owns `updatedInput`, so no two rewrites of the same call race.

import { type Arg, type Command, commands, readStdin } from "./shell-commands.mts";

const MATCHERS = new Set(["pgrep", "pkill"]);
const VALUED_SHORT = new Set([..."dgGPstuUFOrq"]);
const VALUED_LONG = new Set([
  "--cgroup",
  "--delimiter",
  "--env",
  "--euid",
  "--group",
  "--ns",
  "--nslist",
  "--older",
  "--parent",
  "--pgroup",
  "--pidfile",
  "--queue",
  "--runstates",
  "--session",
  "--signal",
  "--terminal",
  "--uid",
]);
// pkill's `-9`, `-TERM`, `-USR1`.
const SIGNAL = /^-(\d+|[A-Z][A-Z0-9]+)$/;

/** The pattern of a match against full command lines, or null for any other. */
function fullPattern({ name, args }: Command): Arg | null {
  let full = false;
  let pattern: Arg | null = null;
  for (let i = 0; i < args.length; i++) {
    const { value } = args[i];
    if (value === "--") {
      pattern ??= args[i + 1] ?? null;
      break;
    }
    if (name === "pkill" && i === 0 && SIGNAL.test(value)) continue;
    if (value.startsWith("--")) {
      const option = value.split("=", 1)[0];
      if (option === "--full") full = true;
      else if (option === value && VALUED_LONG.has(option)) i++;
    } else if (value.startsWith("-") && value.length > 1) {
      for (let j = 1; j < value.length; j++) {
        if (value[j] === "f") full = true;
        if (VALUED_SHORT.has(value[j])) {
          if (j === value.length - 1) i++;
          break;
        }
      }
    } else {
      pattern ??= args[i];
    }
  }
  return full ? pattern : null;
}

/** Index past the bracket expression opening at `at`. */
function pastBracket(pattern: string, at: number): number {
  let i = at + 1;
  if (pattern[i] === "^") i++;
  if (pattern[i] === "]") i++;
  while (i < pattern.length) {
    if (pattern[i] === "[" && ":.=".includes(pattern[i + 1] ?? "]")) {
      const close = pattern.indexOf(`${pattern[i + 1]}]`, i + 2);
      i = close < 0 ? pattern.length : close + 2;
    } else if (pattern[i] === "]") {
      return i + 1;
    } else {
      i++;
    }
  }
  return i;
}

/** The regex with one letter bracketed, `mise run` as `[m]ise run`: it matches
 * what it matched, but no longer its own text. Null where no letter can be. */
export function bracketed(pattern: string): string | null {
  // The bracketed letter must be followed by a letter the match requires, so the
  // `]` that follows it in the rewritten text cannot continue a match.
  const letter = (at: number) => /^[A-Za-z0-9]$/.test(pattern[at] ?? "");
  let candidate = -1;
  let i = 0;
  while (i < pattern.length) {
    const c = pattern[i];
    if (c === "\\") {
      i += 2;
    } else if (c === "[") {
      i = pastBracket(pattern, i);
    } else if (c === "{") {
      const close = pattern.indexOf("}", i);
      i = close < 0 ? pattern.length : close + 1;
    } else if (c === "|") {
      return null;
    } else {
      if (candidate < 0 && letter(i) && letter(i + 1) && !/[?*{]/.test(pattern[i + 2] ?? "")) {
        candidate = i;
      }
      i++;
    }
  }
  if (candidate < 0) return null;
  return `${pattern.slice(0, candidate)}[${pattern[candidate]}]${pattern.slice(candidate + 1)}`;
}

const singleQuoted = (text: string) => `'${text.replaceAll("'", "'\\''")}'`;
const expands = (raw: string) => /[$`]/.test(raw.replace(/'[^']*'/g, ""));

/** `command` with `set -o pipefail` ahead, so `cmd | tail` reports cmd's failure
 * (issue #1083), and each `pgrep -f` / `pkill -f` pattern bracketed, so it stops
 * matching the shell whose command line carries it. */
export function rewritten(command: string): string {
  const edits: { start: number; end: number; text: string }[] = [];
  for (const invocation of commands(command)) {
    if (!MATCHERS.has(invocation.name)) continue;
    const pattern = fullPattern(invocation);
    if (pattern === null || pattern.start === null || expands(pattern.raw)) continue;
    const text = bracketed(pattern.value);
    if (text === null) continue;
    const end = pattern.start + pattern.raw.length;
    edits.push({ start: pattern.start, end, text: singleQuoted(text) });
  }
  let out = command;
  for (const { start, end, text } of edits.sort((a, b) => b.start - a.start)) {
    out = out.slice(0, start) + text + out.slice(end);
  }
  return `set -o pipefail; ${out}`;
}

if (import.meta.main) {
  const toolInput = JSON.parse(await readStdin())?.tool_input;
  if (toolInput?.command) {
    process.stdout.write(
      JSON.stringify({
        hookSpecificOutput: {
          hookEventName: "PreToolUse",
          updatedInput: { ...toolInput, command: rewritten(toolInput.command) },
        },
      }),
    );
  }
}
