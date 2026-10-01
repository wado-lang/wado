(component
  (type $result-unit (;0;) (result))
  (core module $mem-mod (;0;)
    (type (;0;) (func (param i64)))
    (type (;1;) (func (param i32 i32 i32 i32) (result i32)))
    (memory (;0;) 1)
    (global (;0;) (mut i32) (i32.const 8))
    (export "realloc" (func $realloc))
    (export "memory" (memory 0))
    (func $grow_memory (;0;) (type 0) (param i64)
      (local i64)
      (local.set 1
        (i64.mul
          (i64.extend_i32_s
            (memory.size))
          (i64.const 65536)))
      (local.set 0
        (i64.sub
          (local.get 0)
          (local.get 1)))
      (local.set 1
        (select (result i64)
          (local.get 1)
          (i64.const 16777216)
          (i64.lt_u
            (local.get 1)
            (i64.const 16777216))))
      (@metadata.code.branch_hint "\00")
      (if ;; label = @1
        (if (result i32) ;; label = @1
          (i32.lt_s
            (memory.grow
              (i32.wrap_i64
                (i64.div_u
                  (i64.sub
                    (i64.add
                      (local.tee 1
                        (if (result i64) ;; label = @1
                          (i64.gt_u
                            (local.get 0)
                            (local.get 1))
                          (then
                            (i64.shl
                              (i64.const 1)
                              (i64.sub
                                (i64.const 64)
                                (i64.clz
                                  (i64.sub
                                    (local.get 0)
                                    (i64.const 1))))))
                          (else
                            (local.get 1))))
                      (i64.const 65536))
                    (i64.const 1))
                  (i64.const 65536))))
            (i32.const 0))
          (then
            (i32.lt_s
              (memory.grow
                (i32.wrap_i64
                  (i64.div_u
                    (i64.sub
                      (i64.add
                        (local.get 0)
                        (i64.const 65536))
                      (i64.const 1))
                    (i64.const 65536))))
              (i32.const 0)))
          (else
            (i32.const 0)))
        (then
          (unreachable)))
    )
    (func $realloc (;1;) (type 1) (param i32 i32 i32 i32) (result i32)
      (local i64 i64)
      (if ;; label = @1
        (i32.eqz
          (local.get 3))
        (then
          (if ;; label = @2
            (i32.eq
              (i32.add
                (local.get 0)
                (local.get 1))
              (global.get 0))
            (then
              (global.set 0
                (local.get 0))))
          (return
            (i32.const 0))))
      (local.set 4
        (i64.sub
          (i64.extend_i32_u
            (local.get 2))
          (i64.const 1)))
      (@metadata.code.branch_hint "\00")
      (if ;; label = @1
        (i64.gt_u
          (local.tee 5
            (i64.add
              (local.tee 4
                (i64.and
                  (i64.add
                    (i64.extend_i32_u
                      (global.get 0))
                    (local.get 4))
                  (i64.xor
                    (local.get 4)
                    (i64.const -1))))
              (i64.extend_i32_u
                (local.get 3))))
          (i64.const 4294967295))
        (then
          (unreachable)))
      (@metadata.code.branch_hint "\00")
      (if ;; label = @1
        (i64.gt_u
          (local.get 5)
          (i64.mul
            (i64.extend_i32_s
              (memory.size))
            (i64.const 65536)))
        (then
          (call $grow_memory
            (local.get 5))))
      (global.set 0
        (i32.wrap_i64
          (local.get 5)))
      (return
        (i32.wrap_i64
          (local.get 4)))
      (unreachable)
    )
  )
  (core instance $mem (;0;) (instantiate $mem-mod))
  (alias core export $mem "memory" (core memory $memory (;0;)))
  (alias core export $mem "realloc" (core func $realloc (;0;)))
  (type $stream-u8 (;1;) (stream u8))
  (core func $task.return (;1;) (canon task.return (result $result-unit) (memory $memory)))
  (core module $main-mod (;1;)
    (type (;0;) (func (param i32 i32 i32 i32) (result i32)))
    (type (;1;) (func))
    (type (;2;) (func (param i32)))
    (import "mem" "realloc" (func (;0;) (type 0)))
    (import "mem" "memory" (memory (;0;) 1))
    (import "wasi" "task-return" (func (;1;) (type 2)))
    (export "run" (func $ops.mess_dirty.wado/$cm_export__run))
    (func $ops.mess_dirty.wado/$cm_export__run (;2;) (type 1)
      (call 1
        (i32.const 0))
    )
  )
  (core instance $wasi-instance (;1;)
    (export "task-return" (func $task.return))
  )
  (core instance $mem-instance (;2;)
    (export "memory" (memory $memory))
    (export "realloc" (func $realloc))
  )
  (core instance $main (;3;) (instantiate $main-mod
      (with "wasi" (instance $wasi-instance))
      (with "mem" (instance $mem-instance))
    )
  )
  (alias core export $main "run" (core func $run-core (;2;)))
  (type $run-func-type (;2;) (func async (result $result-unit)))
  (func $run (;0;) (type $run-func-type) (canon lift (core func $run-core) async (memory $memory) (realloc $realloc)))
  (instance (;0;)
    (export "run" (func $run))
  )
  (export (;1;) "wasi:cli/run@0.3.0" (instance 0))
)
