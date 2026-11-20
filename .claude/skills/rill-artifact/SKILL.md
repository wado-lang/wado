---
name: rill-artifact
description: Publish a Rill app as a Claude artifact, a private page on claude.ai that runs the app hydrated in the viewer's browser. Use when asked to share, publish or demo a Rill app (package-rill's to-do example, or another) as an artifact.
---

# Publishing a Rill App as an Artifact

An artifact is a static page with files beside it, which is all a Rill app
needs once it is built as a static site
([`package-rill/static-generation.md`](../../../package-rill/static-generation.md)).
The page is the initial HTML, and `app.js` with the core Wasm modules hydrates
it. Nothing about the app changes: only the page takes another shape.

`package-rill/example/todo` is the worked example. Its artifact is
<https://claude.ai/artifact/QoNF24bTGgwN5v26KbYNxP>.

## What the artifact asks of the page

- The artifact wraps the file in a document of its own, with the charset and
  a viewport meta. So the file holds the head's other children and the body's
  children, with no `<html>`, `<head>` or `<body>` of its own, and no meta of
  its own but `claude:viewport-fit-sides`.
- The page follows the viewer's theme. Each color is a token on `:root`,
  redefined under `@media (prefers-color-scheme: dark)` guarded by
  `:root:not([data-theme="light"])`, and again under `:root[data-theme="dark"]`.
  `body` sets its own background.
- The side gutter is `max(16px, env(safe-area-inset-left, 0px))` on each side,
  which is what the `claude:viewport-fit-sides` meta promises.
- A script loads from the artifact's own files by a relative URL. Each core
  Wasm module goes up with `contentType: "application/wasm"`.

The to-do example's `src/style.css` meets all of these, so the same
stylesheet serves both the site and the artifact.

## Build the page

The site build renders the page twice from one `render(script)`, which builds
it into the current document: once whole, as `index.html`, and once as the
artifact takes it, as `artifact.html` beside the build's intermediates. See
`artifact_page` in `package-rill/example/todo/site.wado`.

```sh
mise run rill-todo-site
```

It writes:

- `build/rill-todo/` — `index.html`, `app.js` and the `*.core*.wasm` modules.
- `build/rill-todo-work/artifact.html` — the page the artifact publishes.

Another app copies the shape: a `render(script)` that builds the page, a
browser program that calls `mount` with `hydrate` on the root it finds by id,
and a site build that writes both pages.

## Publish

Publish with the Artifact tool: `file_path` is `artifact.html`, and `files`
maps `app.js` and every core module the build wrote
(`ls build/rill-todo/*.core*.wasm`) to the files in `build/rill-todo/`, each
module with its content type. A module left out fails to load, and the page
never hydrates.

```json
{
  "file_path": "build/rill-todo-work/artifact.html",
  "files": {
    "app.js": "build/rill-todo/app.js",
    "main.component.core.wasm": {
      "from": "build/rill-todo/main.component.core.wasm",
      "contentType": "application/wasm"
    },
    "main.component.core2.wasm": {
      "from": "build/rill-todo/main.component.core2.wasm",
      "contentType": "application/wasm"
    },
    "main.component.core3.wasm": {
      "from": "build/rill-todo/main.component.core3.wasm",
      "contentType": "application/wasm"
    }
  },
  "icon": "checklist"
}
```

To update an artifact published in another conversation, pass its URL as
`url`, after reading it with `action: "read"`. A publish from a new path
without `url` makes a separate artifact.

## Check

Open the artifact and add an item. A row that appears means the program
hydrated the page. The page shows before the Wasm loads either way, so a
page that looks right proves nothing.

The viewer's browser needs Wasm GC and JSPI, as Chrome 137 and Safari 27 have.
A browser missing either shows the page as served, with no bindings.
