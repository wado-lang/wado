# Marl Format — A Standalone Markdown Formatter CLI for Wado

## Context

`wado-dev-tools format-md` (invoked by `mise run format`) formats the repo's
Markdown by embedding the `dprint-plugin-markdown` Rust crate. Marl is
`package-marl`'s Markdown toolkit, written in Wado: a Markdown-to-HTML renderer
(`render`, used by [Sheaf](./wep-2026-07-05-sheaf.md)) and a
Markdown-to-Markdown formatter (`format`), shipped together as a
`wasi:cli/command` program — another instance of Wado dogfooding its own
toolchain, alongside Sheaf and Kiln.

The formatter's bar is "sufficiently reasonable Markdown output," not
byte-for-byte parity with dprint. dprint stays the design reference: it is
mature, and this repo's Markdown is already in its canonical form, which makes
it a convenient corpus for round-trip / idempotency testing. Whether Marl
Format ever replaces dprint in `mise run format` is out of scope here (see
Non-goals).

## Decision

### One parser, two backends

`render` and `format` share a single CommonMark + GFM parser
(`fmt_parse_block.wado` / `fmt_parse_inline.wado`) producing one AST
(`fmt_ast.wado`). The AST retains enough of the source — marker characters,
delimiter choices, table alignment — for the formatter to normalize rather
than merely echo it. Two backends walk the same tree:

- `fmt_html.wado` — AST to HTML (`render`).
- `fmt_print.wado` — AST to canonical Markdown (`format`).

`lib.wado` re-exports `render`, `format`, and the `escape_text` /
`escape_attr` helpers Sheaf's templates use.

### HTML output is safe by construction

`render` escapes `<`, `>`, `&`, and `"` in text; renders raw HTML — block and
inline — as escaped literal text, never passthrough; and scheme-filters
link / image / autolink destinations (`sanitize_url`: only `http`, `https`,
`mailto`, `tel`, and relative URLs survive; any other scheme becomes an empty
destination). Reference-style links resolve against the document's definition
table; link reference definitions and front matter produce no output.

### A standalone CLI

`package-marl/wado.toml` declares `[world]."wasi:cli/command" = "src/main.wado"`
alongside its `lib`, so the package stays importable as a library and is also
directly runnable. The CLI mirrors `format_md.rs`'s surface (positional paths,
`--check`) and `package-sheaf`'s WASI file I/O: walk the preopened tree for
`*.md` (skipping `vendor` / `target` / `.git` / `node_modules` / `.vscode-test`),
then read / format / compare / write. No Rust, no `wado-dev-tools` dependency,
no `mise` or CI wiring.

### Unicode width

Table-column alignment is the one place display width determines the exact
bytes emitted. `unicode_width.wado` ships the full East Asian Width
Wide+Fullwidth table (Unicode 15.0) plus a small zero-width set (combining
marks, variation selectors, ZWJ/ZWNJ); no grapheme clustering. The brief
allowed an approximation, but the exact table is small enough to ship whole.

### Testing

- Idempotency over the real corpus (`format(format(x)) == format(x)`) — the
  strongest cheap invariant, mirroring `wado format`'s existing invariant tests.
- Per-module unit tests, one `fmt_*_test.wado` per module (`fmt_html_test.wado`
  covers the renderer).
- A non-blocking reference diff against dprint (dev-time, not CI) to spot-check
  reasonableness against the already-canonical corpus.

### Non-goals (this iteration)

- Wiring into `wado-dev-tools` / `mise run format`, and removing the dprint
  dependency — a separate, later decision.
- Byte-for-byte dprint parity.
- Footnotes and `$…$` math (zero genuine corpus occurrences); a footnote-shaped
  line is only protected from being misparsed as a link reference definition.
- Recursive code-block / front-matter reformatting (dprint's callback no-ops too).
- Configurability (`wado.toml` formatting options).

## Consequences

- One parser and AST serve both `render` and `format`; there is no duplicated
  Markdown scanning.
- `render` handles the full grammar — reference links, raw HTML, indented code
  — where a subset renderer would treat them as literal text. Output stays safe
  by construction (escaping plus scheme filtering).
- A hand-written CommonMark + GFM parser is substantial work, but the
  already-canonical corpus makes the reference diff free and informative, and
  the width table is independently reusable.
- Running from source pays the usual fixed `wado run` startup each invocation —
  fine for an occasional standalone tool, a real cost only if it is ever wired
  into a hot path (another reason that is out of scope here).

## Progress

- [x] AST, block/inline parsers, links (`fmt_ast` / `fmt_parse_block` /
      `fmt_parse_inline` / `fmt_links`)
- [x] HTML renderer (`fmt_html`) and canonical printer (`fmt_print` /
      `unicode_width` / `format`), re-exported from `lib.wado`
- [x] Standalone CLI (`main.wado` + the `[world]` entry)
- [x] Idempotent across all tracked `.md` files in this repo; manual
      reference-diff against dprint reviewed as reasonable

## Known defects (deferred)

A recall-oriented review surfaced these CommonMark/GFM conformance defects. None
is triggered by this repo's corpus (the acceptance checks stay green), so they
are deferred — fix them if Marl is ever pointed at arbitrary Markdown.

- [ ] Paragraph-interruption rules are not enforced: a `[label]: url`-shaped
      line, a 4+-space-indented list marker or thematic break, or an ordered
      marker not starting at `1` each wrongly interrupts an open paragraph.
- [ ] Reference definitions nested in a blockquote or list item are never
      registered, so references to them do not resolve and stay literal text.
- [ ] A failed inline-link tail (`[text](…`) drops the link instead of falling
      back to reference/shortcut resolution.
- [ ] `![alt]` (shortcut reference image) is printed as `![alt][alt]` — there
      is no shortcut-image form.
- [ ] `write_file` truncates the target before the new content is confirmed
      written, risking a truncated file on a mid-write I/O failure.
