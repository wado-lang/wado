;; Wat module with a `start` section. Phase 1 rejects start sections so
;; instantiation-time side effects can't sneak in through wasm imports.
(module
  (func $init)
  (start $init)
  (memory (export "memory") 1)
)
