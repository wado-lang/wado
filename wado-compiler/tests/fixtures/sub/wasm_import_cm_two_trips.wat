;; A component exporting two interfaces named `trip`, from the packages `a:x`
;; and `b:y` (issue #2421).
(component
  (core module $m
    (func (export "f") (result i32) i32.const 1)
    (func (export "g") (result i32) i32.const 2)
  )
  (core instance $i (instantiate $m))
  (func $f (result u32) (canon lift (core func $i "f")))
  (func $g (result u32) (canon lift (core func $i "g")))
  (instance $x (export "f" (func $f)))
  (instance $y (export "g" (func $g)))
  (export "a:x/trip@0.1.0" (instance $x))
  (export "b:y/trip@0.1.0" (instance $y))
)
