# Marl Format — A Standalone Markdown Formatter CLI for Wado

## Context

`wado-dev-tools format-md` (invoked by `mise run format`) formats the repo's
Markdown by embedding the `dprint-plugin-markdown` Rust crate. Marl Format is a
Markdown formatter written in Wado, living in `package-marl` alongside the
GFM-subset renderer from [WEP: Sheaf & Marl](./wep-2026-07-05-sheaf.md), shipped
as its own `wasi:cli/command` program — another instance of Wado dogfooding its
own toolchain, alongside Sheaf and Kiln.

The bar is "sufficiently reasonable Markdown output," not byte-for-byte parity
with dprint. dprint is still the design reference: it is mature, and this repo's
Markdown is already in its canonical form, which makes it a convenient corpus
for round-trip / idempotency testing. Whether Marl Format ever replaces dprint
in `mise run format` is out of scope here (see Non-goals).

## Decision

### A real parser, not a GFM subset

The existing renderer is deliberately a GFM _subset_ — reference links, raw
HTML, and the like render as literal text. That is right for a blog renderer but
wrong for a formatter, which must round-trip the repo's actual prose. Marl
Format implements a materially more complete CommonMark + GFM grammar.

### A separate AST and pipeline

The renderer goes straight from source to HTML, discarding exact spelling. A
formatter needs the opposite: it must retain enough of the original (marker
characters, spacing, alignment) to normalize rather than merely echo it. So Marl
Format has its own AST and pipeline (`fmt_*.wado`), package-local, sharing only
the `StrCursor` utility and not touching the renderer's `marl.wado` /
`inline.wado`. `lib.wado` re-exports `format` alongside `render`.

### A standalone CLI

`package-marl/wado.toml` declares `[world]."wasi:cli/command" = "src/main.wado"`
alongside its `lib`, so the package stays importable as a library and is also
directly runnable. The CLI mirrors `format_md.rs`'s surface (positional paths,
`--check`) and `package-sheaf`'s WASI file I/O: walk the preopened tree for
`*.md` (skipping `vendor` / `target` / `.git` / `node_modules` / `.vscode-test`),
then read / format / compare / write. No Rust, no `wado-dev-tools` dependency, no
`mise` or CI wiring.

### Unicode width

Table-column alignment is the one place display width determines the exact bytes
emitted. `unicode_width.wado` ships the full East Asian Width Wide+Fullwidth
table (Unicode 15.0) plus a small zero-width set (combining marks, variation
selectors, ZWJ/ZWNJ); no grapheme clustering. The brief allowed an
approximation, but the exact table is small enough to ship whole.

### Testing

- Idempotency over the real corpus (`format(format(x)) == format(x)`) — the
  strongest cheap invariant, mirroring `wado format`'s existing invariant tests.
- Per-construct unit tests, one `fmt_*_test.wado` per module.
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

- A normal Wado CLI package — runnable and testable immediately, with no Rust,
  no new crate dependency, and no coupling to `wado-dev-tools`. The
  already-canonical corpus makes the reference diff free and informative, and
  the width table is independently reusable.
- A hand-written CommonMark + GFM parser is substantial work, and `package-marl`
  now holds two Markdown parsers (the renderer's and the formatter's) with some
  duplicated low-level scanning — accepted rather than forcing a shared AST that
  would compromise one consumer or the other.
- Running from source pays the usual fixed `wado run` startup each invocation —
  fine for an occasional standalone tool, a real cost only if it is ever wired
  into a hot path (another reason that is out of scope here).

## Progress

- [x] AST, block/inline parsers, links (`fmt_ast` / `fmt_parse_block` /
      `fmt_parse_inline` / `fmt_links`)
- [x] Printer + Unicode width + entry point (`fmt_print` / `unicode_width` /
      `format`, re-exported from `lib.wado`)
- [x] Standalone CLI (`main.wado` + the `[world]` entry)
- [x] Idempotent across all tracked `.md` files in this repo; manual
      reference-diff against dprint reviewed as reasonable. One pre-existing
      authoring bug was found and fixed in the process.

## Known defects (deferred)

A recall-oriented review surfaced these CommonMark/GFM conformance defects. None
is triggered by this repo's corpus (the acceptance checks stay green), so they
are deferred — fix them if Marl is ever pointed at arbitrary Markdown.

- [ ] Paragraph-interruption rules are not enforced: a `[label]: url`-shaped
      line, a 4+-space-indented list marker or thematic break, or an ordered
      marker not starting at `1` each wrongly interrupts an open paragraph.
- [ ] Reference definitions nested in a blockquote or list item are never
      registered, so references to them print as literal `[id]` text.
- [ ] A failed inline-link tail (`[text](…`) drops the link instead of falling
      back to reference/shortcut resolution.
- [ ] `![alt]` (shortcut reference image) is expanded to `![alt][alt]` — there
      is no shortcut-image form.
- [ ] `write_file` truncates the target before the new content is confirmed
      written, risking a truncated file on a mid-write I/O failure.
