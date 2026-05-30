(component
  (type $types-instance-type (;0;)
    (instance
      (type (;0;) (enum "io" "illegal-byte-sequence" "pipe"))
      (export (;1;) "error-code" (type (eq 0)))
    )
  )
  (import "wasi:cli/types@0.3.0-rc-2026-03-15" (instance $wasi:cli/types@0.3.0-rc-2026-03-15 (;0;) (type $types-instance-type)))
  (alias export $wasi:cli/types@0.3.0-rc-2026-03-15 "error-code" (type $error-code (;1;)))
  (type $stdin-instance-type (;2;)
    (instance
      (type (;0;) (stream u8))
      (alias outer 1 $error-code (type (;1;)))
      (type (;2;) (result (error 1)))
      (type (;3;) (future 2))
      (type (;4;) (tuple 0 3))
      (type (;5;) (func (result 4)))
      (export (;0;) "read-via-stream" (func (type 5)))
    )
  )
  (import "wasi:cli/stdin@0.3.0-rc-2026-03-15" (instance $wasi:cli/stdin@0.3.0-rc-2026-03-15 (;1;) (type $stdin-instance-type)))
  (alias export $wasi:cli/stdin@0.3.0-rc-2026-03-15 "read-via-stream" (func $read-via-stream (;0;)))
  (type $result-unit (;3;) (result))
  (core module $mem-mod (;0;)
    (type (;0;) (func (param i32)))
    (type (;1;) (func (param i32 i32 i32 i32) (result i32)))
    (memory (;0;) 1)
    (global (;0;) (mut i32) (i32.const 8))
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
  (type $stream-u8 (;4;) (stream u8))
  (type $cli-transmission-result (;5;) (result (error $error-code)))
  (type $cli-transmission-future (;6;) (future $cli-transmission-result))
  (core func $task.return (;1;) (canon task.return (result $result-unit) (memory $memory)))
  (core func $stream.drop-readable (;2;) (canon stream.drop-readable $stream-u8))
  (core func $future.drop-readable (;3;) (canon future.drop-readable $cli-transmission-future))
  (core func $wasi:cli/Stdin::read_via_stream (;4;) (canon lower (func $read-via-stream) (memory $memory) (realloc $realloc)))
  (core module $main-mod (;1;)
    (rec
      (type (;0;) (sub (struct (field (mut i32)) (field (mut i32)))))
    )
    (type (;1;) (func (param i32 i32 i32 i32) (result i32)))
    (type (;2;) (func (param i32)))
    (type (;3;) (func (param i32)))
    (type (;4;) (func (result (ref 0))))
    (type (;5;) (func))
    (type (;6;) (func (param i32)))
    (type (;7;) (func (param i32)))
    (import "mem" "realloc" (func (;0;) (type 1)))
    (import "wasi" "task-return" (func (;1;) (type 2)))
    (import "wasi" "wasi:cli/Stdin::read_via_stream" (func (;2;) (type 3)))
    (import "mem" "memory" (memory (;0;) 1))
    (import "wasi" "stream-drop-readable" (func (;3;) (type 6)))
    (import "wasi" "future-drop-readable:transmission-cli" (func (;4;) (type 7)))
    (export "run" (func $/tmp/leak_read_via_stream.wado/__cm_export__run))
    (func $/tmp/leak_read_via_stream.wado/__cm_binding__Stdin_read_via_stream (;5;) (type 4) (result (ref 0))
      (local i32 i32 i32)
      (local.set 0
        (call 0
          (i32.const 0)
          (i32.const 0)
          (i32.const 4)
          (i32.const 8)))
      (call 2
        (local.get 0))
      (local.set 1
        (i32.load
          (i32.add
            (local.get 0)
            (i32.const 0))))
      (local.set 2
        (i32.load
          (i32.add
            (local.get 0)
            (i32.const 4))))
      (drop
        (call 0
          (local.get 0)
          (i32.const 8)
          (i32.const 4)
          (i32.const 0)))
      (return
        (struct.new 0
          (local.get 1)
          (local.get 2)))
      (unreachable)
    )
    (func $/tmp/leak_read_via_stream.wado/__cm_export__run (;6;) (type 5)
      (local i32 i32 (ref null 0))
      (local.set 2
        (call $/tmp/leak_read_via_stream.wado/__cm_binding__Stdin_read_via_stream))
      (local.set 0
        (struct.get 0 0
          (local.get 2)))
      (local.set 1
        (struct.get 0 1
          (local.get 2)))
      (call 3
        (local.get 0))
      (call 4
        (local.get 1))
      (call 1
        (i32.const 0))
    )
  )
  (core instance $wasi-instance (;1;)
    (export "task-return" (func $task.return))
    (export "stream-drop-readable" (func $stream.drop-readable))
    (export "future-drop-readable:transmission-cli" (func $future.drop-readable))
    (export "wasi:cli/Stdin::read_via_stream" (func $wasi:cli/Stdin::read_via_stream))
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
  (alias core export $main "run" (core func $run-core (;5;)))
  (type $run-func-type (;7;) (func async (result $result-unit)))
  (func $run (;1;) (type $run-func-type) (canon lift (core func $run-core) async (memory $memory) (realloc $realloc)))
  (export $"#func2 run" (@name "run") (;2;) "run" (func $run))
)
