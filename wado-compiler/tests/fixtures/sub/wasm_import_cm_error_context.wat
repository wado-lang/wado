;; A component whose export returns an `error-context`, which has no import mapping.
(component
  (core module $m
    (func (export "fail") (result i32)
      i32.const 0)
  )
  (core instance $i (instantiate $m))
  (func $fail (result error-context)
    (canon lift (core func $i "fail")))
  (instance $iface (export "fail" (func $fail)))
  (export "wado:test/fails@0.1.0" (instance $iface))
)
