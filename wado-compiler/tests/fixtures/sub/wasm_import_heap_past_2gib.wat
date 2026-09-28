;; Wat module whose memory is 64 KiB short of 2 GiB: the component's heap
;; starts where a signed 32-bit address turns negative.
(module
  (import "env" "memory" (memory 32767))
  (func (export "get") (result i32)
    i32.const 7)
)
