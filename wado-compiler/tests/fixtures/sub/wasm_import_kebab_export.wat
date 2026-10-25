;; Exports spelled in kebab-case and camelCase, which Wado reads in snake_case.
(module
  (func (export "add-one") (param i32) (result i32)
    local.get 0
    i32.const 1
    i32.add)
  (func (export "getValue") (result i32)
    i32.const 7))
