;; A component that imports a package's default interface (`wado:host/host`),
;; which its consumer provides as a guest effect, and exports one of its own.
(component
  (import "wado:host/host@0.1.0" (instance $host
    (export "get" (func (result u32)))))
  (alias export $host "get" (func $get))
  (core func $get_lowered (canon lower (func $get)))
  (core instance $host_core (export "get" (func $get_lowered)))
  (core module $m
    (import "host" "get" (func $get (result i32)))
    (func (export "twice") (result i32)
      call $get
      call $get
      i32.add)
  )
  (core instance $i (instantiate $m (with "host" (instance $host_core))))
  (func $twice (result u32) (canon lift (core func $i "twice")))
  (instance $calc (export "twice" (func $twice)))
  (export "wado:calc/calc@0.1.0" (instance $calc))
)
