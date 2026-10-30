#!/usr/bin/env -S mise x -- node
// PreToolUse hook for the Bash tool: keep the implementation source a package's
// AGENTS.md forbids unread. `permissions.deny` covers the Read tool; this covers
// `cat`, `grep`, `find -exec` and friends, which reach the same bytes.
//
// Every path a command names is resolved against the directory it runs in, `cd`,
// `cd -`, `pushd` and `popd` included, so `..` and a relative name reach the file
// they mean. What the guard cannot evaluate, an expansion or a directory it lost
// track of, is judged from the word's `vendor` on: the guard catches a command
// aimed at the source, not one written to evade it. A directory is
// a search root: naming one that holds forbidden source is denied unless the
// command only lists it. Whether a path is a directory is read from its name
// (no extension), since CI checks out no submodule. A search from above
// `vendor/` is not judged: it is not aimed at a vendor's source.

import path from "node:path";

import { type Arg, type Command, commands, denial, readStdin } from "./shell-commands.mts";

type Vendor = {
  /** Directories whose files of `extensions` are forbidden. */
  roots: string[];
  /** Directories under a root that stay readable. */
  allowed: string[];
  extensions: RegExp;
  reason: string;
};

const ANTLR4 =
  "License hygiene (package-gale/AGENTS.md): ANTLR4 implementation source under vendor/antlr4/{tool,runtime} must not be read. To settle what ANTLR does, run the published jar as a black box (`mise run oracle-gale`, package-gale/scripts/antlr4_oracle.wado) or read vendor/antlr4/doc/*.md. Reading a .g4 file or a runtime-testsuite descriptor by its name is allowed.";

const VENDORS: Vendor[] = [
  { roots: ["vendor/antlr4/tool"], allowed: [], extensions: /\.(java|g)$/, reason: ANTLR4 },
  { roots: ["vendor/antlr4/runtime"], allowed: [], extensions: /\.java$/, reason: ANTLR4 },
  {
    roots: ["vendor/onnx/onnx", "vendor/onnx/tools", "vendor/onnx/docs/docsgen"],
    allowed: ["vendor/onnx/onnx/backend/test", "vendor/onnx/onnx/fuzz"],
    extensions: /\.(py|cc|cpp|c)$/,
    reason:
      "License hygiene (package-loam/AGENTS.md): ONNX's source under vendor/onnx must not be read. Read the operator definitions in vendor/onnx/docs/*.md by name, the .proto files, the headers, and the tests under vendor/onnx/tests and vendor/onnx/onnx/backend/test. To settle a behavior, run onnxruntime as an oracle.",
  },
];

const ROOT = path.resolve(import.meta.dirname, "../..");
const GLOB = /[*?[{]/;
const LISTS = new Set(["cd", "pushd", "ls", "tree", "mkdir"]);
const GIT_LISTS = new Set(["submodule", "status", "fetch", "log", "rev-parse", "ls-files", "checkout", "switch", "describe"]);

const within = (p: string, dir: string) => p === dir || p.startsWith(`${dir}/`);

/** Why the repository-relative `rel` may not be named, or null. */
function judge(rel: string, lists: boolean): string | null {
  if (!within(rel, "vendor")) return null;
  const literal = GLOB.test(rel) ? rel.slice(0, rel.search(GLOB)).replace(/\/[^/]*$/, "") : rel;
  const directory = literal !== rel || !path.basename(literal).slice(1).includes(".");
  for (const vendor of VENDORS) {
    const kept = vendor.allowed.some((dir) => within(literal, dir));
    for (const root of vendor.roots) {
      if (directory) {
        if (lists) continue;
        if ((within(literal, root) && !kept) || within(root, literal)) return vendor.reason;
      } else if (within(literal, root) && !kept && vendor.extensions.test(literal)) {
        return vendor.reason;
      }
    }
  }
  return null;
}

/** `git log`'s options that print each commit's diff, which is the files' contents. */
const GIT_PATCH = /^(-p|-u|-U\d*|--patch|--unified(=.*)?|--word-diff(=.*)?|--color-words(=.*)?|--cc|--patch-with-stat|--patch-with-raw)$/;

/** Whether the command reads no file's contents, only names. */
function onlyLists({ name, args }: Command): boolean {
  if (LISTS.has(name)) return true;
  if (name !== "git") return false;
  const sub = args.findIndex(({ value }, i) => !value.startsWith("-") && args[i - 1]?.value !== "-C" && args[i - 1]?.value !== "-c");
  if (sub < 0 || !GIT_LISTS.has(args[sub].value)) return false;
  return !(args[sub].value === "log" && args.slice(sub + 1).some(({ value }) => GIT_PATCH.test(value)));
}

/** Where the shell stands: its directory, the one `cd -` returns to, and
 * `pushd`'s stack. `null` is a directory the guard cannot know. */
type Shell = { dir: string | null; previous: string | null; stack: (string | null)[] };

/** A word's leading `$NAME`, `${NAME}`, `~+`, `~-` or `~user`. */
const PREFIX = /^(?:\$(?:\{([A-Za-z_]\w*)\}|([A-Za-z_]\w*))|~([^/]*))(?=\/|$)/;
const TILDES: Record<string, string> = { "+": "PWD", "-": "OLDPWD" };

/** The directory a leading expansion named `name` stands for, if the guard knows it. */
function expanded(name: string, shell: Shell): string | null {
  if (name === "PWD") return shell.dir;
  if (name === "OLDPWD") return shell.previous;
  return null;
}

/** The absolute path `word` names, as the shell expands it where it stands. A
 * word is resolved only where the guard knows every expansion in it: none, or a
 * leading `$PWD` or `$OLDPWD` and their tildes. Any other start (an expansion, a
 * substitution, an unknown directory) is taken as the repository from the
 * word's `vendor` on, and a word naming no `vendor` names nothing judged.
 * `raw` is the word as written: a substitution in it is gone from `word`. */
function locate(word: string, raw: string, shell: Shell): string | null {
  const prefix = word.match(PREFIX);
  const rest = prefix ? word.slice(prefix[0].length).replace(/^\//, "") : word;
  const substituted = raw.includes("$(") || raw.includes("`");
  let base: string | null = null;
  if (!substituted && !rest.includes("$")) {
    if (prefix) base = expanded(prefix[1] ?? prefix[2] ?? TILDES[prefix[3]] ?? "", shell);
    else if (path.isAbsolute(word)) return word;
    else base = shell.dir;
  }
  if (base !== null) return path.resolve(base, rest);
  const from = `/${rest}`.search(/\/vendor(\/|$)/);
  return from >= 0 ? path.join(ROOT, `/${rest}`.slice(from + 1)) : null;
}

/** The paths a word may name, as git and the shell read it: itself, and what
 * follows a `=` or a git `<rev>:` in it. */
function candidates({ value, raw }: Arg, shell: Shell): string[] {
  const words = [value, ...value.split("=").slice(1), ...value.split(":").slice(1)];
  return words.flatMap((word) => (word === "" ? [] : (locate(word, raw, shell) ?? [])));
}

/** Where the shell stands once `cmd` has run. */
function entered({ name, args }: Command, shell: Shell): Shell {
  const target = args.at(-1);
  const enter = (dir: string | null, stack = shell.stack): Shell => ({ dir, previous: shell.dir, stack });
  const at = ({ value, raw }: Arg) => locate(value, raw, shell);
  switch (name) {
    case "cd":
      if (target === undefined) return enter(null);
      return target.value === "-" ? enter(shell.previous) : enter(at(target));
    case "pushd":
      if (target === undefined) {
        const top = shell.stack.at(-1);
        return top === undefined ? shell : enter(top, [...shell.stack.slice(0, -1), shell.dir]);
      }
      // `+N` and `-N` rotate the stack.
      if (/^[+-]\d/.test(target.value)) return enter(null, []);
      return enter(at(target), [...shell.stack, shell.dir]);
    case "popd": {
      const top = shell.stack.at(-1);
      if (target !== undefined || top === undefined) return enter(null, []);
      return enter(top, shell.stack.slice(0, -1));
    }
    default:
      return shell;
  }
}

/** Why the command is denied, or null when it reads no forbidden source. */
export function denialReason(command: string, cwd: string = ROOT): string | null {
  let shell: Shell = { dir: cwd, previous: null, stack: [] };
  for (const cmd of commands(command)) {
    const lists = onlyLists(cmd);
    for (const target of [...cmd.args, ...cmd.redirects].flatMap((arg) => candidates(arg, shell))) {
      const reason = judge(path.relative(ROOT, target), lists);
      if (reason) return reason;
    }
    shell = entered(cmd, shell);
  }
  return null;
}

if (import.meta.main) {
  const input = await readStdin();
  // A guard that throws must not let the command through.
  let reason: string | null;
  try {
    const payload = JSON.parse(input);
    reason = denialReason(payload?.tool_input?.command ?? "", payload?.cwd ?? ROOT);
  } catch (error) {
    reason = `this command could not be read (${(error as Error).message}), so it is denied. Rephrase it, or report the input if it is an ordinary one.`;
  }
  if (reason) process.stdout.write(denial(reason));
}
