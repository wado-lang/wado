;; A component exporting the world function `other`, which answers 3.
(component
  (core module $m (func (export "other") (result i32) i32.const 3))
  (core instance $i (instantiate $m))
  (func $f (result u32) (canon lift (core func $i "other")))
  (export "other" (func $f))
)
