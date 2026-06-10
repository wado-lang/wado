#!/usr/bin/env node
/**
 * Aggregate a samply profile of the native `wado` binary into Rust-level
 * bottlenecks.
 *
 * samply's `--save-only` profile is **unsymbolicated** (funcTable.name holds a
 * hex relative-virtual-address). This script:
 *
 *   1. keys every frame by (lib_index, rva) so identical raw addresses in
 *      different libraries are never merged;
 *   2. symbolicates each lib via the host's debuginfo tool:
 *        - macOS: `atos` (main executable's __TEXT base is 0x100000000,
 *          shared dylibs use base 0);
 *        - Linux: `addr2line -fC -e <path>` (PIE base is 0 because RVAs in
 *          the profile are already relative to the lib);
 *   3. weights samples by **threadCPUDelta** (real CPU), not wall-clock
 *      `weight` — otherwise parked tokio/rayon worker threads bury everything;
 *   4. reports a library breakdown, top self functions (all + main-binary
 *      only), and the syscall/alloc CPU attributed to its nearest Rust caller.
 *
 * Runs directly on Node.js >= 23.6 (type stripping is on by default):
 *   ./analyze_native_profile.ts PROFILE.json [--top N] [--binary wado]
 *                             [--main-base 0x100000000] [--symbolicator auto|atos|addr2line]
 */
import { spawnSync } from "node:child_process";
import { readFileSync } from "node:fs";
import { machine } from "node:os";
import { parseArgs } from "node:util";

const HEXNAME = /^0x([0-9a-fA-F]+)$/;
const HASH_SUFFIX = /::h[0-9a-f]{16}\b/g;
const OFFSET_SUFFIX = /\s*\+\s*\d+\s*$/;
const LINE_SUFFIX = /\s*\([^()]*:\d+\)\s*$/;
// Common Rust v0/legacy mangling fragments atos leaves in place.
const DEMANGLE: [string, string][] = [
  ["$u20$", " "], ["$u7b$", "{"], ["$u7d$", "}"], ["$u5b$", "["],
  ["$u5d$", "]"], ["$LT$", "<"], ["$GT$", ">"], ["$C$", ","],
  ["$RF$", "&"], ["$LP$", "("], ["$RP$", ")"], ["..", "::"],
];

function demangle(name: string): string {
  for (const [a, b] of DEMANGLE) {
    name = name.split(a).join(b);
  }
  return name.split("_<").join("<").trim();
}

/** Collapse a symbol to its function identity (drop line/offset/hash). */
function clean(name: string): string {
  name = name.replace(LINE_SUFFIX, "");
  name = name.replace(OFFSET_SUFFIX, "");
  name = name.replace(HASH_SUFFIX, "");
  return demangle(name);
}

type Key = readonly [number, number];

/** Encode a (lib_index, rva) pair as a Map key. */
function kk(lib: number, rva: number): string {
  return `${lib} ${rva}`;
}

function hex(n: number): string {
  return "0x" + n.toString(16);
}

/** funcTable index -> (lib_index, rva) for one thread. */
function threadFuncKeys(thread: any): Key[] {
  const strings = thread.stringArray;
  const ftab = thread.funcTable;
  const rtab = thread.resourceTable;
  const keys: Key[] = [];
  for (let i = 0; i < ftab.length; i++) {
    const res = ftab.resource[i];
    let lib = -1;
    if (
      res != null && res >= 0 && res < rtab.length && rtab.lib[res] != null
    ) {
      lib = rtab.lib[res];
    }
    const m = HEXNAME.exec(strings[ftab.name[i]]);
    keys.push([lib, m ? parseInt(m[1], 16) : -1]);
  }
  return keys;
}

function symbolicateAtos(
  path: string, base: number, rvas: number[], libName: string, arch: string,
): Map<number, string> {
  const addrs = rvas.map((r) => hex(base + r));
  let lines: string[] = [];
  try {
    const proc = spawnSync(
      "atos", ["-o", path, "-arch", arch, "-l", hex(base), ...addrs],
      { encoding: "utf8", timeout: 180_000 },
    );
    lines = (proc.stdout ?? "").split("\n");
    if (lines.length && lines[lines.length - 1] === "") lines.pop();
  } catch {
    lines = [];
  }
  while (lines.length < rvas.length) lines.push("");
  const out = new Map<number, string>();
  for (let i = 0; i < rvas.length; i++) {
    const r = rvas[i];
    let s = (lines[i] ?? "").trim();
    if (!s || s.startsWith("0x")) {
      s = `[${libName}]+${hex(r)}`;
    }
    out.set(r, clean(s));
  }
  return out;
}

/**
 * `addr2line -fC -e <path>` over stdin; output is paired (func, file:line).
 *
 * PIE binaries on Linux store RVAs directly in the profile — no base offset
 * to add. `-i` (inlined frames) is intentionally omitted because addr2line
 * does not emit a per-input separator with `-i`, so the output cannot be
 * reliably split back into addresses. The outermost frame is the one that
 * matches the hot self-CPU bucket, which is what we want.
 */
function symbolicateAddr2line(
  path: string, rvas: number[], libName: string,
): Map<number, string> {
  const addrs = rvas.map((r) => hex(r));
  let lines: string[] = [];
  try {
    const proc = spawnSync(
      "addr2line", ["-fC", "-e", path],
      { input: addrs.join("\n") + "\n", encoding: "utf8", timeout: 180_000 },
    );
    lines = (proc.stdout ?? "").split("\n");
    if (lines.length && lines[lines.length - 1] === "") lines.pop();
  } catch {
    lines = [];
  }
  const out = new Map<number, string>();
  for (let idx = 0; idx < rvas.length; idx++) {
    const r = rvas[idx];
    const func = (idx * 2 < lines.length ? lines[idx * 2] : "").trim();
    const fileline = (idx * 2 + 1 < lines.length ? lines[idx * 2 + 1] : "")
      .trim();
    if (!func || func === "??") {
      out.set(r, `[${libName}]+${hex(r)}`);
    } else {
      // Match the atos output shape so downstream filters
      // like `(in wado)` work on both platforms.
      out.set(r, clean(`${func} (in ${libName}) (${fileline})`));
    }
  }
  return out;
}

/** (lib_index, rva) -> cleaned symbol, batched per lib. */
function symbolicate(
  profile: any, binary: string, mainBase: number, arch: string,
  symbolicator: string,
): Map<string, string> {
  const libs = profile.libs;
  const byLib = new Map<number, Set<number>>();
  for (const thread of profile.threads) {
    for (const [lib, rva] of threadFuncKeys(thread)) {
      let set = byLib.get(lib);
      if (!set) byLib.set(lib, (set = new Set()));
      set.add(rva);
    }
  }

  const out = new Map<string, string>();
  for (const [lib, rvaSet] of byLib) {
    const rvas = [...rvaSet].sort((a, b) => a - b);
    if (lib < 0 || lib >= libs.length) {
      for (const r of rvas) out.set(kk(lib, r), "[unknown]");
      continue;
    }
    const meta = libs[lib];
    const path = meta.path ?? meta.name;
    let sym: Map<number, string>;
    if (symbolicator === "atos") {
      const base = meta.name === binary ? mainBase : 0;
      sym = symbolicateAtos(path, base, rvas, meta.name, arch);
    } else if (symbolicator === "addr2line") {
      sym = symbolicateAddr2line(path, rvas, meta.name);
    } else {
      console.error(`unknown symbolicator: ${symbolicator}`);
      process.exit(1);
    }
    for (const [r, s] of sym) out.set(kk(lib, r), s);
  }
  return out;
}

function detectSymbolicator(explicit: string): string {
  if (explicit !== "auto") return explicit;
  if (process.platform === "darwin") return "atos";
  return "addr2line";
}

function main(): void {
  const { values, positionals } = parseArgs({
    allowPositionals: true,
    options: {
      top: { type: "string", default: "30" },
      binary: { type: "string", default: "wado" },
      "main-base": { type: "string", default: "0x100000000" },
      arch: { type: "string", default: machine() },
      symbolicator: { type: "string", default: "auto" },
    },
  });

  if (positionals.length < 1) {
    console.error(
      "usage: analyze_native_profile.ts PROFILE.json [--top N] " +
        "[--binary wado] [--main-base 0x100000000] " +
        "[--symbolicator auto|atos|addr2line]",
    );
    process.exit(2);
  }

  const profilePath = positionals[0];
  const top = parseInt(values.top as string, 10);
  const binary = values.binary as string;
  const mainBase = parseInt(values["main-base"] as string, 16);
  const arch = values.arch as string;
  const symChoice = values.symbolicator as string;
  if (!["auto", "atos", "addr2line"].includes(symChoice)) {
    console.error(
      `argument --symbolicator: invalid choice: '${symChoice}' ` +
        "(choose from 'auto', 'atos', 'addr2line')",
    );
    process.exit(2);
  }

  const symbolicator = detectSymbolicator(symChoice);
  const profile = JSON.parse(readFileSync(profilePath, "utf8"));
  const libs = profile.libs;
  const keyname = symbolicate(profile, binary, mainBase, arch, symbolicator);

  const selfBy = new Map<string, number>();
  const inclBy = new Map<string, number>();
  const libSelf = new Map<string, number>();
  const attr = new Map<string, number>(); // syscall/alloc leaf -> nearest Rust caller
  let total = 0.0;
  let usedWallclock = false;

  const add = (m: Map<string, number>, k: string, w: number) =>
    m.set(k, (m.get(k) ?? 0) + w);

  for (const thread of profile.threads) {
    const keys = threadFuncKeys(thread);
    const frFunc = thread.frameTable.func;
    const stFrame = thread.stackTable.frame;
    const stPrefix = thread.stackTable.prefix;
    const samples = thread.samples;
    const stacks = samples.stack;
    let weights = samples.threadCPUDelta;
    if (weights == null) {
      weights = samples.weight;
      usedWallclock = true;
    }

    const key = (stackIdx: number): Key => keys[frFunc[stFrame[stackIdx]]];
    const name = (stackIdx: number): string => {
      const [lib, rva] = key(stackIdx);
      return keyname.get(kk(lib, rva))!;
    };
    const libName = (stackIdx: number): string => {
      const li = key(stackIdx)[0];
      return li >= 0 && li < libs.length ? libs[li].name : "[unknown]";
    };

    for (let i = 0; i < stacks.length; i++) {
      const leaf = stacks[i];
      if (leaf == null) continue;
      const w = weights == null ? 1.0 : (weights[i] || 0);
      total += w;
      add(selfBy, name(leaf), w);
      add(libSelf, libName(leaf), w);
      // inclusive: dedup by name per sample (avoids >100% on recursion)
      const seen = new Set<string>();
      let cur: number | null = leaf;
      while (cur != null) {
        const n = name(cur);
        if (!seen.has(n)) {
          seen.add(n);
          add(inclBy, n, w);
        }
        cur = stPrefix[cur];
      }
      // attribute non-main-binary leaf CPU to nearest main-binary caller
      if (libName(leaf) !== binary) {
        cur = leaf;
        let caller: string | null = null;
        while (cur != null) {
          if (libName(cur) === binary) {
            caller = name(cur);
            break;
          }
          cur = stPrefix[cur];
        }
        add(attr, caller ?? "[no Rust ancestor]", w);
      }
    }
  }

  if (total === 0) {
    console.log("No samples found.");
    return;
  }
  if (usedWallclock) {
    console.log(
      "WARNING: no threadCPUDelta; weighting by wall-clock samples " +
        "(idle/parked threads will dominate).\n",
    );
  }

  const pct = (v: number): string =>
    (v / total * 100).toFixed(2).padStart(6) + "%";

  const show = (
    title: string, table: Map<string, number>,
    pred?: (k: string) => boolean,
  ) => {
    console.log(`\n=== ${title} ===`);
    const rows = [...table].filter(([k]) => pred == null || pred(k));
    rows.sort((a, b) => b[1] - a[1]);
    for (const [k, v] of rows.slice(0, top)) {
      console.log(`${pct(v)}  ${k}`);
    }
  };

  const inBin = `(in ${binary})`;
  console.log(`symbolicator = ${symbolicator}`);
  console.log(`total CPU weight = ${total.toFixed(0)}`);
  console.log("\n=== CPU by library (self) ===");
  const libRows = [...libSelf].sort((a, b) => b[1] - a[1]);
  for (const [k, v] of libRows) {
    console.log(`${pct(v)}  ${k}`);
  }
  show("Top SELF — all", selfBy);
  show(`Top SELF — ${binary} (Rust) only`, selfBy, (n) => n.includes(inBin));
  show(
    `Top INCLUSIVE — ${binary} (Rust) only`, inclBy, (n) => n.includes(inBin),
  );
  show(
    "Syscall/alloc CPU attributed to nearest Rust caller", attr,
    (n) => n.includes(inBin),
  );
}

main();
