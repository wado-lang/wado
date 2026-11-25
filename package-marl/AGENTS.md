# Marl Development Guide

Marl renders Markdown to HTML, and formats it back to canonical Markdown. The
`wado-package-standard` skill says where each kind of file goes.
[WEP: Marl](../docs/wep-2026-07-05-marl.md) holds the design and the open work.

## Compatibility

Marl is based on GFM, but does not aim at full compatibility. The goal is that
a GFM renderer displays what Marl accepts and prints without breaking it. Marl
leaves out on purpose the parts of CommonMark that are a liability, such as
setext headings and trailing-space hard breaks; the WEP lists them.

## Architecture

One parser, a block pass and an inline pass, builds one AST that keeps enough
of the source (markers, delimiters, table alignment) for the formatter to
normalize it. Two backends walk that tree: the HTML renderer and the Markdown
printer. The public API takes and returns values only, so `render` and
`format` lower onto a Component Model interface.

## Changing the Formatter

`mise run format-md` formats the repository's Markdown with Marl, and CI's
`tidy` job runs it. A change to what `format` prints therefore reformats the
repository: run `mise run format-md` and commit the result with the change.
`format(format(x)) == format(x)` must hold over that corpus.

`format` keeps meaning: `render(format(x)).html == render(x).html` for every
input. Every test of `format` or the printer asserts it, and the CLI leaves a
file that would break it unwritten and fails. A normalization the renderer
cannot see through is a bug in one of the two.
