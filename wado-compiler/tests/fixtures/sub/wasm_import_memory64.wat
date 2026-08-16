;; Wat module with a 64-bit memory. An embedded asset shares the component's
;; 32-bit memory, so Phase 1 rejects this shape with a diagnostic instead of
;; emitting a component that fails validation.
(module
  (memory i64 1)
  (func (export "get") (result i32)
    i64.const 0
    i32.load)
)
