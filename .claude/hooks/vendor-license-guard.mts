#!/usr/bin/env -S mise x -- node
// PreToolUse hook for the Bash tool: keep the implementation source a package's
// AGENTS.md forbids unread. `permissions.deny` covers the Read tool; this covers
// `cat`, `grep`, `find -exec` and friends, which reach the same bytes.
//
// Every path a command names is resolved against the directory it runs in, `cd`
// included, so `..` and a relative name reach the file they mean. A directory is
// a search root: naming one that holds forbidden source is denied unless the
// command only lists it. Whether a path is a directory is read from its name
// (no extension), since CI checks out no submodule. A search from above
// `vendor/` is not judged: it is not aimed at a vendor's source.

import path from "node:path";

import { type Command, commands, denial, readStdin } from "./shell-commands.mts";

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

/** Whether the command reads no file's contents, only names. */
function onlyLists({ name, args }: Command): boolean {
  if (LISTS.has(name)) return true;
  if (name !== "git") return false;
  const sub = args.findIndex(({ value }, i) => !value.startsWith("-") && args[i - 1]?.value !== "-C" && args[i - 1]?.value !== "-c");
  return sub >= 0 && GIT_LISTS.has(args[sub].value);
}

/** The paths a word may name: itself, and what follows a `=` in it. */
const candidates = (value: string) => [value, ...value.split("=").slice(1)].filter((v) => v !== "");

/** Why the command is denied, or null when it reads no forbidden source. */
export function denialReason(command: string, cwd: string = ROOT): string | null {
  let dir = cwd;
  for (const cmd of commands(command)) {
    const lists = onlyLists(cmd);
    const named = [...cmd.args.flatMap(({ value }) => candidates(value)), ...cmd.redirects.map(({ value }) => value)];
    for (const word of named) {
      const reason = judge(path.relative(ROOT, path.resolve(dir, word)), lists);
      if (reason) return reason;
    }
    if ((cmd.name === "cd" || cmd.name === "pushd") && cmd.args.length > 0) {
      dir = path.resolve(dir, cmd.args[cmd.args.length - 1].value);
    }
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
