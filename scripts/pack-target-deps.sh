#!/usr/bin/env bash
# Pack the dependency half of target/ into the tarball every session restores
# from, plus the manifest the restore hook validates it against.
#
# Only registry dependencies are packed. Workspace-owned artifacts are excluded
# on purpose: cargo decides a path dependency's freshness by mtime, and a fresh
# clone always looks newer than a CI artifact. Shipping them would either force
# the rebuild anyway or — once the mtimes were forced — silently reuse artifacts
# compiled from different source. Registry dependencies carry no such hazard;
# their fingerprints key on the immutable package version, which is why they
# survive a fresh clone untouched.
#
# `incremental/` is excluded too: it is per-branch state the session rebuilds as
# it edits.
set -e -o pipefail

OUT="${1:?usage: pack-target-deps.sh <out.tar.gz> <out.manifest.json>}"
MANIFEST_OUT="${2:?usage: pack-target-deps.sh <out.tar.gz> <out.manifest.json>}"
TARGET_DIR="${CARGO_TARGET_DIR:-${PWD}/target}"
WORK="$(mktemp -d)"
trap 'rm -rf "${WORK}"' EXIT

cargo metadata --no-deps --format-version 1 \
  | python3 -c '
import json, sys
names = set()
for p in json.load(sys.stdin)["packages"]:
    for t in p["targets"]:
        names.add(t["name"].replace("-", "_"))
print("\n".join(sorted(names)))' > "${WORK}/targets.txt"

python3 - "$TARGET_DIR" "${WORK}/targets.txt" > "${WORK}/files.list" <<'PY'
import os, re, sys

target_dir, names_file = sys.argv[1], sys.argv[2]
names = set(open(names_file).read().split())


def owned_by_workspace(name: str) -> bool:
    stem = re.sub(r"^lib", "", name)
    stem = re.split(r"-[0-9a-f]{8,}", stem)[0].split(".")[0]
    return stem.replace("-", "_") in names


# `target/debug` and, for a cross target, `target/<triple>/debug`. The workflow
# builds no cross targets today; walking for them anyway keeps the tarball a
# function of what was built, so adding one cannot silently go unpacked.
def profile_dirs(root: str):
    for name in sorted(os.listdir(root)):
        path = os.path.join(root, name)
        if not os.path.isdir(path):
            continue
        if os.path.isdir(os.path.join(path, "deps")):
            yield name, path
        else:
            for sub in sorted(os.listdir(path)):
                nested = os.path.join(path, sub)
                if os.path.isdir(os.path.join(nested, "deps")):
                    yield os.path.join(name, sub), nested


out = []
for profile, root in profile_dirs(target_dir):
    deps = os.path.join(root, "deps")
    for entry in os.listdir(deps):
        if not owned_by_workspace(entry):
            out.append(f"{profile}/deps/{entry}")
    # Artifacts before the fingerprints that vouch for them: the restore
    # extracts in this order, so an interrupted one leaves units cargo
    # rebuilds rather than units it wrongly believes are fresh.
    for sub in ("build", ".fingerprint"):
        base = os.path.join(root, sub)
        for dirpath, _, files in os.walk(base):
            rel = os.path.relpath(dirpath, root)
            unit = rel.split(os.sep)[1] if os.sep in rel else ""
            if unit and owned_by_workspace(unit):
                continue
            for f in files:
                out.append(os.path.join(profile, rel, f))
print("\n".join(out))
PY

# Resolved, not raw env: the session leaves CARGO_HOME unset and inherits the
# same directory by default, and the restore hook compares resolved paths.
python3 - "$MANIFEST_OUT" "$PWD" "${CARGO_HOME:-$HOME/.cargo}" "$TARGET_DIR" <<'PY'
import json, subprocess, sys

rustc = subprocess.run(["rustc", "-vV"], capture_output=True, text=True).stdout
version = next(l.split(": ", 1)[1] for l in rustc.splitlines() if l.startswith("release: "))
commit = subprocess.run(["git", "rev-parse", "HEAD"], capture_output=True, text=True).stdout.strip()

out, repo_root, cargo_home, target_dir = sys.argv[1:5]
json.dump(
    {
        "schema": 1,
        "repo_root": repo_root,
        "cargo_home": cargo_home,
        "cargo_target_dir": target_dir,
        "rustc": version,
        "commit": commit,
    },
    open(out, "w"),
    indent=2,
)
PY

# Last entry in the archive on purpose: the restore hook takes its presence in
# target/ as proof that an extraction finished, and skips on that.
cp "$MANIFEST_OUT" "$TARGET_DIR/wado-cache-manifest.json"
echo "wado-cache-manifest.json" >> "${WORK}/files.list"

tar -C "$TARGET_DIR" -czf "$OUT" -T "${WORK}/files.list"
echo "packed $(wc -l < "${WORK}/files.list") files -> $(du -h "$OUT" | cut -f1)"
cat "$MANIFEST_OUT"
