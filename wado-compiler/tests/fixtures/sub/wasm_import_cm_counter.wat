;; A component whose default interface `wado:counter/counter` declares a type of
;; its basename, `counter`.
(component
  (core module $m
    (func (export "make") (result i32) i32.const 7)
  )
  (core instance $i (instantiate $m))
  (type $counter (record (field "n" u32)))
  (type $make_ty (func (result $counter)))
  (func $make (type $make_ty) (canon lift (core func $i "make")))
  (component $shim
    (type $counter (record (field "n" u32)))
    (import "import-type-counter" (type $imported (eq $counter)))
    (import "import-func-make" (func $make (result $imported)))
    (export $exported "counter" (type $counter))
    (export "make" (func $make) (func (result $exported)))
  )
  (instance $c (instantiate $shim
    (with "import-func-make" (func $make))
    (with "import-type-counter" (type $counter))
  ))
  (export "wado:counter/counter@0.1.0" (instance $c))
)
