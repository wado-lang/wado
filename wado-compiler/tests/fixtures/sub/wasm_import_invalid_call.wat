;; Wat module that assembles but does not validate: the call names a function
;; index the module does not have. The loader must report it rather than let
;; codegen walk an index space that isn't there.
(module
  (memory 1)
  (func (export "f") (result i32)
    (call 99))
)
