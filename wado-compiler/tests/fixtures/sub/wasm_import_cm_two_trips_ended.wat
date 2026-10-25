;; A component exporting two interfaces named `trip`, from the packages `a:x`
;; and `b:y`, each declaring a record `ended` and an enum `unit` (issue #2421).
;; `a:x/trip` also declares a variant `shape`.
;;
;; - `a:x/trip`'s `f` answers an `ended` of 1, `b:y/trip`'s `g` one of 2.
;; - `unit-code` answers 10 for `metric` and 11 for `imperial`, in either.
;; - `shape-size` answers a `circle`'s radius and 0 for a `dot`.
(component
  (core module $m
    (func (export "f") (result i32) i32.const 1)
    (func (export "g") (result i32) i32.const 2)
    (func (export "unit-code") (param i32) (result i32)
      local.get 0
      i32.const 10
      i32.add)
    (func (export "shape-size") (param i32 i32) (result i32)
      local.get 0
      if (result i32)
        i32.const 0
      else
        local.get 1
      end)
  )
  (core instance $i (instantiate $m))
  (type $ended_x (record (field "reason" u32)))
  (type $unit_x (enum "metric" "imperial"))
  (type $shape_x (variant (case "circle" u32) (case "dot")))
  (type $ended_y (record (field "reason" u32)))
  (type $unit_y (enum "metric" "imperial"))
  (func $f (result $ended_x) (canon lift (core func $i "f")))
  (func $unit_code_x (param "u" $unit_x) (result u32) (canon lift (core func $i "unit-code")))
  (func $shape_size (param "s" $shape_x) (result u32) (canon lift (core func $i "shape-size")))
  (func $g (result $ended_y) (canon lift (core func $i "g")))
  (func $unit_code_y (param "u" $unit_y) (result u32) (canon lift (core func $i "unit-code")))
  (component $shim_x
    (type $ended (record (field "reason" u32)))
    (type $unit (enum "metric" "imperial"))
    (type $shape (variant (case "circle" u32) (case "dot")))
    (import "import-type-ended" (type $i_ended (eq $ended)))
    (import "import-type-unit" (type $i_unit (eq $unit)))
    (import "import-type-shape" (type $i_shape (eq $shape)))
    (import "import-func-f" (func $f (result $i_ended)))
    (import "import-func-unit-code" (func $unit_code (param "u" $i_unit) (result u32)))
    (import "import-func-shape-size" (func $shape_size (param "s" $i_shape) (result u32)))
    (export $e_ended "ended" (type $ended))
    (export $e_unit "unit" (type $unit))
    (export $e_shape "shape" (type $shape))
    (export "f" (func $f) (func (result $e_ended)))
    (export "unit-code" (func $unit_code) (func (param "u" $e_unit) (result u32)))
    (export "shape-size" (func $shape_size) (func (param "s" $e_shape) (result u32)))
  )
  (component $shim_y
    (type $ended (record (field "reason" u32)))
    (type $unit (enum "metric" "imperial"))
    (import "import-type-ended" (type $i_ended (eq $ended)))
    (import "import-type-unit" (type $i_unit (eq $unit)))
    (import "import-func-g" (func $g (result $i_ended)))
    (import "import-func-unit-code" (func $unit_code (param "u" $i_unit) (result u32)))
    (export $e_ended "ended" (type $ended))
    (export $e_unit "unit" (type $unit))
    (export "g" (func $g) (func (result $e_ended)))
    (export "unit-code" (func $unit_code) (func (param "u" $e_unit) (result u32)))
  )
  (instance $x (instantiate $shim_x
    (with "import-func-f" (func $f))
    (with "import-func-unit-code" (func $unit_code_x))
    (with "import-func-shape-size" (func $shape_size))
    (with "import-type-ended" (type $ended_x))
    (with "import-type-unit" (type $unit_x))
    (with "import-type-shape" (type $shape_x))
  ))
  (instance $y (instantiate $shim_y
    (with "import-func-g" (func $g))
    (with "import-func-unit-code" (func $unit_code_y))
    (with "import-type-ended" (type $ended_y))
    (with "import-type-unit" (type $unit_y))
  ))
  (export "a:x/trip@0.1.0" (instance $x))
  (export "b:y/trip@0.1.0" (instance $y))
)
