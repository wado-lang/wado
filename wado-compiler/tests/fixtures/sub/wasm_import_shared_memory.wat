;; Wat module with a shared memory. An embedded asset shares the component's
;; unshared memory, so Phase 1 rejects this shape with a diagnostic instead of
;; emitting a component that fails validation.
(module
  (memory 1 1 shared)
  (func (export "get") (result i32)
    i32.const 0
    i32.load)
)
