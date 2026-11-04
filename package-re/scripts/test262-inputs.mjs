// Writes tests/generated/test262.json: the patterns and subject strings
// test262's RegExp tests run, as cases for oracle.mjs. Each test runs in Node
// with RegExp.prototype.exec wrapped to record what it was called on; every
// search a RegExp makes, `test`, `match`, `replace` and `split` included, goes
// through it. test262 is read as a corpus of inputs here, and Node answers
// for them: its assertions are not read.
//
// A case is kept when Re means the same by it:
// - the flags beyond `dgy` are `m` and `s`, which become a modifier group;
//   `i` and `v` are left out;
// - Node accepts the pattern with the `u` flag;
// - it holds no lookaround or backreference, which Re refuses;
// - the subject is well formed, which a Wado string must be, and short.
//
// Run from the repository root, with vendor/test262 checked out:
// mise run test262-re

import { readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import vm from "node:vm";
import { acceptsUnderU, corpus, files, root } from "./test262-corpus.mjs";

const dirs = [
  "test/built-ins/RegExp",
  "test/built-ins/String/prototype/match",
  "test/built-ins/String/prototype/matchAll",
  "test/built-ins/String/prototype/replace",
  "test/built-ins/String/prototype/replaceAll",
  "test/built-ins/String/prototype/search",
  "test/built-ins/String/prototype/split",
];
const MAX_SUBJECT = 200;
const MAX_PATTERN = 200;
// The first subjects each pattern ran: `\S+` alone is run over every code point.
const MAX_SUBJECTS = 16;

function metadata(source) {
  const block = /\/\*---([\s\S]*?)---\*\//.exec(source)?.[1] ?? "";
  const list = (key) => {
    const m = new RegExp(`${key}:\\s*\\[([^\\]]*)\\]`).exec(block);
    return m ? m[1].split(",").map((s) => s.trim()).filter(Boolean) : [];
  };
  const flags = list("flags");
  return { includes: list("includes"), skip: flags.includes("async") || flags.includes("module") || flags.includes("raw") };
}

// Whether a `u`-mode pattern holds what WEP: Re lists as a known gap:
// lookaround, a backreference, the `i` modifier, a repeated group name, or a
// count above 4294967295.
function refused(pattern) {
  const names = new Set();
  let inClass = false;
  for (let i = 0; i < pattern.length; i++) {
    const c = pattern[i];
    const rest = pattern.slice(i + 1);
    if (c === "\\") {
      if (!inClass && /^[1-9k]/.test(rest)) {
        return true;
      }
      i++;
    } else if (inClass) {
      inClass = c !== "]";
    } else if (c === "[") {
      inClass = true;
    } else if (c === "(") {
      const name = /^\?<([^=!>]+)>/.exec(rest)?.[1];
      const modifiers = /^\?([a-z]*(?:-[a-z]*)?):/.exec(rest)?.[1] ?? "";
      if (/^\?(=|!|<=|<!)/.test(rest) || modifiers.includes("i") || (name && names.has(name))) {
        return true;
      }
      names.add(name);
    } else if (c === "{") {
      const bounds = /^(\d+)(?:,(\d*))?\}/.exec(rest)?.slice(1) ?? [];
      if (bounds.some((n) => Number(n || 0) > 4294967295)) {
        return true;
      }
    }
  }
  return false;
}

// The pattern under Re's rules, or null where Re means something else by it.
function asRe(source, flags) {
  if (/[^dgymsu]/.test(flags) || source.length > MAX_PATTERN) {
    return null;
  }
  const modifiers = [...flags].filter((f) => f === "m" || f === "s").join("");
  const pattern = modifiers ? `(?${modifiers}:${source})` : source;
  return acceptsUnderU(pattern) && !refused(pattern) ? pattern : null;
}

const harnesses = new Map();
const harness = (name) => {
  if (!harnesses.has(name)) {
    harnesses.set(name, readFileSync(join(corpus, "harness", name), "utf8"));
  }
  return harnesses.get(name);
};
const found = new Map();
const record = (pattern, flags, subject) => {
  const re = asRe(pattern, flags);
  if (re === null || subject.length > MAX_SUBJECT || !subject.isWellFormed()) {
    return;
  }
  if (!found.has(re)) {
    found.set(re, new Set());
  }
  if (found.get(re).size < MAX_SUBJECTS) {
    found.get(re).add(subject);
  }
};
for (const dir of dirs) {
  for (const path of files(join(corpus, dir))) {
    const source = readFileSync(path, "utf8");
    const { includes, skip } = metadata(source);
    if (skip) {
      continue;
    }
    const context = vm.createContext({ print() {}, __record: record });
    try {
      vm.runInContext(
        `(() => {
          const exec = RegExp.prototype.exec;
          RegExp.prototype.exec = function (s) {
            if (this instanceof RegExp && typeof s === "string") {
              __record(this.source, this.flags, s);
            }
            return exec.call(this, s);
          };
        })();`,
        context,
      );
      for (const name of ["assert.js", "sta.js", ...includes]) {
        vm.runInContext(harness(name), context);
      }
      vm.runInContext(source, context, { timeout: 2000 });
    } catch {
      // A test that fails in Node, or needs what the harness here lacks,
      // still recorded what it ran before it stopped.
    }
  }
}

const cases = [...found].sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0)).map(([pattern, subjects]) => [pattern, [...subjects]]);
writeFileSync(join(root, "tests/generated/test262.json"), `${JSON.stringify(cases, null, 2)}\n`);
