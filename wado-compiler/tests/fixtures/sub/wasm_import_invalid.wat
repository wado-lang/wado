;; Deliberately malformed WAT — `(func (export "x") this is not WAT)` —
;; used by `wasm_import_invalid_wat.wado` to verify the loader surfaces
;; `wat::parse_bytes` failures with a pointed compile-time error.
(module
  (func (export "x") this is not WAT)
)
