;; A component whose guest effect (`test:res/store`) declares a resource, which
;; no handler can provide.
(component
  (import "test:res/store@0.1.0" (instance $store
    (export "entry" (type (sub resource)))
    (export "count" (func (result u32)))))
  (alias export $store "count" (func $count_fn))
  (core func $count_lowered (canon lower (func $count_fn)))
  (core instance $imports (export "count" (func $count_lowered)))
  (core module $m
    (import "host" "count" (func $count (result i32)))
    (func (export "total") (result i32)
      call $count))
  (core instance $i (instantiate $m (with "host" (instance $imports))))
  (func $total (result u32) (canon lift (core func $i "total")))
  (instance $use (export "total" (func $total)))
  (export "test:res/use@0.1.0" (instance $use))
)
