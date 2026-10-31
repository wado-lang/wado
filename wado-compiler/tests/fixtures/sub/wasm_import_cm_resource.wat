;; A component whose interface `wado:test/counters` exports a resource with a
;; destructor, a constructor, a method taking the handle by borrow, and a static
;; function, and `take`, which takes a handle by `own` and keeps its
;; representation without running the destructor. `drops` counts what the
;; destructor ran, so a consumer can observe that its handles were dropped.
(component
  (core module $m
    (global $drops (mut i32) (i32.const 0))
    (func (export "dtor") (param i32)
      global.get $drops
      i32.const 1
      i32.add
      global.set $drops)
    (func (export "get") (param i32) (result i32)
      local.get 0)
    (func (export "drops") (result i32)
      global.get $drops)
  )
  (core instance $i (instantiate $m))
  (alias core export $i "dtor" (core func $dtor))
  (type $counter (resource (rep i32) (dtor (core func $dtor))))
  (core func $new (canon resource.new $counter))
  (func $ctor (param "start" u32) (result (own $counter))
    (canon lift (core func $new)))
  (func $get (param "self" (borrow $counter)) (result u32)
    (canon lift (core func $i "get")))
  ;; An `own` handle reaches its defining component as a handle into that
  ;; component's own table, so `take` reads the representation through it.
  (core func $rep (canon resource.rep $counter))
  (core instance $rep_i (export "rep" (func $rep)))
  (core module $t
    (import "" "rep" (func $rep (param i32) (result i32)))
    (func (export "take") (param i32) (result i32)
      local.get 0
      call $rep))
  (core instance $ti (instantiate $t (with "" (instance $rep_i))))
  (func $take (param "c" (own $counter)) (result u32)
    (canon lift (core func $ti "take")))
  (func $drops (result u32)
    (canon lift (core func $i "drops")))
  (component $shim
    (import "import-type-counter" (type $imported (sub resource)))
    (import "import-ctor" (func $ctor (param "start" u32) (result (own $imported))))
    (import "import-get" (func $get (param "self" (borrow $imported)) (result u32)))
    (import "import-from" (func $from (param "start" u32) (result (own $imported))))
    (import "import-take" (func $take (param "c" (own $imported)) (result u32)))
    (import "import-drops" (func $drops (result u32)))
    (export $exported "counter" (type $imported))
    (export "[constructor]counter" (func $ctor)
      (func (param "start" u32) (result (own $exported))))
    (export "[method]counter.get" (func $get)
      (func (param "self" (borrow $exported)) (result u32)))
    (export "[static]counter.from" (func $from)
      (func (param "start" u32) (result (own $exported))))
    (export "take" (func $take) (func (param "c" (own $exported)) (result u32)))
    (export "drops" (func $drops) (func (result u32)))
  )
  (instance $c (instantiate $shim
    (with "import-type-counter" (type $counter))
    (with "import-ctor" (func $ctor))
    (with "import-get" (func $get))
    (with "import-from" (func $ctor))
    (with "import-take" (func $take))
    (with "import-drops" (func $drops))
  ))
  (export "wado:test/counters@0.1.0" (instance $c))
)
