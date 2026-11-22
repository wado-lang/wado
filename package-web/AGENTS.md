# Web Development Guide

`wado-lang:web` binds the browser's DOM to Wado, generated from WebIDL, and
serves the same bindings without a browser through `SurfaceDom`. The
`wado-package-standard` skill says where each kind of file goes.
[README.md](./README.md) is the user manual, and
[WEP: The Web Interface for Wado](../docs/wep-2026-04-01-web.md) the design.

## Architecture

`idl/dom.webidl.json` is the vendored WebIDL slice, as a webidl2 AST.
`wado-from-idl` generates two things from it: the bindings in `src/dom.wado`,
and the browser glue in `glue/dom.js`, one shim per member over the main
module's own signature. An object crosses as itself, so the glue keeps no
table. `SurfaceDom` is written by hand; each node lives in its handle, linked
to the others by theirs. `src/lib.wado` is the facade `wado-lang:web` names.

## Generated Files

- `mise run update-webidl-snapshot` refreshes `idl/dom.webidl.json`.
- `mise run update-package-web` regenerates `src/dom.wado` and `glue/dom.js`.
  Never edit either by hand.
- A name a wider slice generates must be added to `src/lib.wado`; a test fails
  until it is.
- `mise run test-web-glue` runs the glue on Node against jsdom
  (`glue/dom.test.mjs`).
