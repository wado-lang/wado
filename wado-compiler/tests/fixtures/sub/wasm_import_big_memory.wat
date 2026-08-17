;; Wat module written against the component's memory, asking for 17 pages of
;; it. The component sizes its memory from what its assets need, imported
;; memories included, so the store below lands inside it.
(module
  (import "env" "memory" (memory 17))
  (func (export "high_store") (param i32)
    i32.const 1048576
    local.get 0
    i32.store)
  (func (export "high_load") (result i32)
    i32.const 1048576
    i32.load)
)
