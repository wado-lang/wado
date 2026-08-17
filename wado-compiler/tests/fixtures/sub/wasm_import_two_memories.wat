;; Wat module that both imports `env.memory` and defines one of its own. Only
;; one memory can be wired to the component's, so Phase 1 counts both.
(module
  (import "env" "memory" (memory 1))
  (memory 1)
  (func (export "get") (result i32)
    i32.const 0
    i32.load)
)
