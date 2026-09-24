# wado-from-idl

Generates Wado binding modules from IDL files: the stdlib from WIT, and
`package-web` from WebIDL.

## Generated Modules

- `wasi:*` — the WASI P3 bindings in `wado-compiler/lib/wasi/`, generated from
  wasmtime's WIT. Regenerate with `mise run update-stdlib-wasi`; it requires the
  `vendor/wasmtime` submodule.
- `core:kiln` — the submodules under `wado-compiler/lib/core/kiln/`. Regenerate
  with `mise run update-stdlib-kiln`. The facade `lib/core/kiln.wado` is
  hand-written and must be preserved.
- `web:dom` — `package-web/src/dom.wado` and its browser glue
  `package-web/glue/dom.js`, generated from the WebIDL snapshot
  `package-web/idl/dom.webidl.json`. Regenerate with
  `mise run update-package-web`; `tests/web_dom_is_fresh.rs` fails when
  either is stale. The snapshot is the webidl2 AST of the slice
  `scripts/webidl/snapshot.mjs` takes from `@webref/idl`; widen the slice
  there and run `mise run update-webidl-snapshot`. See
  `docs/wep-2026-04-01-tide.md`.

Never edit a generated `.wado` file. Change this crate and regenerate.
