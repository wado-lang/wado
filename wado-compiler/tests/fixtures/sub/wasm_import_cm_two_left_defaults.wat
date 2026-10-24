;; A component exporting two package default interfaces of one basename
;; (`wado:left/left`, `acme:left/left`) that share the name `pick`.
(component
  (core module $m
    (func (export "wado-pick") (result i32) i32.const 1)
    (func (export "acme-pick") (result i32) i32.const 2)
  )
  (core instance $i (instantiate $m))
  (func $wado_pick (result u32) (canon lift (core func $i "wado-pick")))
  (func $acme_pick (result u32) (canon lift (core func $i "acme-pick")))
  (instance $wado (export "pick" (func $wado_pick)))
  (instance $acme (export "pick" (func $acme_pick)))
  (export "wado:left/left@0.1.0" (instance $wado))
  (export "acme:left/left@0.1.0" (instance $acme))
)
