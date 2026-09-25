// Write the webidl2 AST of the `web:dom` slice as JSON.
//
// Usage: node snapshot.mjs <output.json>
//
// The slice is the interfaces listed below, their partials and included mixins
// from every spec, and the typedefs and callbacks their members name. `wado-from-idl --webidl`
// reads the output, so generation never needs the network.

import { parseAll } from "@webref/idl";
import { readFile, writeFile } from "node:fs/promises";
import { createRequire } from "node:module";

const PACKAGE = "dom";
const SLICE = [
  "EventTarget",
  "Event",
  "Node",
  "Element",
  "HTMLElement",
  "HTMLInputElement",
  "Document",
  "Window",
];

const out = process.argv[2];
if (!out) {
  console.error("Usage: node snapshot.mjs <output.json>");
  process.exit(1);
}

const all = await parseAll();
// Spec order is the output order, so the snapshot is deterministic.
const defs = Object.keys(all)
  .sort()
  .flatMap((spec) => all[spec]);

const interfaces = defs.filter(
  (def) => def.type === "interface" && SLICE.includes(def.name),
);
const includes = defs.filter(
  (def) => def.type === "includes" && SLICE.includes(def.target),
);
const mixinNames = new Set(includes.map((i) => i.includes));
const mixins = defs.filter(
  (def) => def.type === "interface mixin" && mixinNames.has(def.name),
);

// A typedef or a callback is kept when a member of the slice names it,
// transitively.
const named = (types) =>
  new Map(defs.filter((def) => types.includes(def.type)).map((def) => [def.name, def]));
const typedefs = named(["typedef"]);
const callbacks = named(["callback", "callback interface"]);
const kept = new Map();
const keptCallbacks = new Map();
const visitMembers = (members) => {
  for (const m of members) {
    if (m.idlType) visit(m.idlType);
    for (const a of m.arguments ?? []) visit(a.idlType);
  }
};
const visit = (t) => {
  if (Array.isArray(t.idlType)) {
    t.idlType.forEach(visit);
    return;
  }
  const td = typedefs.get(t.idlType);
  if (td && !kept.has(td.name)) {
    kept.set(td.name, td);
    visit(td.idlType);
  }
  const cb = callbacks.get(t.idlType);
  if (cb && !keptCallbacks.has(cb.name)) {
    keptCallbacks.set(cb.name, cb);
    visitMembers(cb.type === "callback" ? [cb] : cb.members);
  }
};
for (const iface of [...interfaces, ...mixins]) {
  visitMembers(iface.members);
}

const require = createRequire(import.meta.url);
const webref = JSON.parse(
  await readFile(require.resolve("@webref/idl/package.json"), "utf8"),
);

const snapshot = {
  webref: webref.version,
  package: PACKAGE,
  slice: SLICE,
  interfaces,
  mixins,
  includes,
  typedefs: [...kept.values()].sort((a, b) => a.name.localeCompare(b.name)),
  callbacks: [...keptCallbacks.values()].sort((a, b) => a.name.localeCompare(b.name)),
};
await writeFile(out, JSON.stringify(snapshot, null, 2) + "\n");
console.error(
  `snapshot: ${interfaces.length} interface and ${mixins.length} mixin definitions, ${kept.size} typedefs, ${keptCallbacks.size} callbacks (@webref/idl ${webref.version}) → ${out}`,
);
