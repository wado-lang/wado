;; A component exporting the world function `twin`, which answers 2.
(component
  (core module $m (func (export "twin") (result i32) i32.const 2))
  (core instance $i (instantiate $m))
  (func $f (result u32) (canon lift (core func $i "twin")))
  (export "twin" (func $f))
)
