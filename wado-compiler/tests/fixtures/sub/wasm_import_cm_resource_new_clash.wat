;; A component whose resource has both a constructor and a static function named
;; `new`, which Wado would name alike.
(component
  (type $cell (resource (rep i32)))
  (core func $new (canon resource.new $cell))
  (func $ctor (param "v" u32) (result (own $cell))
    (canon lift (core func $new)))
  (component $shim
    (import "import-type-cell" (type $imported (sub resource)))
    (import "import-ctor" (func $ctor (param "v" u32) (result (own $imported))))
    (export $exported "cell" (type $imported))
    (export "[constructor]cell" (func $ctor)
      (func (param "v" u32) (result (own $exported))))
    (export "[static]cell.new" (func $ctor)
      (func (param "v" u32) (result (own $exported))))
  )
  (instance $c (instantiate $shim
    (with "import-type-cell" (type $cell))
    (with "import-ctor" (func $ctor))
  ))
  (export "wado:test/cells@0.1.0" (instance $c))
)
