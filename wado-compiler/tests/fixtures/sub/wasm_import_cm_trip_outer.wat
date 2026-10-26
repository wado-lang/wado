;; A component exporting two interfaces named `trip`, from the packages `a:x`
;; and `b:y` (issue #2421). `a:x/trip`'s record `outer` holds its record
;; `inner`, a type of its own interface.
;;
;; - `a:x/trip`'s `f` answers an `outer` whose `inner` has a reason of 5.
;; - `b:y/trip`'s `g` answers 2.
(component
  (core module $m
    (func (export "f") (result i32) i32.const 5)
    (func (export "g") (result i32) i32.const 2)
  )
  (core instance $i (instantiate $m))
  (type $inner_x (record (field "reason" u32)))
  (type $outer_x (record (field "inner" $inner_x)))
  (func $f (result $outer_x) (canon lift (core func $i "f")))
  (func $g (result u32) (canon lift (core func $i "g")))
  (component $shim_x
    (type $inner (record (field "reason" u32)))
    (import "import-type-inner" (type $i_inner (eq $inner)))
    (type $outer (record (field "inner" $i_inner)))
    (import "import-type-outer" (type $i_outer (eq $outer)))
    (import "import-func-f" (func $f (result $i_outer)))
    (type $e_inner_def (record (field "reason" u32)))
    (export $e_inner "inner" (type $e_inner_def))
    (type $e_outer_def (record (field "inner" $e_inner)))
    (export $e_outer "outer" (type $e_outer_def))
    (export "f" (func $f) (func (result $e_outer)))
  )
  (component $shim_y
    (import "import-func-g" (func $g (result u32)))
    (export "g" (func $g))
  )
  (instance $x (instantiate $shim_x
    (with "import-func-f" (func $f))
    (with "import-type-inner" (type $inner_x))
    (with "import-type-outer" (type $outer_x))
  ))
  (instance $y (instantiate $shim_y
    (with "import-func-g" (func $g))
  ))
  (export "a:x/trip@0.1.0" (instance $x))
  (export "b:y/trip@0.1.0" (instance $y))
)
