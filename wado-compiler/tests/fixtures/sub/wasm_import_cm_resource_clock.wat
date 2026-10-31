;; A component exporting a resource whose method reads the host's monotonic
;; clock, so calling into the resource requires what that import maps to.
(component
  (import "wasi:clocks/monotonic-clock@0.3.0" (instance $clock
    (export "now" (func (result u64)))))
  (alias export $clock "now" (func $now_fn))
  (core func $now_lowered (canon lower (func $now_fn)))
  (core instance $imports (export "now" (func $now_lowered)))
  (core module $m
    (import "host" "now" (func $now (result i64)))
    (func (export "read") (param i32) (result i32)
      call $now
      drop
      local.get 0))
  (core instance $i (instantiate $m (with "host" (instance $imports))))
  (type $ticker (resource (rep i32)))
  (core func $new (canon resource.new $ticker))
  (func $ctor (param "start" u32) (result (own $ticker))
    (canon lift (core func $new)))
  (func $read (param "self" (borrow $ticker)) (result u32)
    (canon lift (core func $i "read")))
  (component $shim
    (import "import-type-ticker" (type $imported (sub resource)))
    (import "import-ctor" (func $ctor (param "start" u32) (result (own $imported))))
    (import "import-read" (func $read (param "self" (borrow $imported)) (result u32)))
    (export $exported "ticker" (type $imported))
    (export "[constructor]ticker" (func $ctor)
      (func (param "start" u32) (result (own $exported))))
    (export "[method]ticker.read" (func $read)
      (func (param "self" (borrow $exported)) (result u32)))
  )
  (instance $c (instantiate $shim
    (with "import-type-ticker" (type $ticker))
    (with "import-ctor" (func $ctor))
    (with "import-read" (func $read))
  ))
  (export "wado:test/ticks@0.1.0" (instance $c))
)
