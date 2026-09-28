;; Wat module whose memory starts at 2 GiB: the component's heap starts past
;; an asset's memory, and would start past what a 32-bit memory can address.
(module
  (import "env" "memory" (memory 32768))
  (func (export "get") (result i32)
    i32.const 7)
)
