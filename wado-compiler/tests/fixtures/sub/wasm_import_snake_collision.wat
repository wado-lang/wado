;; Two exports that read as one snake_case name.
(module
  (func (export "a-b") (result i32) i32.const 1)
  (func (export "a_b") (result i32) i32.const 2))
