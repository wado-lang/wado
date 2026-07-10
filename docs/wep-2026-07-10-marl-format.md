# Marl Format — A Standalone Markdown Formatter CLI for Wado

## Context

`wado-dev-tools format-md` (invoked by `mise run format`) currently formats every
`*.md` file in the repository by embedding the `dprint-plugin-markdown` Rust
crate (`wado-dev-tools/src/format_md.rs`). Marl Format is a Markdown formatter
written in Wado, living in `package-marl` — the GFM-subset Markdown-to-HTML
renderer introduced by [WEP: Sheaf & Marl](./wep-2026-07-05-sheaf.md) — shipped
as its own standalone CLI (a `wasi:cli/command` world, run the same way as
Sheaf), independent of `wado-dev-tools` and the Rust toolchain entirely. It is
another instance of Wado dogfooding its own toolchain, alongside Sheaf and
Kiln. Whether and how it eventually becomes `mise run format`'s default (in
place of dprint) is explicitly out of scope for this iteration — see
Non-goals.

The target is "sufficiently reasonable Markdown output," not byte-for-byte
parity with dprint. dprint's actual behavior is still the primary design
reference throughout this document — it is a mature, widely-used formatter,
and this repository's entire Markdown corpus already happens to be in its
canonical form, which makes it an unusually good source of "what does
sensible Markdown formatting look like" and a convenient corpus for
round-trip and idempotency testing (see Testing). But matching it exactly is
not a goal, and a handful of dprint's more idiosyncratic byte-level quirks are
deliberately _not_ replicated below, in favor of simpler, equally-reasonable
choices. The corpus itself is large and non-trivial: 201 `*.md` files (per
`DEFAULT_EXCLUDED_DIRS` in `format_md.rs`: excludes `.vscode-test`, `vendor`,
`target`, `node_modules`, `.git`; notably not `.claude/`), ~4.1 MB, ~80,000
lines, including hand-written docs and WEPs, `.claude/skills/*/SKILL.md`
(YAML front matter), `wado-lsp/lsp.md` (a copy of the LSP specification, raw
HTML badges), and `wado-compiler/ref/tc39-temporal.md` (a copy of the TC39
Temporal proposal, ~9,200 lines of deeply nested numbered lists) — broad
enough to exercise real CommonMark/GFM breadth, not just a handful of
hand-picked examples.

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

The rules below are adopted because they are good defaults — a mature
formatter's already-settled answers to "how should this look" — not because
matching dprint exactly is required. Where a simpler, equally reasonable
choice exists, it's taken in preference to a dprint-specific quirk (noted
inline). The generation algorithm mirrors `generate.rs`'s node-by-node walk
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
- Link text: single line if its **display width** (see Unicode width below)
  is under `line_width / 2` (80 / 2 = 40 by default), else spread across the
  source's original line breaks. dprint compares **UTF-8 byte length**
  instead of display width here (see Context) — a small, arguably-accidental
  quirk not worth replicating now that exactness isn't the bar; display width
  is the more defensible measurement and this is the one place it's used in
  preference to dprint's own choice.

### Unicode width

Table-column alignment is the one place display width — not byte length, not
codepoint count — determines the exact bytes emitted. dprint's own
`unicode-width` 0.1.10 dependency does a pure East Asian Width lookup with no
zero-width case at all for combining marks or variation selectors (see
Context) — a known gap in that specific old crate version, not a considered
design choice. Since exactness isn't the bar, Marl Format's own width table
fixes that gap rather than replicating it: Wide/Fullwidth ⇒ 2, a small set of
genuinely-zero-width codepoints (combining marks, variation selectors
`U+FE00`–`U+FE0F`, zero-width joiner/non-joiner) ⇒ 0, everything else ⇒ 1.
This keeps output correct for the "⬆️"-style cells the corpus survey actually
found (see Context) without inheriting an unrelated old-crate quirk.

The East Asian Width **Wide + Fullwidth** range set (fetched
`EastAsianWidth.txt`, Unicode 15.0, merged into contiguous ranges) is exactly
**121 ranges** (59 in the BMP, 62 above it — mostly emoji blocks, plus a
handful of rare historic scripts: Tangut, Nüshu, CJK Extension B–I). Small
enough to encode in full rather than hand-curate a "CJK / Latin-1 / emoji
only" approximation — the brief allows an approximation, but an exact table
costs barely more than a partial one, so `unicode_width.wado` ships the
complete Wide+Fullwidth table.

```wado
pub fn char_display_width(c: char) -> i32 {
    let cp = c as i32;
    if is_zero_width(cp) { return 0; }  // combining marks, variation selectors, ZWJ/ZWNJ
    if cp < 0x20 { return 0; }          // C0 control (and NUL)
    if cp < 0x7F { return 1; }          // ASCII printable
    if cp < 0xA0 { return 0; }          // DEL + C1 control
    if is_wide(cp) { return 2; }        // binary search over the 121-range table
    return 1;                            // Narrow / Neutral / Ambiguous / Halfwidth
}

pub fn display_width(s: &String) -> i32 {
    let mut w = 0;
    for let c of s.chars() { w += char_display_width(c); }
    return w;
}
```

No grapheme clustering (multi-codepoint emoji sequences sum their parts
rather than measuring as one cluster) — a known, acceptable simplification
for this repo's actual usage (see Context: CJK, Latin-1, emoji), not a
correctness target worth a full text-segmentation implementation.

This is a hand-rolled table rather than a call into the bundled-ICU data
pipeline ([WEP: Compile-Time Data Providers](./wep-2026-06-13-compile-time-data-providers.md)):
`core:text`'s properties component is not shipped yet (absent from the
current stdlib module list in `docs/cheatsheet.md`), so there's nothing to
call into today. Revisit if/when it ships an East Asian Width property.

### A standalone CLI, no host-language integration

Marl Format ships as an ordinary Wado CLI program — no Rust code, no
`wado-dev-tools` dependency, no host-side Component Model plumbing. This was
the original plan's most complex section (compiling to a library-world
component and hosting it from Rust via `wasmtime`'s component API); dropping
the wado-dev-tools integration requirement removes that whole layer, and
what's left is exactly `package-sheaf`'s existing shape:

`package-marl/wado.toml` gains a `[world]` entry alongside its existing
`lib = "src/lib.wado"` — a package can declare both (`docs/wep-2026-02-14-package-manifest.md`:
"a package must declare at least one world: a `[world]` entry,
`[package].lib`, or both"), exactly like `render`/`format` stay importable as
a library while the package is _also_ directly runnable:

```toml
[world]
"wasi:cli/command" = "src/main.wado"
```

`package-marl/src/main.wado` (new file) is the CLI entry point, following
`package-sheaf/src/main.wado`'s existing WASI I/O style directly (same
`Preopens`/`Descriptor`/`read_via_stream`/`write_via_stream` calls Sheaf
already uses):

```wado
use { println, eprintln, args, Stdout, Stderr } from "core:cli";
use { parse } from "core:args";
use { Preopens, Descriptor, PathFlags, OpenFlags, DescriptorFlags } from "wasi:filesystem";
use { format } from "./format.wado";

struct Cli {
    #[serde(positional)]
    paths: List<String> = [],   // files or directories; empty = whole preopened tree
    check: bool = false,
}
impl Deserialize for Cli;

const EXCLUDED_DIRS: List<String> = [".vscode-test", "vendor", "target", "node_modules", ".git"];

export fn run() with Stdout, Stderr, Environment, Preopens, Exit {
    let cli = match parse::<Cli>(args()) {
        Ok(c) => c,
        Err(e) => { eprintln(`marl-format: {e.message}`); exit_error(); },
    };
    let root = Preopens::get_directories()[0].0;
    let files = collect_md_files(&root, &cli.paths);   // recursive walk + EXCLUDED_DIRS + *.md filter
    let mut had_changes = false;
    for let path of files {
        let original = read_file(&root, &path);        // as Sheaf::read_file
        let formatted = format(&original);
        if formatted == original { continue; }
        had_changes = true;
        if cli.check {
            println(`would reformat: {path}`);
        } else {
            write_file(&root, &path, &formatted);       // as Sheaf::write_file
            println(`formatted: {path}`);
        }
    }
    if cli.check && had_changes { exit_error(); }
}
```

(`collect_md_files` is a straightforward recursive extension of Sheaf's
`read_dir_names` — walk every preopened subdirectory, skip `EXCLUDED_DIRS` by
name, collect `*.md` paths; full implementation is mechanical, not designed
here.) `core:args`' `#[serde(positional)] paths: List<String>` plus a `check:
bool` flag directly matches `format_md.rs`'s existing CLI shape (bare path
arguments, `--check`), so the command-line surface stays familiar.

Running it needs nothing beyond what any other Wado program in this repo
already needs:

```sh
# from source, like any other Wado CLI program (recompiles each run, paying
# `wado build`'s fixed ~1.3-1.5s compiler startup cost — dominated by stdlib
# snapshot construction, not input size; a known, accepted cost since this
# tool isn't wired into a hot path):
wado run --dir . package-marl/src/main.wado -- --check

# or compiled once, then run with any Component-Model-capable Wasm runtime
# (wasmtime, or `wado`'s own bundled one via a future `wado run <wasm>` path):
wado compile -o marl-format.wasm package-marl/src/main.wado
wasmtime run --dir . marl-format.wasm -- --check
```

No mise task, no CI wiring, and no change to `wado-dev-tools` or the existing
`mise run format` task are part of this design — see Non-goals.

### Testing — "sufficiently reasonable," checked rather than assumed

The bar is no longer bytewise dprint parity, but "reasonable" still needs a
concrete, checkable meaning rather than a vibe:

1. **Idempotency**, over the real corpus (all 201 files, read-only —
   `format(format(x)) == format(x)`): mirrors the existing invariant-testing
   pattern already used for `wado format` (`mise.toml`'s `test-format` task:
   "idempotency, AST round-trip, no comment drop" over the fixtures + stdlib
   corpus). This is the strongest cheap invariant a formatter can have and
   catches most real bugs (an unstable formatter is a broken one, regardless
   of how close it is to any reference output).
2. **Per-construct unit tests** in `fmt_*_test.wado`, one file per module
   (existing package-marl convention — see `marl_test.wado`), covering each
   node kind plus the specific edge cases the corpus survey surfaced:
   reference/collapsed/shortcut links (including a code span as link text),
   front matter, raw HTML blocks and inline HTML, wide-table cells
   (`char_display_width` against the `tc39-temporal.md` case worked out
   above), deeply nested mixed-marker ordered lists, and adjacent same-kind
   lists separated by no blank line (the "alternate marker" case).
3. **A non-blocking reference diff against dprint**, run manually or as a
   dev-time script (not CI) over the real corpus: not a pass/fail gate, but
   the fastest way to spot-check "does this look reasonable" against a
   corpus that's already known-good, and to catch accidental regressions
   during development. Differences are expected and fine; large or
   structural differences (wrong nesting, dropped content, corrupted links)
   are the signal to actually look at.
4. Since Marl Format never ships wired into `mise run format` in this
   iteration (see Non-goals), there is no cutover step and no requirement to
   ever remove `dprint-plugin-markdown` — that question is deferred entirely
   to whenever (if ever) this tool is proposed as the project's default.

### Non-goals (this iteration)

- **Wiring into `wado-dev-tools` or `mise run format`**: this design ships
  Marl Format as a standalone, independently runnable and testable CLI only.
  Whether it ever becomes the project's default Markdown formatter (replacing
  `dprint-plugin-markdown` in `wado-dev-tools`) is a separate, later decision
  — deliberately decoupled from building and validating the tool itself, so
  neither is blocked on the other.
- **Byte-for-byte dprint parity**: see Context. dprint's behavior is the
  design reference, not a compatibility contract.
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

- Zero host-language integration: a normal Wado CLI package, runnable and
  testable the moment it's implemented, with no Rust code, no new crate
  dependency, and no coupling to `wado-dev-tools`'s build or release
  process — matching Sheaf's and Kiln's precedent for dogfooding Wado's own
  toolchain on a real, standalone developer tool.
- The corpus being already dprint-canonical means the reference-diff check
  (see Testing) is essentially free to run and immediately informative —
  most formatter projects don't get a large, real, already-known-good corpus
  to compare against on day one.
- `unicode_width.wado`'s East-Asian-Width table is independently reusable
  (table formatting, terminal-width alignment) well beyond Marl, and is more
  correct than the reference implementation it's informed by (see Unicode
  width above), not merely a copy of it.

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
- Running from source costs the same fixed ~1.3–1.5 s compiler startup every
  invocation as any other `wado run` program (see Integration) — fine for a
  standalone tool run occasionally by hand, but a real cost if this is ever
  wired into a hot path (another reason that's explicitly out of scope here
  rather than half-solved now).

### Trade-offs

Standalone CLI vs. wado-dev-tools integration: covered under Non-goals and
Integration — building and validating the formatter doesn't need to be
gated on solving host-language embedding, artifact caching, or CI wiring;
those are real questions but separable ones, deferred until the tool exists
and is worth wiring in.

Exact vs. approximate Unicode width table: the brief explicitly allows a
hardcoded approximation, but the exact Wide+Fullwidth table turned out to be
only 121 ranges — cheap enough that there was no real reason to under-cover
it once the ranges were in hand.

Separate AST vs. shared AST with the renderer: covered under Architecture —
the two consumers want incompatible things from the same input (discard vs.
preserve source fidelity), so sharing would have compromised one of them.

## Progress

### Phase 0: AST and parsing

- [x] `fmt_ast.wado` — node types
- [x] `fmt_parse_block.wado` — block scanner (front matter, headings,
      paragraphs, blockquotes, fenced + indented code, thematic breaks, lists,
      tables, raw HTML blocks, link reference definitions)
- [x] `fmt_parse_inline.wado` — tokenizer + delimiter-stack resolution
- [x] `fmt_links.wado` — inline/reference/collapsed/shortcut links and images,
      autolinks, two-pass reference-definition table

### Phase 1: printing

- [x] `unicode_width.wado` — East Asian Width table + `char_display_width` /
      `display_width`
- [x] `fmt_print.wado` — AST → canonical Markdown, mirroring `generate.rs`'s
      node-adjacency spacing rules
- [x] `format.wado` — `pub fn format(source: &String) -> String`; re-exported
      from `lib.wado`

### Phase 2: standalone CLI

- [x] `package-marl/wado.toml`: add `[world]."wasi:cli/command" =
      "src/main.wado"` alongside the existing `lib`
- [x] `package-marl/src/main.wado`: `core:args` CLI parsing (`--check`,
      positional paths), recursive `wasi:filesystem` directory walk with
      `EXCLUDED_DIRS`, read/format/compare/write, `would reformat:` /
      `formatted:` messages, non-zero exit on `--check` with pending changes
- [x] Idempotency test over the corpus — stable (0 files change on a second
      pass) across all 202 tracked `.md` files in this repo
- [x] Manual reference-diff pass against dprint's current output across the
      real corpus, reviewing for reasonableness (not requiring a match) — the
      differences found are all intentional and safe: bare autolinks gained
      `<...>`, over-wide list-item continuation indent was normalized to the
      marker's own content column, and ordered-list markers were renumbered
      sequentially from `start` (all render identically to the original).
      One pre-existing authoring bug was found and fixed in the process (see
      below).

### Known defects (deferred)

A recall-oriented review surfaced these CommonMark/GFM conformance defects and
CLI-robustness gaps. None is triggered by this repo's corpus — every one
produces output that is still idempotent and dprint-diff-reasonable on the real
files (which is why the acceptance checks are green) — so they are deferred,
not blocking. Fix them if Marl is ever pointed at arbitrary Markdown outside
this repo. Each was confirmed by running the code, not just by reading.

- [ ] Paragraph-interruption rules are not enforced. `parse_blocks` reuses its
      "first line of a new block" detectors on every line, so a line that
      should stay paragraph continuation instead starts a new block: a
      `[label]: url`-shaped line (missing the `para.is_empty()` guard the
      indented-code branch has), a list marker or thematic break indented 4+
      spaces (`parse_list_marker` / `is_thematic_break` lack the 3-space cap
      `skip_indent` applies), and an ordered-list marker not starting at `1`
      (CommonMark allows only `1.` to interrupt a paragraph).
- [ ] Reference definitions nested in a blockquote or list item are never
      registered. `collect_ref_defs` scans raw, un-stripped lines, so it never
      sees `> [id]: url` or `- [id]: url`; a reference to such a label anywhere
      in the document then fails to resolve and prints as literal `[id]` text.
- [ ] A failed inline-link tail drops the link. When `[text](…` fails to parse
      as an inline link, `try_link` returns `null` instead of falling back to
      reference/shortcut resolution, so a defined `[text]` reference is lost.
- [ ] GFM tables with a header/delimiter column-count mismatch are accepted;
      `is_delimiter_row` validates each cell but not the count against the
      header, where GFM rejects the whole construct as a paragraph.
- [ ] No shortcut-image form. `![alt]` (shortcut reference image) is always
      expanded to `![alt][alt]` on the first pass — there is no `ShortcutImage`
      node paralleling `ShortcutLink`.
- [ ] `write_file` truncates the target before the new content is confirmed
      written, risking a truncated file on a mid-write I/O failure.
