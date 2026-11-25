// Write the webidl2 AST of `package-web`'s `dom` slice as JSON.
//
// Usage: node snapshot.mjs <output.json> [--all]
//
// The slice is the interfaces listed below, their partials and included mixins
// from every spec, and the typedefs, callbacks, dictionaries and enums their
// members name. `wado-from-idl --webidl` reads the output, so generation never
// needs the network.
//
// `--all` takes every interface a page can reach in place of the list, and
// every namespace: what `mise run report-webidl-coverage` measures.

import { parseAll } from "@webref/idl";
import { readFile, writeFile } from "node:fs/promises";
import { createRequire } from "node:module";

// The CM package is the Wado package's own name. WebIDL interface names are
// unique across the web platform, so one CM package holds every module's.
const PACKAGE = "wado-lang:web";
const MODULE = "dom";
const SLICE = [
  "EventTarget",
  "Event",
  "Node",
  "CharacterData",
  "Text",
  "Element",
  "HTMLElement",
  "HTMLInputElement",
  "Document",
  "Window",
];

const [out, mode] = process.argv.slice(2);
if (!out || (mode !== undefined && mode !== "--all")) {
  console.error("Usage: node snapshot.mjs <output.json> [--all]");
  process.exit(1);
}

const all = await parseAll();
// Spec order is the output order, so the snapshot is deterministic.
const defs = Object.keys(all)
  .sort()
  .flatMap((spec) => all[spec]);

// An interface exposed only to workers or worklets is one no page reaches, and
// each of those globals would be a second `[Global]`.
const onWindow = (def) => {
  const exposed = def.extAttrs.find((a) => a.name === "Exposed");
  if (!exposed || exposed.rhs.type === "*") return true;
  return [exposed.rhs.value].flat().some((v) => (v.value ?? v) === "Window");
};
const everyInterface = () => {
  const defined = new Map(
    defs
      .filter((def) => def.type === "interface" && !def.partial)
      .map((def) => [def.name, def]),
  );
  const slice = new Set(
    [...defined.values()].filter(onWindow).map((def) => def.name),
  );
  // A slice is closed: a set grows while it is walked, so each parent's is visited too.
  for (const name of slice) {
    const parent = defined.get(name).inheritance;
    if (parent) slice.add(parent);
  }
  return slice;
};
const slice = mode === "--all" ? everyInterface() : new Set(SLICE);

const interfaces = defs.filter(
  (def) => def.type === "interface" && slice.has(def.name),
);
const includes = defs.filter(
  (def) => def.type === "includes" && slice.has(def.target),
);
const mixinNames = new Set(includes.map((i) => i.includes));
const mixins = defs.filter(
  (def) => def.type === "interface mixin" && mixinNames.has(def.name),
);
const namespaces = defs.filter(
  (def) => def.type === "namespace" && mode === "--all",
);

// A typedef, a callback, a dictionary or an enum is kept when a member of the
// slice names it, transitively.
const named = (types) =>
  new Map(defs.filter((def) => types.includes(def.type)).map((def) => [def.name, def]));
const typedefs = named(["typedef"]);
const callbacks = named(["callback", "callback interface"]);
const enums = named(["enum"]);
// A dictionary is its definition and its partials.
const dictionaries = Map.groupBy(
  defs.filter((def) => def.type === "dictionary"),
  (def) => def.name,
);
const kept = new Map();
const keptCallbacks = new Map();
const keptEnums = new Map();
const keptDictionaries = new Map();
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
  const en = enums.get(t.idlType);
  if (en) keptEnums.set(en.name, en);
  const parts = dictionaries.get(t.idlType);
  if (parts && !keptDictionaries.has(t.idlType)) {
    keptDictionaries.set(t.idlType, parts);
    for (const part of parts) {
      visitMembers(part.members);
      if (part.inheritance) visit({ idlType: part.inheritance });
    }
  }
};
for (const container of [...interfaces, ...mixins, ...namespaces]) {
  visitMembers(container.members);
}
const byName = (definitions) =>
  [...definitions.values()].flat().sort((a, b) => a.name.localeCompare(b.name));

const require = createRequire(import.meta.url);
const webref = JSON.parse(
  await readFile(require.resolve("@webref/idl/package.json"), "utf8"),
);

const snapshot = {
  webref: webref.version,
  package: PACKAGE,
  module: MODULE,
  slice: [...slice],
  interfaces,
  mixins,
  includes,
  typedefs: byName(kept),
  callbacks: byName(keptCallbacks),
  dictionaries: byName(keptDictionaries),
  enums: byName(keptEnums),
  namespaces,
};
await writeFile(out, JSON.stringify(snapshot, null, 2) + "\n");
console.error(
  `snapshot: ${slice.size} interfaces, ${mixinNames.size} mixins, ${kept.size} typedefs, ${keptCallbacks.size} callbacks, ${keptDictionaries.size} dictionaries, ${keptEnums.size} enums, ${namespaces.length} namespace definitions (@webref/idl ${webref.version}) → ${out}`,
);
