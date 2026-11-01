// The builds CI compiles once and hands to later jobs, and the check that keeps
// them reusable.
//
// Usage (from the repository root): node scripts/prebuilt.ts build wado|e2e
//                                    node scripts/prebuilt.ts check
//
//   build  compiles one prebuilt; `e2e` also copies the test binary, whose name
//          carries a hash, to target/debug/e2e
//   check  fails unless the builds that start from `wado`'s target/ compile
//          each unit it holds as it does, naming the features that split them
//
// The e2e build and the test build restore the target/ of `wado`'s build and
// build on it. They reuse a unit only where cargo names it alike: the same
// package, target, mode, features, profile and dependencies. A dev-dependency
// enabling a feature `wado` does not name splits every unit above that crate,
// and the run then compiles the workspace once per job.

import { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import { copyFileSync } from "node:fs";

const PREBUILTS = {
  wado: ["build", "--locked", "--bin", "wado", "--bin", "wado-dev-tools"],
  // A selection of one package resolves features without the rest of the
  // workspace, so `-p wado-compiler-tests` would share no unit with the test
  // build.
  e2e: ["test", "--locked", "--workspace", "--test", "e2e", "--no-run"],
};

// What `cargo test --workspace` compiles in the job that runs the Rust tests.
const TEST_BUILD = ["test", "--locked", "--workspace", "--no-run"];

const E2E_MANIFEST = "wado-compiler/tests/Cargo.toml";

type Unit = {
  pkg_id: string;
  target: { name: string; kind: string[] };
  mode: string;
  features: string[];
  profile: Record<string, unknown>;
  platform: string | null;
  dependencies: { index: number }[];
};

function isPrebuilt(name: string): name is keyof typeof PREBUILTS {
  return Object.hasOwn(PREBUILTS, name);
}

function build(name: keyof typeof PREBUILTS): void {
  if (name === "wado") {
    execFileSync("cargo", PREBUILTS.wado, { stdio: "inherit" });
    return;
  }
  const messages = execFileSync(
    "cargo",
    [...PREBUILTS.e2e, "--message-format=json-render-diagnostics"],
    { encoding: "utf8", maxBuffer: 1 << 30, stdio: ["ignore", "pipe", "inherit"] },
  );
  const executables = messages
    .split("\n")
    .filter((line) => line.startsWith("{"))
    .map((line) => JSON.parse(line))
    .filter(
      (m) =>
        m.reason === "compiler-artifact" &&
        m.target.name === "e2e" &&
        m.manifest_path.endsWith(E2E_MANIFEST) &&
        m.executable,
    )
    .map((m) => m.executable as string);
  if (executables.length !== 1) {
    throw new Error(`expected one e2e executable of ${E2E_MANIFEST}, got ${executables.length}`);
  }
  copyFileSync(executables[0], "target/debug/e2e");
}

function unitGraph(args: string[]): Unit[] {
  const json = execFileSync("cargo", [...args, "--unit-graph", "-Z", "unstable-options"], {
    encoding: "utf8",
    maxBuffer: 1 << 30,
    stdio: ["ignore", "pipe", "inherit"],
    // `--unit-graph` is unstable; it only reads the resolution, never builds.
    env: { ...process.env, RUSTC_BOOTSTRAP: "1" },
  });
  return JSON.parse(json).units;
}

// A profile's name is not part of a unit's artifact name: a `cargo build` and
// a `cargo test` share their library units.
function profileOf(unit: Unit): Record<string, unknown> {
  const { name: _, ...profile } = unit.profile;
  return profile;
}

// One digest per unit, over what cargo hashes into the unit's artifact name.
function identities(units: Unit[]): string[] {
  const memo = new Map<number, string>();
  const identity = (index: number): string => {
    const known = memo.get(index);
    if (known !== undefined) {
      return known;
    }
    const unit = units[index];
    const digest = createHash("sha256")
      .update(
        JSON.stringify([
          unit.pkg_id,
          unit.target.name,
          unit.target.kind,
          unit.mode,
          unit.features,
          profileOf(unit),
          unit.platform,
          unit.dependencies.map((d) => identity(d.index)).sort(),
        ]),
      )
      .digest("hex");
    memo.set(index, digest);
    return digest;
  };
  return units.map((_, index) => identity(index));
}

// Which unit it is, whatever features and dependencies it was compiled with.
// The profile tells a crate built for a build script or a proc macro, where
// cargo drops debuginfo, from the same crate built for the program.
function slot(unit: Unit): string {
  return JSON.stringify([
    unit.pkg_id,
    unit.target.name,
    unit.target.kind,
    unit.mode,
    profileOf(unit),
    unit.platform,
  ]);
}

function check(): boolean {
  const handed = unitGraph(PREBUILTS.wado);
  const handedIds = identities(handed);
  let ok = true;
  for (const [name, args] of [
    ["e2e", PREBUILTS.e2e],
    ["test", TEST_BUILD],
  ] as const) {
    const units = unitGraph(args);
    const ids = new Set(identities(units));
    const bySlot = Map.groupBy(units, slot);
    const split = handed.filter((unit, i) => bySlot.has(slot(unit)) && !ids.has(handedIds[i]));
    if (split.length === 0) {
      console.log(`${name}: reuses every unit of wado's build it compiles`);
      continue;
    }
    ok = false;
    console.log(`${name}: compiles ${split.length} units of wado's build otherwise`);
    // A unit whose features match was split by a dependency, named on its own line.
    const causes = new Set<string>();
    for (const unit of split) {
      for (const theirs of bySlot.get(slot(unit)) ?? []) {
        const added = theirs.features.filter((f) => !unit.features.includes(f));
        const dropped = unit.features.filter((f) => !theirs.features.includes(f));
        if (added.length > 0 || dropped.length > 0) {
          causes.add(`  ${unit.pkg_id}: adds [${added}], drops [${dropped}]`);
        }
      }
    }
    for (const cause of causes) {
      console.log(cause);
    }
  }
  return ok;
}

const [mode, name] = process.argv.slice(2);
if (mode === "build" && name !== undefined && isPrebuilt(name)) {
  build(name);
} else if (mode === "check" && name === undefined) {
  if (!check()) {
    console.log("Name each added feature where the prebuilt's own dependency is declared.");
    process.exit(1);
  }
} else {
  console.error("usage: node scripts/prebuilt.ts build wado|e2e | check");
  process.exit(2);
}
