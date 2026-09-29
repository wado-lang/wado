// Regenerates the index in docs/README.md: the title of every document in
// docs/, grouped by filename prefix. Only the text between the markers changes.

import { readdirSync, readFileSync, writeFileSync } from "node:fs";

const DOCS = "docs";
const README = `${DOCS}/README.md`;
const BEGIN = "<!-- BEGIN GENERATED: mise run update-docs-index -->";
const END = "<!-- END GENERATED -->";

const OVERVIEW = "spec-overview.md";

const GROUPS = [
  { heading: "Specification", prefix: "spec-", order: chapters },
  { heading: "Wado Evolution Proposals", prefix: "wep-" },
  { heading: "Standard Library", prefix: "stdlib-" },
  { heading: "Research", prefix: "research-" },
  { heading: "Guides", prefix: "" },
];
const UNLISTED = new Set(["README.md", "AGENTS.md"]);

function title(file) {
  let fence = false;
  for (const line of readFileSync(`${DOCS}/${file}`, "utf8").split("\n")) {
    if (/^(```|~~~)/.test(line)) fence = !fence;
    else if (!fence && line.startsWith("# ")) return line.slice(2).trim();
  }
  throw new Error(`${DOCS}/${file}: no top-level heading`);
}

// The overview's `## Chapters` list is the specification's reading order.
function chapters(files) {
  const text = readFileSync(`${DOCS}/${OVERVIEW}`, "utf8");
  const section = /^## Chapters\n([\s\S]*?)(?=^## |(?![\s\S]))/m.exec(text);
  if (!section) throw new Error(`${DOCS}/${OVERVIEW}: no "## Chapters" section`);
  const listed = [...section[1].matchAll(/^\d+\. \[[^\]]*\]\(\.\/(spec-[^)#]+\.md)\)/gm)].map((m) => m[1]);
  const expected = files.filter((f) => f !== OVERVIEW);
  const missing = expected.filter((f) => !listed.includes(f));
  const unknown = listed.filter((f) => !expected.includes(f));
  const repeated = listed.filter((f, i) => listed.indexOf(f) !== i);
  if (missing.length || unknown.length || repeated.length) {
    throw new Error(
      `${DOCS}/${OVERVIEW}: "## Chapters" must list every spec file once` +
        ` (missing: ${missing.join(", ") || "none"}; unknown: ${unknown.join(", ") || "none"};` +
        ` repeated: ${repeated.join(", ") || "none"})`,
    );
  }
  return [OVERVIEW, ...listed];
}

function index() {
  let files = readdirSync(DOCS)
    .filter((f) => f.endsWith(".md") && !UNLISTED.has(f))
    .sort();
  const sections = [];
  for (const g of GROUPS) {
    let mine = files.filter((f) => f.startsWith(g.prefix));
    files = files.filter((f) => !mine.includes(f));
    if (g.order) mine = g.order(mine);
    const items = mine.map((f) => `- [${title(f).replace(/^WEP: /, "")}](./${f})`);
    sections.push(`## ${g.heading}\n\n${items.join("\n")}`);
  }
  return sections.join("\n\n");
}

const current = readFileSync(README, "utf8");
const begin = current.indexOf(BEGIN);
const end = current.indexOf(END);
if (begin < 0 || end < begin) throw new Error(`${README}: missing index markers`);
const next = `${current.slice(0, begin)}${BEGIN}\n\n${index()}\n\n${current.slice(end)}`;

if (next !== current) writeFileSync(README, next);
