#!/usr/bin/env node
// PreToolUse hook for the Bash tool: deny the command words below, read the way
// a shell reads them, so one is caught wherever it runs.

const FORBIDDEN = [
  {
    pattern: /^(sed|awk|python(3(\.\d+)?)?)$/,
    reason:
      "sed, awk, python and python3 are forbidden (AGENTS.md > Tooling): a rewrite keeps" +
      " matching where it was not aimed. Edit with the editing tools, one call per change" +
      " site; script in Node.js.",
  },
  {
    pattern: /\$/,
    reason:
      "a command word that is only known once the shell expands it — $var, ${var}, $(…) —" +
      " cannot be read, so it is denied. Name the command itself.",
  },
  {
    pattern: /^nohup$/,
    reason:
      "nohup is forbidden (AGENTS.md > Tooling): it notifies nobody when the job exits. Run" +
      " a long job through the harness's background mechanism.",
  },
];

// Words that pass their argument on to another command: the command word is the
// first argument that is neither a flag, a listed flag's value, nor a duration.
const RUNNERS = new Map([
  ["command", ["-v", "-V"]],
  ["env", ["-u", "--unset", "-C", "--chdir"]],
  ["exec", ["-a"]],
  ["ionice", ["-c", "-n", "-p", "-P", "-u"]],
  ["nice", ["-n", "--adjustment"]],
  ["nohup", []],
  ["setsid", []],
  ["stdbuf", ["-e", "-i", "-o", "--error", "--input", "--output"]],
  ["sudo", ["-C", "-D", "-g", "-p", "-R", "-r", "-t", "-U", "-u", "--chdir", "--group", "--user"]],
  ["time", ["-f", "--format", "-o", "--output"]],
  ["timeout", ["-k", "--kill-after", "-s", "--signal"]],
  ["watch", ["-d", "-n", "--interval"]],
  ["xargs", ["-a", "-d", "-E", "-I", "-i", "-L", "-l", "-n", "-P", "-s", "--arg-file", "--delimiter"]],
]);
const SHELLS = new Set(["bash", "dash", "ksh", "sh", "zsh"]);
// `bash -c`, `sh -ec`, `zsh -lic`: the script follows when `c` ends the flag.
const SHELL_FLAG = /^-[A-Za-z]*c$/;
// `env -S 'cmd'`, `env -S'cmd'`, `env --split-string=cmd`: a command line of its own.
const ENV_SPLIT = /^(?:-S|--split-string)=?([\s\S]*)$/;
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
type Heredoc = { delimiter: string; stripsTabs: boolean; expands: boolean; script: boolean };

/** Text between `open` and its matching `close`, skipping quoted spans. */
function balanced(src: string, start: number, open: string, close: string): [string, number] {
  let depth = 1;
  let quote = "";
  let i = start;
  while (i < src.length) {
    const c = src[i];
    if (c === "\\" && quote !== "'") {
      i += 2;
    } else if (quote) {
      if (c === quote) quote = "";
      i++;
    } else if (c === "'" || c === '"') {
      quote = c;
      i++;
    } else {
      if (c === open) depth++;
      else if (c === close && --depth === 0) return [src.slice(start, i), i + 1];
      i++;
    }
  }
  return [src.slice(start), i];
}

const ANSI_C_ESCAPES = new Map([
  ["a", "\x07"],
  ["b", "\b"],
  ["e", "\x1b"],
  ["E", "\x1b"],
  ["f", "\f"],
  ["n", "\n"],
  ["r", "\r"],
  ["t", "\t"],
  ["v", "\v"],
  ["\\", "\\"],
  ["'", "'"],
  ['"', '"'],
  ["?", "?"],
]);

/** The text `$'…'` expands to, and the index past its close. */
function ansiCQuoted(src: string, start: number): [string, number] {
  let value = "";
  let i = start;
  while (i < src.length && src[i] !== "'") {
    if (src[i] !== "\\") {
      value += src[i++];
      continue;
    }
    const escape = src[i + 1] ?? "";
    const named = ANSI_C_ESCAPES.get(escape);
    const numeric = /^(x[0-9a-fA-F]{1,2}|u[0-9a-fA-F]{1,4}|U[0-9a-fA-F]{1,8}|[0-7]{1,3})/.exec(
      src.slice(i + 1),
    );
    if (named !== undefined) {
      value += named;
      i += 2;
    } else if (numeric) {
      const digits = numeric[0];
      const radix = /^[xuU]/.test(digits) ? 16 : 8;
      const code = parseInt(radix === 16 ? digits.slice(1) : digits, radix);
      value += code <= 0x10ffff ? String.fromCodePoint(code) : src.slice(i, i + 1 + digits.length);
      i += 1 + digits.length;
    } else {
      value += src.slice(i, i + 2);
      i += 2;
    }
  }
  return [value, i < src.length ? i + 1 : i];
}

/** Text up to the next `close`, and the index past it. */
function delimited(src: string, start: number, close: string): [string, number] {
  const end = src.indexOf(close, start);
  return end < 0 ? [src.slice(start), src.length] : [src.slice(start, end), end + 1];
}

/** Index past the `$(…)` or `` `…` `` substitution at `at`, or -1 if none is there. */
function readSubstitution(src: string, at: number, subs: string[]): number {
  if (src[at] === "$" && src[at + 1] === "(") {
    const [inner, end] = balanced(src, at + 2, "(", ")");
    subs.push(inner);
    return end;
  }
  if (src[at] === "`") {
    const [inner, end] = delimited(src, at + 1, "`");
    subs.push(inner);
    return end;
  }
  return -1;
}

function doubleQuoted(src: string, start: number, subs: string[]): [string, number] {
  let value = "";
  let i = start;
  while (i < src.length) {
    const c = src[i];
    if (c === '"') return [value, i + 1];
    const substitution = readSubstitution(src, i, subs);
    if (substitution >= 0) {
      i = substitution;
    } else if (c === "\\") {
      const next = src[i + 1] ?? "";
      if (next !== "\n") value += '$`"\\'.includes(next) ? next : c + next;
      i += 2;
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
    const substitution = readSubstitution(src, i, subs);
    if (substitution >= 0) {
      i = substitution;
    } else if (c === "\\") {
      if (src[i + 1] !== "\n") value += src[i + 1] ?? "";
      i += 2;
    } else if (c === "$" && src[i + 1] === "'") {
      const [quoted, end] = ansiCQuoted(src, i + 2);
      value += quoted;
      i = end;
    } else if (c === "$" && src[i + 1] === '"') {
      i++; // a locale-translated string runs as its contents
    } else if (c === "'") {
      const [quoted, end] = delimited(src, i + 1, "'");
      value += quoted;
      i = end;
    } else if (c === '"') {
      const [quoted, end] = doubleQuoted(src, i + 1, subs);
      value += quoted;
      i = end;
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

/** The heredoc body, and the index past its delimiter line. */
function readHeredoc(src: string, start: number, heredoc: Heredoc): [string, number] {
  // The line must be the delimiter itself; `<<-` allows leading tabs before it.
  const closes = (line: string) =>
    (heredoc.stripsTabs ? line.replace(/^\t+/, "") : line) === heredoc.delimiter;
  let i = start;
  while (i < src.length) {
    const newline = src.indexOf("\n", i);
    if (newline < 0) {
      return [src.slice(start, closes(src.slice(i)) ? i : src.length), src.length];
    }
    const lineStart = i;
    const line = src.slice(i, newline);
    i = newline + 1;
    if (closes(line)) return [src.slice(start, lineStart), i];
  }
  return [src.slice(start), i];
}

/** The substitutions an unquoted heredoc body expands, which the shell runs. */
function expansions(body: string): string[] {
  const subs: string[] = [];
  let i = 0;
  while (i < body.length) {
    if (body[i] === "\\") {
      i += 2;
      continue;
    }
    const end = readSubstitution(body, i, subs);
    i = end < 0 ? i + 1 : end;
  }
  return subs;
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
  const heredocs: Heredoc[] = [];
  let atCommand = true;
  let runner = "";
  let skipValue = false;
  let nestNext = false;
  let previous = "";
  let i = 0;

  const startCommand = () => {
    atCommand = true;
    runner = "";
    skipValue = false;
    nestNext = false;
  };

  while (i < src.length) {
    const c = src[i];
    if (c === " " || c === "\t") {
      i++;
    } else if (c === "\n" || c === "\r") {
      i++;
      while (heredocs.length > 0) {
        const heredoc = heredocs.shift()!;
        const [body, end] = readHeredoc(src, i, heredoc);
        if (heredoc.script) nested.push(body);
        else if (heredoc.expands) nested.push(...expansions(body));
        i = end;
      }
      startCommand();
    } else if (c === "#") {
      const newline = src.indexOf("\n", i);
      i = newline < 0 ? src.length : newline;
    } else if (c === ";" || c === "&" || c === "|" || c === "(" || c === ")" || isGroup(i)) {
      i++;
      startCommand();
    } else if (c === "}") {
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
      // A word that is only a substitution runs whatever the substitution prints.
      if (word.value === "") {
        if (word.subs.length > 0) classify("$()");
      } else if (!ioNumber) {
        classify(word.value);
      }
    }
  }

  for (const source of nested) names.push(...commandNames(source));
  return names;

  /** `{` opens a group only as a word of its own; `{}` is find's placeholder. */
  function isGroup(at: number): boolean {
    return src[at] === "{" && (at + 1 === src.length || " \t\n\r".includes(src[at + 1]));
  }

  function readRedirection(at: number): number {
    if (src[at + 1] === "(") {
      const [inner, end] = balanced(src, at + 2, "(", ")");
      nested.push(inner);
      return end;
    }
    if (src.startsWith("<<", at) && !src.startsWith("<<<", at)) {
      const stripsTabs = src[at + 2] === "-";
      const start = skipBlanks(src, at + (stripsTabs ? 3 : 2));
      const word = readWord(src, start);
      // A quoted delimiter turns the body into data; an unquoted one expands.
      // Fed to a shell, the body is the script it runs.
      const expands = !/['"\\]/.test(src.slice(start, word.end));
      heredocs.push({
        delimiter: word.value,
        stripsTabs,
        expands,
        script: SHELLS.has(previous),
      });
      return word.end;
    }
    let i = at + (src.startsWith("<<<", at) ? 3 : 1);
    while (src[i] === ">" || src[i] === "&") i++;
    const target = readWord(src, skipBlanks(src, i));
    nested.push(...target.subs);
    return target.end;
  }

  function classify(value: string): void {
    if (nestNext) {
      nested.push(value);
      nestNext = false;
      // What that payload starts is what a following redirection feeds.
      previous = basename(readWord(value, skipBlanks(value, 0)).value);
      return;
    }
    if (atCommand || runner) {
      if (skipValue) {
        skipValue = false;
        return;
      }
      if (ASSIGNMENT.test(value)) return;
      if (runner) {
        if (value.startsWith("-")) {
          const split = runner === "env" ? ENV_SPLIT.exec(value) : null;
          if (split) {
            if (split[1] === "") nestNext = true;
            else nested.push(split[1]);
          } else {
            skipValue = RUNNERS.get(runner)!.includes(value);
          }
          return;
        }
        if (DURATION.test(value)) return;
      }
      const name = basename(value);
      if (OPENS_COMMAND.has(name)) return startCommand();
      if (NOT_A_COMMAND.has(name)) {
        atCommand = false;
        runner = "";
        return;
      }
      names.push(name);
      previous = name;
      runner = RUNNERS.has(name) ? name : "";
      atCommand = false;
    } else if (EXEC_FLAGS.has(value)) {
      startCommand();
    } else if (SHELL_FLAG.test(value) && SHELLS.has(previous)) {
      nestNext = true;
    } else if (previous === "eval") {
      nested.push(value);
    }
  }
}

/** Why the command is denied, or null when it runs nothing forbidden. */
export function denialReason(command: string): string | null {
  for (const name of commandNames(command)) {
    const ban = FORBIDDEN.find(({ pattern }) => pattern.test(name));
    if (ban) return ban.reason;
  }
  return null;
}

/** The Bash command in the hook's stdin payload, or "" when it carries none. */
function payloadCommand(input: string): string {
  try {
    return JSON.parse(input)?.tool_input?.command ?? "";
  } catch {
    return "";
  }
}

if (import.meta.main) {
  let input = "";
  for await (const chunk of process.stdin) input += chunk;
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
