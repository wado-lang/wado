;; A Component Model component written in the text format, so the loader has to
;; tell a component from a core module after parsing rather than before.
(component
  (core module $m
    (func (export "add") (param i32 i32) (result i32)
      local.get 0
      local.get 1
      i32.add)
  )
  (core instance $i (instantiate $m))
  (func $add (param "a" u32) (param "b" u32) (result u32)
    (canon lift (core func $i "add")))
  (instance $iface (export "add" (func $add)))
  (export "wado:test/math@0.1.0" (instance $iface))
)
