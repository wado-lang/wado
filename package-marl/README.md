# Marl — Markdown Renderer and Formatter

`wado-lang:marl` reads a subset of CommonMark with the GFM extensions, and
writes it back two ways: as safe HTML, and as canonical Markdown. The
repository's own Markdown is formatted with it.
[WEP: Marl](../docs/wep-2026-07-05-marl.md) holds the design, the syntax it
covers, and the work ahead.

## Library

```wado
use { render, format, fenced_code_blocks } from "wado-lang:marl";

let page = render("# Title\n\nSome *text*.\n");
page.html;       // "<h1 id=\"title\">Title</h1>\n<p>Some <em>text</em>.</p>"
page.headings;   // the outline: level, GFM slug, and text of each heading

let tidy = format("*  item\n");                       // "- item\n"
let blocks = fenced_code_blocks("```sh\nls\n```\n");  // each fence's language and body
```

- `render` escapes raw HTML as text and drops a link whose scheme is not
  `http`, `https`, `mailto` or `tel`, so untrusted Markdown cannot inject
  markup or an executable link.
- `render_highlighted` hands each fenced code block, and each HTML comment, to
  a `RenderHook` handler, and inserts what it returns as it is.
- `escape_text` and `escape_attr` escape text for an HTML template.

## CLI

```sh
wado run package-marl/src/main.wado [paths...]           # format in place
wado run package-marl/src/main.wado --check [paths...]   # fail on a change
```

With no path it walks the preopened directory for `*.md`, skipping `vendor`,
`target`, `.git`, `node_modules`, `.vscode-test` and `scratchpad`.
`mise run format-md` runs it over the repository.
