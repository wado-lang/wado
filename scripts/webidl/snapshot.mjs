// Write the webidl2 AST of the `web:dom` slice as JSON.
//
// Usage: node snapshot.mjs <output.json>
//
// The slice is the interfaces listed below, their `partial interface`s and
// the mixins they include — as written by the specs that define one of them —
// and the typedefs their members name. The output is what
// `wado-from-idl --webidl` reads, so generation never needs the network.

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

// webidl2 nodes expose their fields through getters, so a spread would drop
// them; `toJSON` is the plain shape.
const plain = (spec, def) => ({ spec, ...JSON.parse(JSON.stringify(def)) });

const all = await parseAll();
const specs = Object.keys(all).sort();

const inSlice = (def) => def.type === "interface" && SLICE.includes(def.name);
const defining = specs.filter((spec) =>
  all[spec].some((def) => inSlice(def) && !def.partial),
);
const interfaces = [];
const includes = [];
for (const spec of defining) {
  for (const def of all[spec]) {
    if (inSlice(def)) interfaces.push(plain(spec, def));
    if (def.type === "includes" && SLICE.includes(def.target)) {
      includes.push(plain(spec, def));
    }
  }
}
const mixinNames = new Set(includes.map((i) => i.includes));
const mixins = [];
for (const spec of defining) {
  for (const def of all[spec]) {
    if (def.type === "interface mixin" && mixinNames.has(def.name)) {
      mixins.push(plain(spec, def));
    }
  }
}

// A typedef is kept when a member of the slice names it, transitively.
const typedefs = new Map();
for (const spec of specs) {
  for (const def of all[spec]) {
    if (def.type === "typedef") typedefs.set(def.name, plain(spec, def));
  }
}
const named = new Set();
const visit = (t) => {
  if (Array.isArray(t.idlType)) t.idlType.forEach(visit);
  else named.add(t.idlType);
};
for (const iface of [...interfaces, ...mixins]) {
  for (const m of iface.members) {
    if (m.idlType) visit(m.idlType);
    for (const a of m.arguments ?? []) visit(a.idlType);
  }
}
const kept = [];
const queue = [...named];
while (queue.length) {
  const name = queue.shift();
  const td = typedefs.get(name);
  if (!td || kept.includes(td)) continue;
  kept.push(td);
  const before = named.size;
  visit(td.idlType);
  queue.push(...[...named].slice(before));
}
kept.sort((a, b) => a.name.localeCompare(b.name));

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
  typedefs: kept,
};
await writeFile(out, JSON.stringify(snapshot, null, 2) + "\n");
console.error(
  `snapshot: ${interfaces.length} interface and ${mixins.length} mixin definitions from ${defining.join(", ")}, ${kept.length} typedefs (@webref/idl ${webref.version}) → ${out}`,
);
