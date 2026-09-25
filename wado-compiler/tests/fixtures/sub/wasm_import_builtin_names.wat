;; Exports named like `core:builtin` intrinsics, each doing something the
;; intrinsic does not. Used by `wasm_import_builtin_names.wado`.
(module
  (global $n (mut i32) (i32.const 0))
  (func (export "cold_path")
    (global.set $n (i32.add (global.get $n) (i32.const 1))))
  (func (export "count") (result i32)
    (global.get $n))
  (func (export "black_box") (param i32) (result i32)
    (i32.add (local.get 0) (i32.const 100)))
  (func (export "i32_clz") (param i32) (result i32)
    (local.get 0))
  (func (export "select") (param i32 i32 i32) (result i32)
    (i32.add (local.get 0) (i32.add (local.get 1) (local.get 2)))))
