# Marl Format — A Self-Hosted Markdown Formatter, Replacing dprint

## Context

`wado-dev-tools format-md` (invoked by `mise run format`) currently formats every
`*.md` file in the repository by embedding the `dprint-plugin-markdown` Rust
crate (`wado-dev-tools/src/format_md.rs`). The goal is to replace it with a
formatter written in Wado, living in `package-marl` — the GFM-subset
Markdown-to-HTML renderer introduced by
[WEP: Sheaf & Marl](./wep-2026-07-05-sheaf.md) — so that formatting Markdown
becomes another instance of Wado dogfooding its own toolchain, alongside Sheaf
and Kiln.

The bar is "at least byte-identical to dprint's current output for every
Markdown file in this repository," not general CommonMark-formatter parity.
That is a real, checkable target: `cargo run -p wado-dev-tools -- format-md
--check` exits 0 today, with zero diffs, across every `*.md` file the walk
discovers (`DEFAULT_EXCLUDED_DIRS` in `format_md.rs`: `.vscode-test`, `vendor`,
`target`, `node_modules`, `.git` — notably `.claude/` is _not_ excluded). That
corpus is large and non-trivial: 201 files, ~4.1 MB, ~80,000 lines, including
hand-written docs and WEPs, `.claude/skills/*/SKILL.md` (YAML front matter),
`wado-lsp/lsp.md` (a copy of the LSP specification, raw HTML badges), and
`wado-compiler/ref/tc39-temporal.md` (a copy of the TC39 Temporal proposal,
~9,200 lines of deeply nested numbered lists). Because the corpus is already
canonical, **it doubles as the golden oracle**: a differential test that runs
both formatters over every file and asserts identical output is the concrete,
automatable definition of "compatible" (see Testing below), not a matter of
manual judgment.

### What dprint-plugin-markdown actually does

`wado-dev-tools` calls `format_text(text, &ConfigurationBuilder::new().build(),
|_, _, _| Ok(None))` — default config, and a no-op callback for reformatting
fenced-code-block bodies. Reading the crate's actual source (0.22.1, fetched
from GitHub; crates.io itself is blocked by this environment's egress policy)
rather than guessing pins down the behavior precisely:

- It parses with `pulldown-cmark` (full CommonMark, plus `ENABLE_TABLES`,
  `ENABLE_FOOTNOTES`, `ENABLE_STRIKETHROUGH`, `ENABLE_TASKLISTS`,
  `ENABLE_YAML_STYLE_METADATA_BLOCKS`, `ENABLE_PLUSES_DELIMITED_METADATA_BLOCKS`,
  `ENABLE_MATH`), rebuilds a small AST of _ranges into the original source_
  (`generation/common/ast_nodes.rs`), and prints it through `dprint-core`'s
  doc-printer.
- Default config (`configuration/resolve_config.rs`): `line_width = 80`,
  `text_wrap = Maintain`, `emphasis_kind = Underscores`, `strong_kind =
  Asterisks`, `unordered_list_kind = Dashes`, `heading_kind = Atx`,
  `list_indent_kind = CommonMark`.
- **`text_wrap: Maintain` never reflows prose to fit `line_width`.** A `\n`
  inside a paragraph is preserved as a hard line break in the output
  (`generation/generate.rs::gen_str`'s `TextBuilder`: on `Maintain`, `'\n'`
  always becomes `Signal::NewLine`; only redundant runs of spaces collapse to
  one). This is the single most important simplification: it turns "build a
  Markdown formatter" from "implement a line-breaking algorithm" into
  "reproduce the author's line breaks and normalize everything else." Line
  width still matters in two narrow spots: an inline link's text collapses to
  one line only if `single_line_text.len() < line_width / 2` (a **byte**
  count, not a display width — replicated verbatim, quirk included), and GFM
  table column widths, which do use `unicode_width::UnicodeWidthStr::width`
  (display width).
- `format_code_block_text` is a no-op in wado-dev-tools, so fenced code bodies
  (`` ```rust ``, `` ```wado ``, `` ```typescript ``, …) are only de-indented and
  trailing-trimmed, never recursively reformatted — no embedded-language
  formatter is needed. Front-matter bodies are handled the same way (their own
  recursive `format_text("yaml", …)` call also no-ops).
- `unicode-width` 0.1.10 (the pinned version) does a pure [UAX #11 East Asian Width](https://www.unicode.org/reports/tr11/) table lookup: Wide/Fullwidth
  ⇒ 2, everything else ⇒ 1 (this version has **no** zero-width case for
  combining marks or variation selectors — confirmed by reading its generated
  `tables.rs`, and cross-checked against a real corpus example: the "⬆️ [-1]"
  table cell in `tc39-temporal.md`, where the arrow _and_ its variation
  selector each measure width 1 against the file's actual column padding).
  Only C0/C1 controls measure 0.

### Corpus survey — what's actually in scope

Grepping the full corpus (not guessing) settles which CommonMark/GFM features
must be supported precisely, and which can be safely deferred:

| Construct                                                                        | Present?                                            | Notes                                                                                                                    |
| -------------------------------------------------------------------------------- | --------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------ |
| ATX headings, paragraphs, blockquotes, thematic breaks                           | Yes, pervasive                                      |                                                                                                                          |
| Fenced code (`` ``` ``)                                                          | Yes, pervasive                                      | No `~~~` fences found; dprint always emits backtick fences anyway                                                        |
| Ordered/unordered lists, nested                                                  | Yes, pervasive; `tc39-temporal.md` nests 5–6 levels | Stresses marker-width-dependent indent                                                                                   |
| GFM tables                                                                       | Yes, 120 files                                      | Including non-ASCII cell content (see width note above)                                                                  |
| Task lists (`- [ ]`)                                                             | Yes, 65 files                                       |                                                                                                                          |
| Images                                                                           | Yes, 23 files                                       |                                                                                                                          |
| YAML/`+++` front matter                                                          | Yes, 14 files, always at document start             | `.claude/skills/*/SKILL.md`, `wado-lsp/lsp.md`                                                                           |
| Raw HTML (block and inline)                                                      | Yes                                                 | `docs/wado-lang.md` (badge `<p>`/`<img>`), `wado-lsp/lsp.md` (anchor `<div>`/`<a>`)                                      |
| Reference links: full `[text][label]`, collapsed `[label][]`, shortcut `[label]` | Yes, 7+ files                                       | e.g. `[CLI-subcommands WEP][cli-wep]`, ``[`BlobDataProvider`]`` (nested code span as link text), `[wado-lang/wado#1522]` |
| Strikethrough (`~~`)                                                             | Rare (5 files)                                      |                                                                                                                          |
| Setext headings (`===`/`---` underline)                                          | **No**                                              | Every `---` adjacent to text is a front-matter close, verified line by line                                              |
| Footnotes (`[^id]` / `[^id]:`)                                                   | **No**                                              | The only two hits are a WEP mentioning the feature name and an ANTLR character-class example                             |
| Inline/display math (`$...$`)                                                    | **No**                                              | No genuine occurrences past regex false positives (shell `$VAR`, regex anchors)                                          |
| Indented (4-space, non-fenced) code                                              | Not found at top level                              | See Non-goals for why it's still handled                                                                                 |

## Decision

### A real parser, not a "GFM subset"

Marl's existing renderer (`render(source) -> String`) is deliberately a GFM
_subset_: its WEP lists reference links, setext headings, and raw HTML
passthrough as explicitly excluded, rendered as literal text. That is the
right call for a blog renderer, but wrong for a formatter — a formatter must
round-trip _this repository's actual prose_ byte-faithfully, and the corpus
survey above shows reference links and raw HTML are real, not hypothetical.
Marl Format therefore implements a materially more complete CommonMark + GFM
grammar than `render` does.

### A separate AST and pipeline, alongside the existing renderer

`render`'s block/inline scanners (`marl.wado`, `inline.wado`) go straight from
source text to HTML strings — `resolve_pair` in `inline.wado` literally
inserts `<em>`/`</em>` tokens into the stream. That is fine for HTML
generation, which only needs _semantic_ content and can discard exact
spelling (which literal marker was used, exact original spacing). A
formatter needs the opposite: it must retain enough of the original text
(spans, chosen delimiters, marker characters) to reproduce or deliberately
normalize them. Retrofitting one shared AST that serves both a "discard
formatting, keep semantics" consumer and a "preserve-and-normalize
everything" consumer would either compromise the renderer's simplicity or
the formatter's fidelity. So: **a new, dedicated AST and formatter pipeline**,
package-local to `package-marl`, that does not touch `marl.wado` / `inline.wado`
/ `html.wado`. It reuses the same _algorithmic style_ as the existing code —
line-based block scanning, a tokenize-then-resolve delimiter stack for
emphasis (`inline.wado`'s `process_emphasis` is the right shape, just
retargeted to build nodes instead of splicing HTML) — and the same
`StrCursor` utility, but is otherwise independent. This mirrors how the
Sheaf WEP already treats Marl and Sheaf as separate, single-purpose packages
rather than one do-everything module.

Proposed layout (new files only; existing renderer files are untouched):

```
package-marl/src/
  fmt_ast.wado          -- AST node types (block + inline)
  fmt_parse_block.wado  -- line-based block scanner -> AST
  fmt_parse_inline.wado -- tokenize + delimiter-stack resolve -> AST
  fmt_links.wado        -- link/image/autolink parsing; 2-pass reference-definition table
  fmt_print.wado         -- AST -> canonical Markdown string
  unicode_width.wado     -- display-width table (East Asian Width)
  format.wado             -- pub fn format(source: &String) -> String
  fmt_*_test.wado         -- one test file per module, existing package convention
```

`lib.wado` gains `pub use { format } from "./format.wado";` next to the
existing `render` re-export.

### AST

One node enum covering both block and inline levels (mirroring dprint's own
`ast_nodes.rs`, the reference implementation for "what does a Markdown AST
need to hold to round-trip"):

```wado
variant Node {
    // Blocks
    Document(List<Node>),
    FrontMatter([FrontMatterKind, String]),        // kind, raw body text
    Heading([i32, List<Node>]),                     // level, inline children
    Paragraph([List<Node>, Option<TaskMarker>]),
    BlockQuote(List<Node>),
    CodeBlock(CodeBlock),
    List(MdList),
    Item(MdItem),
    Table(MdTable),
    ThematicBreak,
    Html(String),                                   // raw block HTML, verbatim
    LinkReferenceDef(LinkReferenceDef),
    // Inline
    Text(String),
    Emphasis(List<Node>),
    Strong(List<Node>),
    Strikethrough(List<Node>),
    Code(String),
    InlineLink(LinkNode),
    ReferenceLink([List<Node>, String]),             // children, normalized label
    ShortcutLink([List<Node>, String]),
    AutoLink(String),
    InlineImage(ImageNode),
    ReferenceImage([String, String]),                // alt text, label
    SoftBreak,
    HardBreak,
}
```

(field structs elided; exact shape is an implementation detail). Two
deliberate departures from `render`'s model: `Text` nodes hold the **raw**
source substring, not an HTML-escaped one — the formatter never escapes,
since it emits Markdown, not HTML — and every list/table/link node retains
enough of the original (marker character, fence character count, column
alignment) to normalize rather than merely echo it.

### Parsing

Block scanning follows the same recursive, line-array structure as
`marl.wado`'s `render_blocks` (indices into a `List<String>` of lines, with
sub-ranges recursing for blockquotes and list items), extended with:

- **Front matter**: checked once, only at document offset 0 — a leading line
  of exactly `---` or `+++`, consumed verbatim through the matching closing
  delimiter on its own line. Matches pulldown-cmark's
  `ENABLE_YAML_STYLE_METADATA_BLOCKS` / `ENABLE_PLUSES_DELIMITED_METADATA_BLOCKS`
  rule (document-start only, not recognized mid-document).
- **Raw HTML blocks**: CommonMark's block-HTML start/end conditions (an
  opening tag from a fixed set of block-level tag names starting a line, an
  HTML comment, a processing instruction, a CDATA section, …), consumed
  verbatim to the corresponding end condition or a blank line. No HTML
  parsing beyond finding the boundary — the body is copied through
  unmodified, matching `gen_html`'s verbatim `gen_range`.
- **Indented code blocks**: kept in scope despite zero corpus occurrences
  (see Non-goals) because the CommonMark rule is small, self-contained, and
  independent of everything else (blank line before, 4-space indent, not a
  list-marker line) — cheap enough that skipping it would trade a real
  correctness gap for no measurable simplicity.
- **Link reference definitions**: collected in a first pass over the whole
  document (definitions can be forward-referenced, per CommonMark), keyed by
  a case-folded, whitespace-collapsed label, matching pulldown-cmark's
  resolution rule. A line starting `[^` is explicitly excluded from this scan
  — it is a footnote-definition-shaped construct, not a link reference
  definition, and must not be misparsed as one (see Non-goals: footnotes
  aren't fully supported, but must never be corrupted).

Inline parsing keeps `inline.wado`'s tokenize → delimiter-stack-resolve →
serialize shape, retargeted to emit AST nodes:

- Tokenize into text runs, delimiter runs (`*`/`_`/`~`), and "already
  resolved" constructs (code spans, autolinks, links, images), exactly as
  `inline.wado` already does.
- Extend link/image parsing to cover all of CommonMark's forms, not just
  inline `[text](url "title")`: reference (`[text][label]`), collapsed
  (`[label][]`), and shortcut (`[label]`, resolved against the label table
  from the first pass) — required by the corpus survey above. An unresolved
  shortcut candidate (brackets with no matching definition) falls back to
  literal bracket text, per CommonMark.
- Run the same delimiter-stack "process emphasis" procedure
  (`inline.wado::process_emphasis`, flanking rules + rule of three), but
  resolve into `Emphasis`/`Strong`/`Strikethrough` nodes instead of splicing
  HTML tag tokens.

### Printing

The generation algorithm mirrors `generate.rs`'s node-by-node walk
(`generate.rs::gen_nodes`), which is really a state machine over adjacent
sibling pairs deciding how much vertical space belongs between them:

- Between two block-level siblings: exactly one blank line, _unless_ both are
  list items and the source had none — list tightness (loose vs. tight) is
  the one place inter-block spacing is preserved from the source rather than
  normalized, via a `has_leading_blankline` check.
- Between inline siblings: a run of `\n` in the source becomes one hard
  newline in the output (not reflowed — see the `text_wrap: Maintain`
  finding above); a run of spaces collapses to one; adjacency rules around
  punctuation match `ends_with_punctuation`/`starts_with_punctuation`.
- Headings: ATX only (`heading_kind: Atx` is default and setext doesn't occur
  in the corpus), heading text forced onto one line (`with_no_new_lines`).
- Emphasis/strong: emitted as `_..._` / `**...**` (the `Underscores` /
  `Asterisks` defaults) — except when the _source_ used `*` and the character
  immediately after the closing delimiter is alphanumeric, in which case the
  asterisk is kept (`keep_asterisk` in `generate.rs`; this is a GitHub
  rendering-compatibility rule for `__word__something`, replicated verbatim).
- Lists: primary bullet `-` (alternate `*` only for adjacent same-kind lists
  the source didn't separate with a blank line — the "alternate list" case in
  `gen_nodes`), ordered lists renumber from their start index unless the
  first two items are both `1` (kept as `1.`/`1.` rather than renumbered —
  `is_all_ones_list`), and continuation-line indent equals marker width + 1
  space (`ListIndentKind::CommonMark`).
- Tables: column width is the max **display width** (see Unicode width
  below) of the header and every cell in that column; alignment colons come
  from the divider row; non-left/none alignment pads with computed leading
  spaces.
- Fenced code: fence character is always backtick; fence length is
  `max(2, longest run of consecutive backticks in the body) + 1`, so a body
  containing its own triple-backtick fence still round-trips.
- Link text: single line if its **UTF-8 byte length** is under
  `line_width / 2` (80 / 2 = 40 by default), else spread across the source's
  original line breaks — the specific byte-vs-width quirk noted above,
  replicated deliberately rather than "fixed."

### Unicode width

Table-column alignment is the one place display width — not byte length, not
codepoint count — determines the exact bytes emitted, so it is the one place
matching dprint means matching `unicode-width` 0.1.10 specifically, quirks
included, not a "more correct" modern width algorithm.

Given `unicode-width` 0.1.10's algorithm is `is_control ? 0 : (is_wide_or_fullwidth
? 2 : 1)` with no zero-width special case (confirmed by reading its
generated table, see Context), a faithful implementation only needs the East
Asian Width **Wide + Fullwidth** range set. Fetching `EastAsianWidth.txt`
(Unicode 15.0 — the version `unicode-width` 0.1.10 is generated from) and
merging adjacent ranges gives exactly **121 ranges** (59 in the BMP, 62
above it — mostly emoji blocks, plus a handful of rare historic scripts:
Tangut, Nüshu, CJK Extension B–I). That is small enough to encode in full
rather than hand-curate a "CJK / Latin-1 / emoji only" approximation: an
exact replica costs barely more than a partial one, so `unicode_width.wado`
ships the complete Wide+Fullwidth table rather than a lossy subset —
directly answering the brief's "decide the width ranges by hand" with a
byte-exact table instead of a guess.

```wado
pub fn char_display_width(c: char) -> i32 {
    let cp = c as i32;
    if cp < 0x20 { return 0; }        // C0 control (and NUL)
    if cp < 0x7F { return 1; }        // ASCII printable
    if cp < 0xA0 { return 0; }        // DEL + C1 control
    if is_wide(cp) { return 2; }      // binary search over the 121-range table
    return 1;                          // Narrow / Neutral / Ambiguous / Halfwidth
}

pub fn display_width(s: &String) -> i32 {
    let mut w = 0;
    for let c of s.chars() { w += char_display_width(c); }
    return w;
}
```

No grapheme clustering, no ZWJ/skin-tone-modifier sequence handling — matching
`unicode-width` 0.1.10, which also sums per-codepoint with no clustering.
Being "more correct" here would make output diverge from dprint, which is the
opposite of the goal.

This is a hand-rolled table rather than a call into the bundled-ICU data
pipeline ([WEP: Compile-Time Data Providers](./wep-2026-06-13-compile-time-data-providers.md)):
`core:text`'s properties component is not shipped yet (absent from the
current stdlib module list in `docs/cheatsheet.md`), so there's nothing to
call into today. If/when it ships an East Asian Width property, revisit —
the mismatch-with-dprint risk of a hand-rolled table is already fully mapped
by the differential test (see Testing), so there is no correctness reason to
block on it.

### Integration with wado-dev-tools

Confirmed feasible against this codebase's actual mechanisms, not by
analogy:

- `package-marl/wado.toml` already declares `lib = "src/lib.wado"`. `wado
  build --lib` (`wado-compiler/src/wit_emit.rs::wit_contract`) already
  synthesizes a minimal Component Model world from a package's `pub` exports
  — no `wasi:cli/command` or HTTP semantics, no compiler changes needed.
  Adding `format` next to the existing `render` export is enough; this was
  verified directly (`wado build --lib -o marl-lib.wasm` against
  `package-marl` compiles cleanly today).
- A Rust host loading a Wado-compiled component and calling one exported
  function with typed arguments is an established, production pattern in
  this codebase, not a new one: `wado-cli/src/kiln_runtime.rs` does exactly
  this for Kiln generators (`docs/wep-2026-04-12-kiln.md`: "generators are
  ordinary Wado packages compiled to components … executed by the host");
  `wado-compiler/tests/cm_catalog.rs` does the same dynamically
  (`get_typed_func`/`Val`-based marshaling) for test harnesses; `wado test`
  itself (`wado-cli/src/test.rs`) calls
  `instance.get_typed_func::<(), (Result<(), ()>,)>(...)` per test block.
  The same `get_typed_func::<(String,), (String,)>(&mut store,
  "format")` shape applies directly to `format(source: String) -> String`.
  `&String` at a Wado function boundary already lowers to WIT `string` at
  the CM boundary today (true of `render` already).
- wasmtime's **sync** component API (`Linker::instantiate`, `TypedFunc::call`)
  needs no tokio runtime, unlike the async API the CLI's WASI-import-heavy
  commands need — a good fit here, since a pure `format` has no WASI imports
  of substance. (One caveat: even a `--lib` component still imports
  `wasi:cli/stderr` for `assert`/`panic` diagnostics — needs a small
  host-side stub, or reuse of `wasmtime-wasi`, already a workspace
  dependency either way.) `wasmtime` is pinned at the workspace level
  (`=46.0.1`) and already a dependency of `wado-compiler`, which
  `wado-dev-tools` already depends on — no new crate.
- **Precompile and check in the `.wasm` artifact**, rebuilt by a mise task
  mirroring `update-bundled` (`mise.toml:478`, which does the same for
  `wado-bundled-libm`), rather than compiling `package-marl` fresh on every
  `mise run format`. Measured: `wado build --lib` on `package-marl` costs
  ~1.3–1.4 s wall, and that cost is **dominated by fixed per-process
  overhead** (stdlib snapshot construction, ~440 ms; monomorphize/lower/NIR
  optimize, ~350 ms) rather than input size — so it would not amortize away
  even as the package grows, and would eat most of the format command's
  budget before touching a single file. It would also couple `mise run
  format`'s availability to the compiler's correctness on every invocation,
  which is the wrong dependency direction for developer tooling that other
  tasks (`on-task-done`) depend on running reliably. A checked-in `.wasm`,
  instantiated once and called ~200 times (sync API), keeps the runtime
  format cost to the tens-of-milliseconds range.
- `wado-dev-tools/src/format_md.rs`'s existing Rust logic — CLI parsing,
  directory walk, exclusion list, `--check` semantics — is untouched. Only
  the innermost call (`format_text(&original, &config, …)` from
  `dprint_plugin_markdown`) is replaced with a call through the
  instantiated `format` export.

```
mise run update-marl-format-wasm   # wado build --lib package-marl -> checked-in .wasm
                                     # (mirrors mise.toml:478's update-bundled task)
```

### Testing — the actual definition of "100% compatible"

1. **Differential oracle.** While `dprint-plugin-markdown` remains available
   (moved to a dev-only dependency, not deleted yet), a test drives the same
   file-discovery walk `format_md.rs` uses today, runs _both_ formatters over
   every file, and asserts byte-identical output. This is what makes "100%
   compatible in this repo" a CI-checked fact rather than a one-time manual
   comparison — every future doc edit or new file gets re-verified for free
   for as long as the oracle dependency stays.
2. **Idempotency**, over the same corpus: `format(format(x)) == format(x)`,
   mirroring the existing invariant-testing pattern for `wado format`
   (`mise.toml`'s `test-format` task: "idempotency, AST round-trip, no
   comment drop" over the fixtures + stdlib corpus).
3. **Per-construct unit tests** in `fmt_*_test.wado`, one file per module
   (existing package-marl convention — see `marl_test.wado`), covering each
   node kind plus the specific edge cases the corpus survey surfaced:
   reference/collapsed/shortcut links (including a code span as link text),
   front matter, raw HTML blocks and inline HTML, wide-table cells
   (`char_display_width` against the exact `tc39-temporal.md` case worked
   out above), deeply nested mixed-marker ordered lists, and adjacent
   same-kind lists separated by no blank line (the "alternate marker" case).
4. **Cutover**, once the differential oracle is green across the whole
   corpus and stays green through a soak period of ordinary doc edits:
   `format-md` switches to the compiled Marl formatter by default,
   `dprint-plugin-markdown` is deleted from `Cargo.toml` entirely.

### Non-goals (this iteration)

- **Footnotes** (`[^id]` / `[^id]:`) and **inline/display math** (`$...$`):
  zero genuine occurrences in the corpus (see survey). Not fully formatted;
  a footnote-definition line is still recognized well enough to avoid being
  misparsed as a link reference definition (see Parsing), and falls through
  to verbatim block passthrough rather than pretty-printed reflow. Math is
  not specially recognized at all — `$...$` is ordinary text, which
  coincidentally matches dprint's own behavior for math anyway (its
  `gen_display_math`/`gen_inline_math` are verbatim passthrough, same as
  plain text would produce, modulo inter-node spacing edge cases). Promote
  either to first-class support the moment real usage appears — per
  `CLAUDE.md`, that would surface as a formatting bug on real content, which
  is P0.
- **Recursive code-block/front-matter reformatting**: not needed, since
  `wado-dev-tools`'s existing callback is a no-op today (see Context). If
  `format-md` ever starts recursively formatting fenced blocks, that's a
  separate, larger feature (an embedded-language formatter dispatch table),
  out of scope here.
- **Configurability**: no `wado.toml`-level formatting options. dprint's
  defaults are the only behavior this repo has ever used; a config surface
  is speculative until a second, differently-configured consumer exists.

## Consequences

### Positive

- One fewer external Rust dependency (`dprint-plugin-markdown` and its
  transitive `dprint-core`/`pulldown-cmark`/`regex` tree) in favor of Wado
  dogfooding its own compiler and Component Model tooling for a real,
  daily-used developer tool — matching Sheaf's and Kiln's precedent.
  `Cargo.toml`'s existing `[profile.dev.package.dprint-*]` opt-level
  workaround (for an upstream `dprint-core` panic on certain inputs) also
  disappears with the dependency.
- The differential-oracle test doubles as an unusually strong, continuously
  re-verified CommonMark/GFM conformance suite for Marl Format — most
  formatter test suites don't get a second independent implementation to
  diff against for free.
- `unicode_width.wado`'s exact East-Asian-Width table is independently
  reusable (table formatting, terminal-width alignment) well beyond Marl.

### Negative

- A hand-written parser covering full CommonMark + the GFM extensions this
  repo actually uses is real, substantial work — comparable in shape to
  `render`'s existing parser, but broader in grammar coverage (see Corpus
  survey) since it cannot rely on "unsupported constructs render as literal
  text" the way the HTML renderer does.
- Two independent Markdown parsers now live in `package-marl` (the renderer's
  and the formatter's). Accepted deliberately (see Architecture) rather than
  forcing a shared AST that would compromise one consumer or the other; the
  cost is some duplicated low-level scanning logic between `inline.wado` and
  `fmt_parse_inline.wado`.
- A precompiled `.wasm` artifact is a new category of checked-in generated
  file, needing the same discipline as `wado-bundled-libm`'s (`mise run
  update-marl-format-wasm`, CI-verified freshness) — one more thing that can
  go stale if a change to `package-marl` lands without regenerating it.

### Trade-offs

Precompiled artifact vs. compile-on-the-fly: covered under Integration above
— fixed ~1.3–1.4 s per-invocation compiler overhead (measured, not
estimated) makes on-the-fly compilation both slow and a bad dependency
direction for developer tooling; precompiling is the established pattern in
this codebase already.

Exact vs. approximate Unicode width table: the brief explicitly allows a
hardcoded approximation, but the exact Wide+Fullwidth table turned out to be
only 121 ranges — cheap enough that there was no real reason to under-cover
it once the ranges were in hand.

Separate AST vs. shared AST with the renderer: covered under Architecture —
the two consumers want incompatible things from the same input (discard vs.
preserve source fidelity), so sharing would have compromised one of them.

## Progress

### Phase 0: AST and parsing

- [ ] `fmt_ast.wado` — node types
- [ ] `fmt_parse_block.wado` — block scanner (front matter, headings,
      paragraphs, blockquotes, fenced + indented code, thematic breaks, lists,
      tables, raw HTML blocks, link reference definitions)
- [ ] `fmt_parse_inline.wado` — tokenizer + delimiter-stack resolution
- [ ] `fmt_links.wado` — inline/reference/collapsed/shortcut links and images,
      autolinks, two-pass reference-definition table

### Phase 1: printing

- [ ] `unicode_width.wado` — East Asian Width table + `char_display_width` /
      `display_width`
- [ ] `fmt_print.wado` — AST → canonical Markdown, mirroring `generate.rs`'s
      node-adjacency spacing rules
- [ ] `format.wado` — `pub fn format(source: &String) -> String`; re-exported
      from `lib.wado`

### Phase 2: integration

- [ ] `mise run update-marl-format-wasm` task (`wado build --lib
      package-marl` → checked-in `.wasm`, mirroring `update-bundled`)
- [ ] `wado-dev-tools`: sync wasmtime component instantiation + `format`
      `TypedFunc` call, replacing the `dprint_plugin_markdown::format_text`
      call in `format_md.rs`; `wasi:cli/stderr` host stub
- [ ] Differential-oracle test (both formatters, whole corpus, byte-identical)
- [ ] Idempotency test over the corpus

### Phase 3: cutover

- [ ] Differential oracle green across the full corpus
- [ ] `dprint-plugin-markdown` moved to a dev-only oracle dependency, then
      deleted once confidence holds
- [ ] `Cargo.toml`'s `[profile.dev.package.dprint-*]` workaround removed
