#!/usr/bin/env python3
"""Aggregate a samply profile of the native `wado` binary into Rust-level
bottlenecks.

samply's `--save-only` profile is **unsymbolicated** (funcTable.name holds a
hex relative-virtual-address). This script:

  1. keys every frame by (lib_index, rva) so identical raw addresses in
     different libraries are never merged;
  2. symbolicates each lib via the host's debuginfo tool:
       - macOS: `atos` (main executable's __TEXT base is 0x100000000,
         shared dylibs use base 0);
       - Linux: `addr2line -fC -e <path>` (PIE base is 0 because RVAs in
         the profile are already relative to the lib);
  3. weights samples by **threadCPUDelta** (real CPU), not wall-clock
     `weight` — otherwise parked tokio/rayon worker threads bury everything;
  4. reports a library breakdown, top self functions (all + main-binary
     only), and the syscall/alloc CPU attributed to its nearest Rust caller.

Usage:
  analyze_native_profile.py PROFILE.json [--top N] [--binary wado]
                            [--main-base 0x100000000] [--symbolicator auto|atos|addr2line]
"""
import argparse
import json
import platform
import re
import subprocess
import sys
from collections import defaultdict

HEXNAME = re.compile(r"^0x([0-9a-fA-F]+)$")
HASH_SUFFIX = re.compile(r"::h[0-9a-f]{16}\b")
OFFSET_SUFFIX = re.compile(r"\s*\+\s*\d+\s*$")
LINE_SUFFIX = re.compile(r"\s*\([^()]*:\d+\)\s*$")
# Common Rust v0/legacy mangling fragments atos leaves in place.
DEMANGLE = [
    ("$u20$", " "), ("$u7b$", "{"), ("$u7d$", "}"), ("$u5b$", "["),
    ("$u5d$", "]"), ("$LT$", "<"), ("$GT$", ">"), ("$C$", ","),
    ("$RF$", "&"), ("$LP$", "("), ("$RP$", ")"), ("..", "::"),
]


def demangle(name: str) -> str:
    for a, b in DEMANGLE:
        name = name.replace(a, b)
    return name.replace("_<", "<").strip()


def clean(name: str) -> str:
    """Collapse a symbol to its function identity (drop line/offset/hash)."""
    name = LINE_SUFFIX.sub("", name)
    name = OFFSET_SUFFIX.sub("", name)
    name = HASH_SUFFIX.sub("", name)
    return demangle(name)


def thread_func_keys(thread):
    """funcTable index -> (lib_index, rva) for one thread."""
    strings = thread["stringArray"]
    ftab = thread["funcTable"]
    rtab = thread["resourceTable"]
    keys = []
    for i in range(ftab["length"]):
        res = ftab["resource"][i]
        lib = (rtab["lib"][res] if res is not None and 0 <= res < rtab["length"]
               and rtab["lib"][res] is not None else -1)
        m = HEXNAME.match(strings[ftab["name"][i]])
        keys.append((lib, int(m.group(1), 16) if m else -1))
    return keys


def _symbolicate_atos(path, base, rvas, lib_name, arch):
    addrs = [hex(base + r) for r in rvas]
    try:
        lines = subprocess.run(
            ["atos", "-o", path, "-arch", arch, "-l", hex(base), *addrs],
            capture_output=True, text=True, timeout=180,
        ).stdout.splitlines()
    except (OSError, subprocess.SubprocessError):
        lines = []
    lines += [""] * (len(rvas) - len(lines))
    out = {}
    for r, line in zip(rvas, lines):
        s = line.strip()
        if not s or s.startswith("0x"):
            s = f"[{lib_name}]+{hex(r)}"
        out[r] = clean(s)
    return out


def _symbolicate_addr2line(path, rvas, lib_name):
    """`addr2line -fC -e <path>` over stdin; output is paired (func, file:line).

    PIE binaries on Linux store RVAs directly in the profile — no base offset
    to add. `-i` (inlined frames) is intentionally omitted because addr2line
    does not emit a per-input separator with `-i`, so the output cannot be
    reliably split back into addresses. The outermost frame is the one that
    matches the hot self-CPU bucket, which is what we want."""
    addrs = [hex(r) for r in rvas]
    try:
        proc = subprocess.run(
            ["addr2line", "-fC", "-e", path],
            input="\n".join(addrs) + "\n",
            capture_output=True, text=True, timeout=180,
        )
        lines = proc.stdout.splitlines()
    except (OSError, subprocess.SubprocessError):
        lines = []
    out = {}
    for idx, r in enumerate(rvas):
        func = lines[idx * 2] if idx * 2 < len(lines) else ""
        fileline = lines[idx * 2 + 1] if idx * 2 + 1 < len(lines) else ""
        func = func.strip()
        fileline = fileline.strip()
        if not func or func == "??":
            out[r] = f"[{lib_name}]+{hex(r)}"
        else:
            # Match the atos output shape so downstream filters
            # like `(in wado)` work on both platforms.
            out[r] = clean(f"{func} (in {lib_name}) ({fileline})")
    return out


def symbolicate(profile, binary, main_base, arch, symbolicator):
    """(lib_index, rva) -> cleaned symbol, batched per lib."""
    libs = profile["libs"]
    all_keys = set()
    for thread in profile["threads"]:
        all_keys.update(thread_func_keys(thread))

    by_lib = defaultdict(list)
    for lib, rva in all_keys:
        by_lib[lib].append(rva)

    out = {}
    for lib, rvas in by_lib.items():
        rvas = sorted(set(rvas))
        if lib < 0 or lib >= len(libs):
            for r in rvas:
                out[(lib, r)] = "[unknown]"
            continue
        meta = libs[lib]
        path = meta.get("path") or meta["name"]
        if symbolicator == "atos":
            base = main_base if meta["name"] == binary else 0
            sym = _symbolicate_atos(path, base, rvas, meta["name"], arch)
        elif symbolicator == "addr2line":
            sym = _symbolicate_addr2line(path, rvas, meta["name"])
        else:
            raise SystemExit(f"unknown symbolicator: {symbolicator}")
        for r, s in sym.items():
            out[(lib, r)] = s
    return out


def detect_symbolicator(explicit):
    if explicit != "auto":
        return explicit
    if sys.platform == "darwin":
        return "atos"
    return "addr2line"


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("profile")
    ap.add_argument("--top", type=int, default=30)
    ap.add_argument("--binary", default="wado", help="main executable lib name")
    ap.add_argument("--main-base", default="0x100000000",
                    help="__TEXT vmaddr of the main executable (atos only; "
                         "ignored on Linux PIE binaries)")
    ap.add_argument("--arch", default=platform.machine(),
                    help="atos -arch (defaults to the host arch, e.g. arm64 / x86_64)")
    ap.add_argument("--symbolicator", default="auto",
                    choices=["auto", "atos", "addr2line"],
                    help="auto: atos on macOS, addr2line on Linux")
    args = ap.parse_args()

    symbolicator = detect_symbolicator(args.symbolicator)
    profile = json.load(open(args.profile))
    libs = profile["libs"]
    keyname = symbolicate(profile, args.binary, int(args.main_base, 16),
                          args.arch, symbolicator)

    self_by = defaultdict(float)
    incl_by = defaultdict(float)
    lib_self = defaultdict(float)
    attr = defaultdict(float)        # syscall/alloc leaf -> nearest Rust caller
    total = 0.0
    used_wallclock = False

    for thread in profile["threads"]:
        keys = thread_func_keys(thread)
        fr_func = thread["frameTable"]["func"]
        st_frame = thread["stackTable"]["frame"]
        st_prefix = thread["stackTable"]["prefix"]
        samples = thread["samples"]
        stacks = samples["stack"]
        weights = samples.get("threadCPUDelta")
        if weights is None:
            weights = samples.get("weight")
            used_wallclock = True

        def key(stack_idx):
            return keys[fr_func[st_frame[stack_idx]]]

        def name(stack_idx):
            return keyname[key(stack_idx)]

        def lib_name(stack_idx):
            li = key(stack_idx)[0]
            return libs[li]["name"] if 0 <= li < len(libs) else "[unknown]"

        for i, leaf in enumerate(stacks):
            if leaf is None:
                continue
            w = 1.0 if weights is None else (weights[i] or 0)
            total += w
            self_by[name(leaf)] += w
            lib_self[lib_name(leaf)] += w
            # inclusive: dedup by name per sample (avoids >100% on recursion)
            seen = set()
            cur = leaf
            while cur is not None:
                n = name(cur)
                if n not in seen:
                    seen.add(n)
                    incl_by[n] += w
                cur = st_prefix[cur]
            # attribute non-main-binary leaf CPU to nearest main-binary caller
            if lib_name(leaf) != args.binary:
                cur = leaf
                caller = None
                while cur is not None:
                    if lib_name(cur) == args.binary:
                        caller = name(cur)
                        break
                    cur = st_prefix[cur]
                attr[caller or "[no Rust ancestor]"] += w

    if total == 0:
        print("No samples found.")
        return
    if used_wallclock:
        print("WARNING: no threadCPUDelta; weighting by wall-clock samples "
              "(idle/parked threads will dominate).\n")

    def show(title, table, pred=None):
        print(f"\n=== {title} ===")
        rows = [(k, v) for k, v in table.items() if pred is None or pred(k)]
        for k, v in sorted(rows, key=lambda kv: kv[1], reverse=True)[: args.top]:
            print(f"{v / total * 100:6.2f}%  {k}")

    in_bin = f"(in {args.binary})"
    print(f"symbolicator = {symbolicator}")
    print(f"total CPU weight = {total:.0f}")
    print("\n=== CPU by library (self) ===")
    for k, v in sorted(lib_self.items(), key=lambda kv: kv[1], reverse=True):
        print(f"{v / total * 100:6.2f}%  {k}")
    show("Top SELF — all", self_by)
    show(f"Top SELF — {args.binary} (Rust) only", self_by, lambda n: in_bin in n)
    show(f"Top INCLUSIVE — {args.binary} (Rust) only", incl_by, lambda n: in_bin in n)
    show("Syscall/alloc CPU attributed to nearest Rust caller", attr,
         lambda n: in_bin in n)


if __name__ == "__main__":
    main()
