# wado-from-idl

Generates Wado binding modules from IDL files: the stdlib from WIT, and
`package-web` from WebIDL.

## Generated Modules

- `wasi:*` — the WASI P3 bindings in `wado-compiler/lib/wasi/`, generated from
  wasmtime's WIT. Regenerate with `mise run update-stdlib-wasi`; it requires the
  `vendor/wasmtime` submodule.
- `core:kiln`, `core:eval` and `core:coverage` — the submodules under
  `wado-compiler/lib/core/kiln/`, `wado-compiler/lib/core/eval/` and
  `wado-compiler/lib/core/coverage/`. Regenerate with
  `mise run update-stdlib-core-wit`. The facades `lib/core/kiln.wado` and
  `lib/core/eval.wado` are hand-written and must be preserved; `core:coverage`
  has none, since only `core:rt` calls it.
- `wado-lang:web` — `package-web/src/dom.wado` and its browser glue
  `package-web/glue/dom.js`, generated from the WebIDL snapshot
  `package-web/idl/dom.webidl.json`. Regenerate with
  `mise run update-package-web`; `tests/web_dom_is_fresh.rs` fails when
  either is stale. The snapshot is the webidl2 AST of the slice
  `scripts/webidl/snapshot.mjs` takes from `@webref/idl`; widen the slice
  there and run `mise run update-webidl-snapshot`. See
  `docs/wep-2026-04-01-web.md`.

Never edit a generated file. Change this crate and regenerate.
