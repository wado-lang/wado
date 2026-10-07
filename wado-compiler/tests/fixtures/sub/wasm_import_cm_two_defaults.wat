;; A component exporting two package default interfaces (`wado:left/left`,
;; `wado:right/right`) that share the name `pick`, and a world-level `twin`
;; that `left` shares.
(component
  (core module $m
    (func (export "left-pick") (result i32) i32.const 1)
    (func (export "right-pick") (result i32) i32.const 2)
    (func (export "only-left") (result i32) i32.const 3)
    (func (export "left-twin") (result i32) i32.const 20)
    (func (export "twin") (result i32) i32.const 10)
  )
  (core instance $i (instantiate $m))
  (func $left_pick (result u32) (canon lift (core func $i "left-pick")))
  (func $right_pick (result u32) (canon lift (core func $i "right-pick")))
  (func $only_left (result u32) (canon lift (core func $i "only-left")))
  (func $left_twin (result u32) (canon lift (core func $i "left-twin")))
  (func $twin (result u32) (canon lift (core func $i "twin")))
  (instance $left
    (export "pick" (func $left_pick))
    (export "only-left" (func $only_left))
    (export "twin" (func $left_twin)))
  (instance $right (export "pick" (func $right_pick)))
  (export "wado:left/left@0.1.0" (instance $left))
  (export "wado:right/right@0.1.0" (instance $right))
  (export "twin" (func $twin))
)
