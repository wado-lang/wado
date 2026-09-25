;; Exports named like the allocator's `realloc` and the bundled libm's
;; `libm_sin`. Used by `wasm_import_shared_export_names.wado`.
(module
  (func (export "realloc") (param i32 i32 i32 i32) (result i32)
    (i32.const 7))
  (func (export "libm_sin") (param f64) (result f64)
    (f64.const 42)))
