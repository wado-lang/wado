;; A component exporting `a:x/trip`, whose `make` answers a `pt` of 2.
(component
  (core module $m (func (export "make") (result i32) i32.const 2))
  (core instance $i (instantiate $m))
  (type $pt (record (field "n" u32)))
  (type $make_ty (func (result $pt)))
  (func $make (type $make_ty) (canon lift (core func $i "make")))
  (component $shim
    (type $pt (record (field "n" u32)))
    (import "import-type-pt" (type $imported (eq $pt)))
    (import "import-func-make" (func $make (result $imported)))
    (export $exported "pt" (type $pt))
    (export "make" (func $make) (func (result $exported)))
  )
  (instance $c (instantiate $shim
    (with "import-func-make" (func $make))
    (with "import-type-pt" (type $pt))
  ))
  (export "a:x/trip@0.1.0" (instance $c))
)
