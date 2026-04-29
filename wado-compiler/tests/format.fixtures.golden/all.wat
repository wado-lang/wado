(component
  (type $types-instance-type (;0;)
    (instance
      (type (;0;) (enum "io" "illegal-byte-sequence" "pipe"))
      (export (;1;) "error-code" (type (eq 0)))
    )
  )
  (import "wasi:cli/types@0.3.0-rc-2026-03-15" (instance $wasi:cli/types@0.3.0-rc-2026-03-15 (;0;) (type $types-instance-type)))
  (alias export $wasi:cli/types@0.3.0-rc-2026-03-15 "error-code" (type $error-code (;1;)))
  (type $result-unit (;2;) (result))
  (core module $mem-mod (;0;)
    (type (;0;) (func (param i32)))
    (type (;1;) (func (param i32 i32 i32 i32) (result i32)))
    (memory (;0;) 1)
    (global (;0;) (mut i32) (i32.const 8))
    (global (;1;) (mut i32) (i32.const 0))
    (export "realloc" (func $realloc))
    (export "memory" (memory 0))
    (func $grow_memory (;0;) (type 0) (param i32)
      (local i32 i32 i32 i32 i32 i32)
      (local.set 1
        (i32.mul
          (memory.size)
          (i32.const 65536)))
      (local.set 2
        (i32.sub
          (local.get 0)
          (local.get 1)))
      (local.set 4
        (select (result i32)
          (local.get 1)
          (i32.const 16777216)
          (i32.lt_s
            (local.get 1)
            (i32.const 16777216))))
      (local.set 5
        (if (result i32) ;; label = @1
          (i32.gt_s
            (local.get 2)
            (local.get 4))
          (then
            (i32.shl
              (i32.const 1)
              (i32.sub
                (i32.const 32)
                (i32.clz
                  (i32.sub
                    (local.get 2)
                    (i32.const 1))))))
          (else
            (local.get 4))))
      (local.set 6
        (i32.div_s
          (i32.add
            (local.get 5)
            (i32.const 65535))
          (i32.const 65536)))
      (drop
        (memory.grow
          (local.get 6)))
    )
    (func $realloc (;1;) (type 1) (param i32 i32 i32 i32) (result i32)
      (local i32 i32)
      (if ;; label = @1
        (i32.eq
          (local.get 3)
          (i32.const 0))
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
        (i32.and
          (i32.sub
            (i32.add
              (global.get 0)
              (local.get 2))
            (i32.const 1))
          (i32.sub
            (i32.const 0)
            (local.get 2))))
      (local.set 5
        (i32.add
          (local.get 4)
          (local.get 3)))
      (if ;; label = @1
        (i32.gt_s
          (local.get 5)
          (i32.mul
            (memory.size)
            (i32.const 65536)))
        (@metadata.code.branch_hint "\00")
        (then
          (call $grow_memory
            (local.get 5))))
      (global.set 0
        (local.get 5))
      (return
        (local.get 4))
      (unreachable)
    )
  )
  (core instance $mem (;0;) (instantiate $mem-mod))
  (alias core export $mem "memory" (core memory $memory (;0;)))
  (alias core export $mem "realloc" (core func $realloc (;0;)))
  (type $stream-u8 (;3;) (stream u8))
  (core func $task.return (;1;) (canon task.return (result $result-unit) (memory $memory)))
  (core module $main-mod (;1;)
    (type (;0;) (func (param i32 i32 i32 i32) (result i32)))
    (type (;1;) (func (param i32)))
    (type (;2;) (func))
    (import "mem" "realloc" (func (;0;) (type 0)))
    (import "wasi" "task-return" (func (;1;) (type 1)))
    (import "mem" "memory" (memory (;0;) 1))
    (export "run" (func $wado-compiler/tests/format.fixtures/all.dirty.wado/__cm_export__run))
    (func $wado-compiler/tests/format.fixtures/all.dirty.wado/__cm_export__run (;2;) (type 2)
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
  (type $run-func-type (;4;) (func async (result $result-unit)))
  (func $run (;0;) (type $run-func-type) (canon lift (core func $run-core) async (memory $memory) (realloc $realloc)))
  (export $"#func1 run" (@name "run") (;1;) "run" (func $run))
)
