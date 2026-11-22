# Marl Development Guide

Marl renders a CommonMark subset with the GFM extensions to HTML, and formats
it back to canonical Markdown. The `wado-package-standard` skill says where
each kind of file goes. [WEP: Marl](../docs/wep-2026-07-05-marl.md) holds the
design and the open work.

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
