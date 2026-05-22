#!/usr/bin/env python3
"""Aggregate a samply profile of the native `wado` binary into Rust-level
bottlenecks.

samply's `--save-only` profile is **unsymbolicated** (funcTable.name holds a
hex relative-virtual-address). This script:

  1. keys every frame by (lib_index, rva) so identical raw addresses in
     different libraries are never merged;
  2. symbolicates each lib with `atos` (macOS) — the main executable's
     __TEXT base is 0x100000000, shared dylibs use base 0;
  3. weights samples by **threadCPUDelta** (real CPU), not wall-clock
     `weight` — otherwise parked tokio/rayon worker threads bury everything;
  4. reports a library breakdown, top self functions (all + main-binary
     only), and the syscall/alloc CPU attributed to its nearest Rust caller.

macOS-specific (uses `atos`). On Linux, `samply load` symbolicates natively;
the weighting/attribution logic here is the platform-agnostic part.

Usage:
  analyze_native_profile.py PROFILE.json [--top N] [--binary wado]
                            [--main-base 0x100000000]
"""
import argparse
import json
import platform
import re
import subprocess
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


def symbolicate(profile, binary, main_base, arch):
    """(lib_index, rva) -> cleaned symbol, via atos batched per lib."""
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
        base = main_base if meta["name"] == binary else 0
        addrs = [hex(base + r) for r in rvas]
        try:
            lines = subprocess.run(
                ["atos", "-o", path, "-arch", arch, "-l", hex(base), *addrs],
                capture_output=True, text=True, timeout=180,
            ).stdout.splitlines()
        except (OSError, subprocess.SubprocessError):
            lines = []
        lines += [""] * (len(rvas) - len(lines))
        for r, line in zip(rvas, lines):
            s = line.strip()
            if not s or s.startswith("0x"):
                s = f"[{meta['name']}]+{hex(r)}"
            out[(lib, r)] = clean(s)
    return out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("profile")
    ap.add_argument("--top", type=int, default=30)
    ap.add_argument("--binary", default="wado", help="main executable lib name")
    ap.add_argument("--main-base", default="0x100000000",
                    help="__TEXT vmaddr of the main executable")
    ap.add_argument("--arch", default=platform.machine(),
                    help="atos -arch (defaults to the host arch, e.g. arm64 / x86_64)")
    args = ap.parse_args()

    profile = json.load(open(args.profile))
    libs = profile["libs"]
    keyname = symbolicate(profile, args.binary, int(args.main_base, 16), args.arch)

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
    print(f"total CPU weight = {total:.0f}")
    print("\n=== CPU by library (self) ===")
    for k, v in sorted(lib_self.items(), key=lambda kv: kv[1], reverse=True):
        print(f"{v / total * 100:6.2f}%  {k}")
    show("Top SELF — all", self_by)
    show(f"Top SELF — {args.binary} (Rust) only", self_by, lambda n: in_bin in n)
    show("Syscall/alloc CPU attributed to nearest Rust caller", attr,
         lambda n: in_bin in n)


if __name__ == "__main__":
    main()
