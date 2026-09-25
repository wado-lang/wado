// The commands a Bash tool call would run, read the way a shell reads them, for
// the PreToolUse hooks that deny or rewrite one.

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

/** Nested shell source, and where it sits in the top-level command; null where
 * the text is not a span of it (a `bash -c` payload, `eval`, a heredoc). */
type Source = { text: string; offset: number | null };
type Word = { value: string; subs: Source[]; end: number };
/** An argument word: the text it resolves to, and the source that spells it. */
export type Arg = { value: string; raw: string; start: number | null };
export type Command = { name: string; args: Arg[] };
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

/** Text up to the next `close`, and the index past it. */
function delimited(src: string, start: number, close: string): [string, number] {
  const end = src.indexOf(close, start);
  return end < 0 ? [src.slice(start), src.length] : [src.slice(start, end), end + 1];
}

/** Index past the `$(…)` or `` `…` `` substitution at `at`, or -1 if none is there. */
function readSubstitution(src: string, at: number, subs: Source[]): number {
  if (src[at] === "$" && src[at + 1] === "(") {
    const [text, end] = balanced(src, at + 2, "(", ")");
    subs.push({ text, offset: at + 2 });
    return end;
  }
  if (src[at] === "`") {
    const [text, end] = delimited(src, at + 1, "`");
    subs.push({ text, offset: at + 1 });
    return end;
  }
  return -1;
}

function doubleQuoted(src: string, start: number, subs: Source[]): [string, number] {
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
  const subs: Source[] = [];
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
    } else if (c === "$" && (src[i + 1] === "'" || src[i + 1] === '"')) {
      i++; // `$'…'` and `$"…"` run as their contents
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
function expansions(body: string): Source[] {
  const subs: Source[] = [];
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

const unplaced = (text: string): Source => ({ text, offset: null });

/** Every command name the source would run, including nested sources. */
export function commandNames(src: string): string[] {
  return commands(src).map(({ name }) => name);
}

/** Every command the source would run, including nested sources. `base` is where
 * `src` sits in the top-level command, placing each argument's `start`. */
export function commands(src: string, base: number | null = 0): Command[] {
  const found: Command[] = [];
  const nested: Source[] = [];
  const heredocs: Heredoc[] = [];
  let current: Command | null = null;
  let atCommand = true;
  let runner = "";
  let skipValue = false;
  let nestNext = false;
  let previous = "";
  let i = 0;

  const startCommand = () => {
    current = null;
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
        if (heredoc.script) nested.push(unplaced(body));
        else if (heredoc.expands) nested.push(...expansions(body).map(({ text }) => unplaced(text)));
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
      const start = i;
      const word = readWord(src, start);
      if (word.end === start) {
        i++;
        continue;
      }
      i = word.end;
      nested.push(...word.subs);
      const ioNumber = /^\d+$/.test(word.value) && (src[i] === "<" || src[i] === ">");
      if (word.value !== "" && !ioNumber) {
        classify({
          value: word.value,
          raw: src.slice(start, word.end),
          start: base === null ? null : base + start,
        });
      }
    }
  }

  for (const { text, offset } of nested) {
    found.push(...commands(text, base === null || offset === null ? null : base + offset));
  }
  return found;

  /** `{` opens a group only as a word of its own; `{}` is find's placeholder. */
  function isGroup(at: number): boolean {
    return src[at] === "{" && (at + 1 === src.length || " \t\n\r".includes(src[at + 1]));
  }

  function readRedirection(at: number): number {
    if (src[at + 1] === "(") {
      const [text, end] = balanced(src, at + 2, "(", ")");
      nested.push({ text, offset: at + 2 });
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

  function classify(arg: Arg): void {
    const { value } = arg;
    if (nestNext) {
      nested.push(unplaced(value));
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
          skipValue = RUNNERS.get(runner)!.includes(value);
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
      current = { name, args: [] };
      found.push(current);
      previous = name;
      runner = RUNNERS.has(name) ? name : "";
      atCommand = false;
    } else {
      current?.args.push(arg);
      if (EXEC_FLAGS.has(value)) startCommand();
      else if (SHELL_FLAG.test(value) && SHELLS.has(previous)) nestNext = true;
      else if (previous === "eval") nested.push(unplaced(value));
    }
  }
}

/** The Bash command in the hook's stdin payload, or "" when it carries none. */
export function payloadCommand(input: string): string {
  try {
    return JSON.parse(input)?.tool_input?.command ?? "";
  } catch {
    return "";
  }
}

export async function readStdin(): Promise<string> {
  let input = "";
  for await (const chunk of process.stdin) input += chunk;
  return input;
}
