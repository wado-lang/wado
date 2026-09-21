;; A component export taking no parameters. The loader mints `#[cm_params()]`
;; for it, so the attribute schema has to admit an empty argument list.
(component
  (core module $m
    (func (export "answer") (result i32) i32.const 42)
  )
  (core instance $i (instantiate $m))
  (func $answer (result u32) (canon lift (core func $i "answer")))
  (instance $iface (export "answer" (func $answer)))
  (export "wado:test/oracle@0.1.0" (instance $iface))
)
