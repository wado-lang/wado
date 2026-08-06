# wado-from-idl

Generates Wado stdlib modules from WIT files.

## Generated Modules

- `wasi:*` — the WASI P3 bindings in `wado-compiler/lib/wasi/`, generated from
  wasmtime's WIT. Regenerate with `mise run update-stdlib-wasi`; it requires the
  `vendor/wasmtime` submodule.
- `core:kiln` — the submodules under `wado-compiler/lib/core/kiln/`. Regenerate
  with `mise run update-stdlib-kiln`. The facade `lib/core/kiln.wado` is
  hand-written and must be preserved.

Never edit a generated `.wado` file. Change this crate and regenerate.
