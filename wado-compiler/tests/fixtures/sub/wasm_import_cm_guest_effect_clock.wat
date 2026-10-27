;; A component that imports a guest effect (`test:clk/tag`) and a WASI
;; interface (`wasi:clocks/monotonic-clock`), so a consumer answering the guest
;; effect with a handler forwards its own clock import to it.
(component
  (import "test:clk/tag@0.1.0" (instance $tag
    (export "tag" (func (result u32)))))
  (import "wasi:clocks/monotonic-clock@0.3.0" (instance $clock
    (export "now" (func (result u64)))))
  (alias export $tag "tag" (func $tag_fn))
  (alias export $clock "now" (func $now_fn))
  (core func $tag_lowered (canon lower (func $tag_fn)))
  (core func $now_lowered (canon lower (func $now_fn)))
  (core instance $imports
    (export "tag" (func $tag_lowered))
    (export "now" (func $now_lowered)))
  (core module $m
    (import "host" "tag" (func $tag (result i32)))
    (import "host" "now" (func $now (result i64)))
    (func (export "stamp") (result i32)
      call $now
      drop
      call $tag
      i32.const 1
      i32.add)
  )
  (core instance $i (instantiate $m (with "host" (instance $imports))))
  (func $stamp (result u32) (canon lift (core func $i "stamp")))
  (instance $clk (export "stamp" (func $stamp)))
  (export "test:clk/clk@0.1.0" (instance $clk))
)
