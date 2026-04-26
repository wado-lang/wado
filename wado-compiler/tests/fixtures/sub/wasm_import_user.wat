;; Tiny user-supplied .wat used by `wasm_import_user_wat.wado` (positive
;; e2e fixture) and `tests/wasm_import_dce.rs` (DCE check). Exports:
;;   - add_one / twice — exercised by the positive e2e fixture and by
;;     the DCE test (which only imports add_one)
;;   - unused_no_args / unused_squared — never imported from any Wado
;;     entry; the DCE test asserts they are pruned out of the embedded
;;     core module in the final component.
(module
  (func (export "add_one") (param i32) (result i32)
    local.get 0
    i32.const 1
    i32.add)
  (func (export "twice") (param f64) (result f64)
    local.get 0
    local.get 0
    f64.add)
  (func (export "unused_no_args") (result i32)
    i32.const 7)
  (func (export "unused_squared") (param i32) (result i32)
    local.get 0
    local.get 0
    i32.mul)
  (memory (export "memory") 1)
)
