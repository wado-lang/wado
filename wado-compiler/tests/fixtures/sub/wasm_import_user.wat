;; Tiny user-supplied .wat used by `wasm_import_user_wat.wado` to verify
;; that a Wado entry can name-import a wat asset's exports and call them
;; like ordinary functions. The exports are reached via TIR synthesis
;; (see WEP-2026-01-10-wasm-import.md).
(module
  (func (export "add_one") (param i32) (result i32)
    local.get 0
    i32.const 1
    i32.add)
  (func (export "twice") (param f64) (result f64)
    local.get 0
    local.get 0
    f64.add)
  (memory (export "memory") 1)
)
