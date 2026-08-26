#!/usr/bin/env node
// PreToolUse hook for the Bash tool: deny `sed`, `awk`, `python` and `python3`.
// The command is read the way a shell reads it — quoting, substitutions,
// heredocs, redirections — so a command word is caught wherever it runs.

const FORBIDDEN = /^(sed|awk|python(3(\.\d+)?)?)$/;

const REASON =
  "sed, awk, python and python3 are forbidden in this repository (AGENTS.md > Tooling)." +
  " Edit files with the editing tools, one call per change site; read with the Read tool" +
  " and search with the Grep tool; script in Node.js (node, or an executable .mts as in" +
  " .claude/hooks/). The ban is about the tool, not the care taken with it: exact-match" +
  " asserts do not save it. A sed -i or a python3 heredoc that rewrites a file keeps" +
  " matching where it was not aimed — a CLAUDE.md symlink replaced by a regular file, a" +
  " migration table rewritten into nonsense, .rs hit when only .wado was meant — and each" +
  " miss costs a diagnosis-and-revert cycle. For a rename too wide to do one call at a" +
  " time, agree on the approach first.";

// Words that pass their argument on to another command, so the command word is
// the first of their arguments that is neither a flag nor a duration.
const RUNNERS = new Set([
  "command",
  "env",
  "exec",
  "ionice",
  "nice",
  "nohup",
  "setsid",
  "stdbuf",
  "sudo",
  "time",
  "timeout",
  "watch",
  "xargs",
]);
const SHELLS = new Set(["bash", "dash", "ksh", "sh", "zsh"]);
const OPENS_COMMAND = new Set(["!", "do", "elif", "else", "if", "then", "until", "while"]);
const NOT_A_COMMAND = new Set([
  "case",
  "done",
  "esac",
  "fi",
  "for",
  "function",
  "in",
  "select",
]);
const EXEC_FLAGS = new Set(["-exec", "-execdir", "-ok", "-okdir"]);

const ASSIGNMENT = /^[A-Za-z_][A-Za-z0-9_]*(\[[^\]]*\])?\+?=/;
const DURATION = /^\d+(\.\d+)?[smhd]?$/;
const BREAKS_WORD = " \t\n\r;&|()<>";

type Word = { value: string; subs: string[]; end: number };

/** Text between `open` and its matching `close`, skipping quoted spans. */
function balanced(src: string, start: number, open: string, close: string): [string, number] {
  let depth = 1;
  let quote = "";
  let i = start;
  while (i < src.length) {
    const c = src[i];
    if (quote) {
      if (c === "\\" && quote === '"') i += 2;
      else i += c === quote ? ((quote = ""), 1) : 1;
      continue;
    }
    if (c === "\\") i += 2;
    else if (c === "'" || c === '"') (quote = c), i++;
    else if (c === open) depth++, i++;
    else if (c === close && --depth === 0) return [src.slice(start, i), i + 1];
    else i++;
  }
  return [src.slice(start), i];
}

function doubleQuoted(src: string, start: number, subs: string[]): [string, number] {
  let value = "";
  let i = start;
  while (i < src.length) {
    const c = src[i];
    if (c === '"') return [value, i + 1];
    if (c === "\\") {
      const next = src[i + 1] ?? "";
      value += '$`"\\\n'.includes(next) ? next : c + next;
      i += 2;
    } else if (c === "$" && src[i + 1] === "(") {
      const [inner, end] = balanced(src, i + 2, "(", ")");
      subs.push(inner);
      i = end;
    } else if (c === "`") {
      const close = src.indexOf("`", i + 1);
      subs.push(src.slice(i + 1, close < 0 ? undefined : close));
      i = close < 0 ? src.length : close + 1;
    } else {
      value += c;
      i++;
    }
  }
  return [value, i];
}

/** One word, resolved to the text the shell would run, plus the sources it nests. */
function readWord(src: string, start: number): Word {
  const subs: string[] = [];
  let value = "";
  let i = start;
  while (i < src.length) {
    const c = src[i];
    if (BREAKS_WORD.includes(c)) break;
    if (c === "\\") {
      value += src[i + 1] ?? "";
      i += 2;
    } else if (c === "'") {
      const close = src.indexOf("'", i + 1);
      value += src.slice(i + 1, close < 0 ? undefined : close);
      i = close < 0 ? src.length : close + 1;
    } else if (c === '"') {
      const [quoted, end] = doubleQuoted(src, i + 1, subs);
      value += quoted;
      i = end;
    } else if (c === "$" && src[i + 1] === "(") {
      const [inner, end] = balanced(src, i + 2, "(", ")");
      subs.push(inner);
      i = end;
    } else if (c === "`") {
      const close = src.indexOf("`", i + 1);
      subs.push(src.slice(i + 1, close < 0 ? undefined : close));
      i = close < 0 ? src.length : close + 1;
    } else if (c === "$" && src[i + 1] === "{") {
      const [inner, end] = balanced(src, i + 2, "{", "}");
      value += `\${${inner}}`;
      i = end;
    } else {
      value += c;
      i++;
    }
  }
  return { value, subs, end: i };
}

function skipHeredoc(src: string, start: number, delimiter: string): number {
  let i = start;
  while (i < src.length) {
    const newline = src.indexOf("\n", i);
    const line = src.slice(i, newline < 0 ? src.length : newline).trim();
    if (newline < 0) return src.length;
    i = newline + 1;
    if (line === delimiter) return i;
  }
  return i;
}

const skipBlanks = (src: string, i: number) => {
  while (src[i] === " " || src[i] === "\t") i++;
  return i;
};

const basename = (word: string) => word.slice(word.lastIndexOf("/") + 1);

/** Every command name the source would run, including nested sources. */
export function commandNames(src: string): string[] {
  const names: string[] = [];
  const nested: string[] = [];
  const heredocs: string[] = [];
  let atCommand = true;
  let afterRunner = false;
  let shellFlag = false;
  let previous = "";
  let i = 0;

  const startCommand = () => {
    atCommand = true;
    afterRunner = false;
    shellFlag = false;
  };

  while (i < src.length) {
    const c = src[i];
    if (c === " " || c === "\t") {
      i++;
    } else if (c === "\n" || c === "\r") {
      i++;
      while (heredocs.length > 0) i = skipHeredoc(src, i, heredocs.shift()!);
      startCommand();
    } else if (c === "#") {
      const newline = src.indexOf("\n", i);
      i = newline < 0 ? src.length : newline;
    } else if (c === ";" || c === "&" || c === "|" || c === "(" || c === "{") {
      i++;
      startCommand();
    } else if (c === ")" || c === "}") {
      i++;
    } else if (c === "<" || c === ">") {
      i = readRedirection(i);
    } else {
      const word = readWord(src, i);
      if (word.end === i) {
        i++;
        continue;
      }
      i = word.end;
      nested.push(...word.subs);
      const ioNumber = /^\d+$/.test(word.value) && (src[i] === "<" || src[i] === ">");
      if (!ioNumber) classify(word.value);
    }
  }

  for (const source of nested) names.push(...commandNames(source));
  return names;

  function readRedirection(at: number): number {
    if (src[at + 1] === "(") {
      const [inner, end] = balanced(src, at + 2, "(", ")");
      nested.push(inner);
      return end;
    }
    if (src.startsWith("<<", at) && !src.startsWith("<<<", at)) {
      const word = readWord(src, skipBlanks(src, at + (src[at + 2] === "-" ? 3 : 2)));
      heredocs.push(word.value);
      return word.end;
    }
    let i = at + (src.startsWith("<<<", at) ? 3 : 1);
    while (src[i] === ">" || src[i] === "&") i++;
    const target = readWord(src, skipBlanks(src, i));
    nested.push(...target.subs);
    return target.end;
  }

  function classify(value: string): void {
    if (atCommand || afterRunner) {
      if (ASSIGNMENT.test(value)) return;
      if (afterRunner && (value.startsWith("-") || DURATION.test(value))) return;
      const name = basename(value);
      if (OPENS_COMMAND.has(name)) return startCommand();
      if (NOT_A_COMMAND.has(name)) {
        atCommand = false;
        afterRunner = false;
        return;
      }
      names.push(name);
      previous = name;
      afterRunner = RUNNERS.has(name);
      atCommand = false;
    } else if (EXEC_FLAGS.has(value)) {
      startCommand();
    } else if (value === "-c" && SHELLS.has(previous)) {
      shellFlag = true;
    } else if (shellFlag) {
      nested.push(value);
      shellFlag = false;
    }
  }
}

export const forbiddenCommand = (command: string): boolean =>
  commandNames(command).some((name) => FORBIDDEN.test(name));

if (import.meta.main) {
  let input = "";
  for await (const chunk of process.stdin) input += chunk;
  let command = "";
  try {
    command = JSON.parse(input)?.tool_input?.command ?? "";
  } catch {
    command = "";
  }
  if (forbiddenCommand(command)) {
    process.stdout.write(
      JSON.stringify({
        hookSpecificOutput: {
          hookEventName: "PreToolUse",
          permissionDecision: "deny",
          permissionDecisionReason: REASON,
        },
      }),
    );
  }
}
