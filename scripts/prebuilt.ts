// The builds CI compiles and hands between jobs and runs, and the check that
// keeps each reusable by the next.
//
// Usage (from the repository root): node scripts/prebuilt.ts build <BUILDS key>
//                                    node scripts/prebuilt.ts check
//
//   build  compiles one of BUILDS; `e2e` also copies the test binary, whose
//          name carries a hash, to target/debug/e2e
//   check  fails unless each build in REUSES compiles every unit it shares with
//          the build it starts from as that build does, naming what split them
//
// A build reuses a unit another left in target/ only where cargo names it
// alike: the same package, target, mode, features, profile and dependencies. A
// dev-dependency enabling a feature the other build does not name splits every
// unit above that crate, and each build then compiles the workspace again.

import { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import { copyFileSync } from "node:fs";

const BUILDS = {
  wado: ["build", "--locked", "--bin", "wado", "--bin", "wado-dev-tools"],
  // A selection of one package resolves features without the rest of the
  // workspace, so `-p wado-compiler-tests` would share no unit with the test
  // build.
  e2e: ["test", "--locked", "--workspace", "--test", "e2e", "--no-run"],
  // What `cargo test --workspace` compiles in the job that runs the Rust tests.
  test: ["test", "--locked", "--workspace", "--no-run"],
  // A session's plain `cargo build`.
  workspace: ["build", "--locked", "--workspace"],
};

// Each build that starts from another's units, after the one it starts from.
// CI builds `wado` then `e2e` in one job and hands that target/ to the test
// build; a session starts from the shared cache, which holds the test build's
// units. `e2e` to `test` is left out: a crate only a proc macro of the e2e
// build reaches is compiled for the host alone there, and shared with the
// program in the test build.
const REUSES: [from: keyof typeof BUILDS, to: keyof typeof BUILDS][] = [
  ["wado", "e2e"],
  ["wado", "test"],
  ["test", "workspace"],
];

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

function isBuild(name: string): name is keyof typeof BUILDS {
  return Object.hasOwn(BUILDS, name);
}

function build(name: keyof typeof BUILDS): void {
  if (name !== "e2e") {
    execFileSync("cargo", BUILDS[name], { stdio: "inherit" });
    return;
  }
  const messages = execFileSync(
    "cargo",
    [...BUILDS.e2e, "--message-format=json-render-diagnostics"],
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

type Role = "host" | "target";

type Graph = {
  units: Unit[];
  ids: string[];
  bySlot: Map<string, number[]>;
  // A crate a build script or a proc macro uses is compiled for the host, apart
  // from the same crate compiled for the program unless both come out alike;
  // nothing else in the unit graph tells the two apart.
  roles: Set<Role>[];
};

function unitGraph(args: string[]): Graph {
  const json = execFileSync("cargo", [...args, "--unit-graph", "-Z", "unstable-options"], {
    encoding: "utf8",
    maxBuffer: 1 << 30,
    stdio: ["ignore", "pipe", "inherit"],
    // `--unit-graph` is unstable; it only reads the resolution, never builds.
    env: { ...process.env, RUSTC_BOOTSTRAP: "1" },
  });
  const { units, roots }: { units: Unit[]; roots: number[] } = JSON.parse(json);
  const roles = units.map(() => new Set<Role>());
  const visit = (index: number, role: Role): void => {
    const unit = units[index];
    const own =
      unit.target.kind.includes("proc-macro") ||
      (unit.target.kind.includes("custom-build") && unit.mode === "build")
        ? "host"
        : role;
    if (roles[index].has(own)) {
      return;
    }
    roles[index].add(own);
    for (const dependency of unit.dependencies) {
      visit(dependency.index, own);
    }
  };
  for (const root of roots) {
    visit(root, "target");
  }
  const slots = units.map(slot);
  const bySlot = Map.groupBy(
    units.map((_, i) => i),
    (i) => slots[i],
  );
  return { units, ids: identities(units, slots), bySlot, roles };
}

// A profile's name is not part of a unit's artifact name: a `cargo build` and
// a `cargo test` share their library units.
function profileOf(unit: Unit): Record<string, unknown> {
  const { name: _, ...profile } = unit.profile;
  return profile;
}

// Which unit it is, whatever features and dependencies it was compiled with,
// short of the role it is compiled for.
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

// One digest per unit, over what cargo hashes into the unit's artifact name.
function identities(units: Unit[], slots: string[]): string[] {
  const memo = new Map<number, string>();
  const identity = (index: number): string => {
    const known = memo.get(index);
    if (known !== undefined) {
      return known;
    }
    const unit = units[index];
    const dependencies = unit.dependencies.map((d) => identity(d.index)).sort();
    const digest = createHash("sha256")
      .update(JSON.stringify([slots[index], unit.features, dependencies]))
      .digest("hex");
    memo.set(index, digest);
    return digest;
  };
  return units.map((_, index) => identity(index));
}

function label(graph: Graph, index: number): string {
  const unit = graph.units[index];
  const package_ = unit.pkg_id.split("#").at(-1);
  return `${package_} ${unit.target.kind}:${unit.target.name} (${unit.mode}, ${[...graph.roles[index]]})`;
}

// The units of `taken` compiled in the slot and for a role of `given`'s unit.
function counterparts(given: Graph, index: number, taken: Graph): number[] {
  const roles = given.roles[index];
  return (taken.bySlot.get(slot(given.units[index])) ?? []).filter((i) =>
    [...taken.roles[i]].some((r) => roles.has(r)),
  );
}

// How `taken`'s unit differs from `given`'s, its counterpart: in features, or
// else in which compiled units it depends on.
function difference(given: Graph, index: number, taken: Graph, theirs: number): string {
  const ours = given.units[index];
  const other = taken.units[theirs];
  const added = other.features.filter((f) => !ours.features.includes(f));
  const dropped = ours.features.filter((f) => !other.features.includes(f));
  if (added.length > 0 || dropped.length > 0) {
    return `features: adds [${added}], drops [${dropped}]`;
  }
  const dependencies = (graph: Graph, unit: Unit) =>
    new Map(unit.dependencies.map((d) => [graph.ids[d.index], label(graph, d.index)]));
  const ourDeps = dependencies(given, ours);
  const otherDeps = dependencies(taken, other);
  const only = (a: Map<string, string>, b: Map<string, string>) =>
    [...a].filter(([id]) => !b.has(id)).map(([, name]) => name);
  return `dependencies: adds [${only(otherDeps, ourDeps)}], drops [${only(ourDeps, otherDeps)}]`;
}

function check(): boolean {
  const graphs = new Map<string, Graph>();
  const graph = (name: keyof typeof BUILDS): Graph => {
    let built = graphs.get(name);
    if (built === undefined) {
      built = unitGraph(BUILDS[name]);
      graphs.set(name, built);
    }
    return built;
  };
  let ok = true;
  for (const [from, to] of REUSES) {
    const given = graph(from);
    const taken = graph(to);
    const split = new Map<number, number[]>();
    given.units.forEach((_, i) => {
      const theirs = counterparts(given, i, taken);
      if (theirs.length > 0 && !theirs.some((j) => taken.ids[j] === given.ids[i])) {
        split.set(i, theirs);
      }
    });
    if (split.size === 0) {
      console.log(`${to}: reuses every unit of the ${from} build it compiles`);
      continue;
    }
    ok = false;
    console.log(`${to}: compiles ${split.size} units of the ${from} build otherwise, from`);
    // Every split unit above one with no split dependency splits through it.
    for (const [i, theirs] of split) {
      if (given.units[i].dependencies.some((d) => split.has(d.index))) {
        continue;
      }
      for (const j of theirs) {
        console.log(`  ${label(given, i)}: ${difference(given, i, taken, j)}`);
      }
    }
  }
  return ok;
}

const [mode, name] = process.argv.slice(2);
if (mode === "build" && name !== undefined && isBuild(name)) {
  build(name);
} else if (mode === "check" && name === undefined) {
  if (!check()) {
    console.log("Name an added feature where the earlier build's own dependency is declared.");
    process.exit(1);
  }
} else {
  console.error(`usage: node scripts/prebuilt.ts build ${Object.keys(BUILDS).join("|")} | check`);
  process.exit(2);
}
