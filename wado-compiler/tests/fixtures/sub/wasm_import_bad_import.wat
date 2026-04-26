;; Wat module that imports something other than `env.memory`. Phase 1
;; only wires `env.memory` through to the embedded module and rejects any
;; other import.
(module
  (import "env" "table" (table 1 funcref))
  (func (export "noop"))
)
