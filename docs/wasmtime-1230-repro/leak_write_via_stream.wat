(component
  (type $types-instance-type (;0;)
    (instance
      (type (;0;) (enum "io" "illegal-byte-sequence" "pipe"))
      (export (;1;) "error-code" (type (eq 0)))
    )
  )
  (import "wasi:cli/types@0.3.0-rc-2026-03-15" (instance $wasi:cli/types@0.3.0-rc-2026-03-15 (;0;) (type $types-instance-type)))
  (alias export $wasi:cli/types@0.3.0-rc-2026-03-15 "error-code" (type $error-code (;1;)))
  (type $stdout-instance-type (;2;)
    (instance
      (type (;0;) (stream u8))
      (alias outer 1 $error-code (type (;1;)))
      (type (;2;) (result (error 1)))
      (type (;3;) (future 2))
      (type (;4;) (func (param "data" 0) (result 3)))
      (export (;0;) "write-via-stream" (func (type 4)))
    )
  )
  (import "wasi:cli/stdout@0.3.0-rc-2026-03-15" (instance $wasi:cli/stdout@0.3.0-rc-2026-03-15 (;1;) (type $stdout-instance-type)))
  (alias export $wasi:cli/stdout@0.3.0-rc-2026-03-15 "write-via-stream" (func $write-via-stream (;0;)))
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
  (core func $stream.write (;1;) (canon stream.write $stream-u8 (memory $memory) (realloc $realloc)))
  (core func $task.return (;2;) (canon task.return (result $result-unit) (memory $memory)))
  (core func $waitable.join (;3;) (canon waitable.join))
  (core func $waitable-set.drop (;4;) (canon waitable-set.drop))
  (core func $waitable-set.new (;5;) (canon waitable-set.new))
  (core func $waitable-set.wait (;6;) (canon waitable-set.wait (memory $memory)))
  (core func $stream.new (;7;) (canon stream.new $stream-u8))
  (core func $stream.drop-writable (;8;) (canon stream.drop-writable $stream-u8))
  (core func $future.drop-readable (;9;) (canon future.drop-readable $cli-transmission-future))
  (core func $wasi:cli/Stdout::write_via_stream (;10;) (canon lower (func $write-via-stream) (memory $memory) (realloc $realloc)))
  (core module $main-mod (;1;)
    (rec
      (type (;0;) (array (mut i8)))
      (type (;1;) (sub (struct (field (mut (ref 0))) (field (mut i32)))))
    )
    (type (;2;) (func (param i32 i32 i32 i32) (result i32)))
    (type (;3;) (func (param i32 i32 i32) (result i32)))
    (type (;4;) (func (param i32)))
    (type (;5;) (func (param i32 i32)))
    (type (;6;) (func (param i32)))
    (type (;7;) (func (result i32)))
    (type (;8;) (func (param i32 i32) (result i32)))
    (type (;9;) (func (param i32) (result i32)))
    (type (;10;) (func))
    (type (;11;) (func (param i32) (result i32)))
    (type (;12;) (func))
    (type (;13;) (func (param (ref 0) i32 i32)))
    (type (;14;) (func (param (ref 0) i32) (result i64)))
    (type (;15;) (func (param i32) (result i32)))
    (type (;16;) (func (param i32 (ref 1))))
    (type (;17;) (func (result i64)))
    (type (;18;) (func (param i32)))
    (type (;19;) (func (param i32)))
    (import "mem" "realloc" (func (;0;) (type 2)))
    (import "wasi" "stream-write" (func (;1;) (type 3)))
    (import "wasi" "task-return" (func (;2;) (type 4)))
    (import "wasi" "waitable-join" (func (;3;) (type 5)))
    (import "wasi" "waitable-set-drop" (func (;4;) (type 6)))
    (import "wasi" "waitable-set-new" (func (;5;) (type 7)))
    (import "wasi" "waitable-set-wait" (func (;6;) (type 8)))
    (import "wasi" "wasi:cli/Stdout::write_via_stream" (func (;7;) (type 9)))
    (import "mem" "memory" (memory (;0;) 1))
    (import "wasi" "stream-new" (func (;8;) (type 17)))
    (import "wasi" "stream-drop-writable" (func (;9;) (type 18)))
    (import "wasi" "future-drop-readable:transmission-cli" (func (;10;) (type 19)))
    (export "run" (func $/tmp/leak_write_via_stream.wado/__cm_export__run))
    (func $/tmp/leak_write_via_stream.wado/run (;11;) (type 10)
      (local i32 (ref null 1) i64 i32 i32)
      (local.set 2
        (call 8))
      (local.set 3
        (i32.wrap_i64
          (local.get 2))
        (local.set 4
          (i32.wrap_i64
            (i64.shr_u
              (local.get 2)
              (i64.const 32)))))
      (local.set 0
        (call $/tmp/leak_write_via_stream.wado/__cm_binding__Stdout_write_via_stream
          (local.get 3)))
      (local.set 1
        (struct.new 1
          (array.new_fixed 0 3
            (i32.const 104)
            (i32.const 105)
            (i32.const 10))
          (i32.const 3)))
      (call $core:internal/cm_stream_write_u8
        (local.get 4)
        (ref.as_non_null
          (local.get 1)))
      (call 9
        (local.get 4))
      (call 10
        (local.get 0))
    )
    (func $/tmp/leak_write_via_stream.wado/__cm_binding__Stdout_write_via_stream (;12;) (type 11) (param i32) (result i32)
      (return
        (call 7
          (local.get 0)))
      (unreachable)
    )
    (func $/tmp/leak_write_via_stream.wado/__cm_export__run (;13;) (type 12)
      (call $/tmp/leak_write_via_stream.wado/run)
      (call 2
        (i32.const 0))
    )
    (func $core:internal/gc_array_to_memory (;14;) (type 13) (param (ref 0) i32 i32)
      (local i32)
      (local.set 3
        (i32.const 0))
      (block ;; label = @1
        (loop ;; label = @2
          (if ;; label = @3
            (i32.ge_s
              (local.get 3)
              (local.get 2))
            (then
              (br 2 (;@1;))))
          (i32.store8
            (i32.add
              (local.get 1)
              (local.get 3))
            (array.get_u 0
              (local.get 0)
              (local.get 3)))
          (local.set 3
            (i32.add
              (local.get 3)
              (i32.const 1)))
          (br 0 (;@2;))))
    )
    (func $core:internal/cm_lower_list_u8 (;15;) (type 14) (param (ref 0) i32) (result i64)
      (local i32)
      (local.set 2
        (call 0
          (i32.const 0)
          (i32.const 0)
          (i32.const 1)
          (local.get 1)))
      (call $core:internal/gc_array_to_memory
        (local.get 0)
        (local.get 2)
        (local.get 1))
      (return
        (i64.or
          (i64.extend_i32_s
            (local.get 2))
          (i64.shl
            (i64.extend_i32_s
              (local.get 1))
            (i64.const 32))))
      (unreachable)
    )
    (func $core:internal/wait_for_blocked (;16;) (type 15) (param i32) (result i32)
      (local i32 i32 i32)
      (local.set 1
        (call 5))
      (call 3
        (local.get 0)
        (local.get 1))
      (local.set 2
        (call 0
          (i32.const 0)
          (i32.const 0)
          (i32.const 4)
          (i32.const 8)))
      (drop
        (call 6
          (local.get 1)
          (local.get 2)))
      (local.set 3
        (i32.load
          (i32.add
            (local.get 2)
            (i32.const 4))))
      (drop
        (call 0
          (local.get 2)
          (i32.const 8)
          (i32.const 4)
          (i32.const 0)))
      (call 4
        (local.get 1))
      (return
        (local.get 3))
      (unreachable)
    )
    (func $core:internal/cm_stream_write_u8 (;17;) (type 16) (param i32 (ref 1))
      (local i64 i32 i32 i32 (ref null 0) i32)
      (local.set 6
        (struct.get 1 0
          (local.get 1)))
      (local.set 7
        (struct.get 1 1
          (local.get 1)))
      (local.set 2
        (call $core:internal/cm_lower_list_u8
          (ref.as_non_null
            (local.get 6))
          (local.get 7)))
      (local.set 3
        (i32.wrap_i64
          (local.get 2)))
      (local.set 4
        (i32.wrap_i64
          (i64.shr_s
            (local.get 2)
            (i64.const 32))))
      (local.set 5
        (call 1
          (local.get 0)
          (local.get 3)
          (local.get 4)))
      (if ;; label = @1
        (i32.eq
          (local.get 5)
          (i32.const -1))
        (then
          (drop
            (call $core:internal/wait_for_blocked
              (local.get 0)))))
      (drop
        (call 0
          (local.get 3)
          (local.get 4)
          (i32.const 1)
          (i32.const 0)))
    )
  )
  (core instance $wasi-instance (;1;)
    (export "stream-write" (func $stream.write))
    (export "task-return" (func $task.return))
    (export "waitable-join" (func $waitable.join))
    (export "waitable-set-drop" (func $waitable-set.drop))
    (export "waitable-set-new" (func $waitable-set.new))
    (export "waitable-set-wait" (func $waitable-set.wait))
    (export "stream-new" (func $stream.new))
    (export "stream-drop-writable" (func $stream.drop-writable))
    (export "future-drop-readable:transmission-cli" (func $future.drop-readable))
    (export "wasi:cli/Stdout::write_via_stream" (func $wasi:cli/Stdout::write_via_stream))
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
  (alias core export $main "run" (core func $run-core (;11;)))
  (type $run-func-type (;7;) (func async (result $result-unit)))
  (func $run (;1;) (type $run-func-type) (canon lift (core func $run-core) async (memory $memory) (realloc $realloc)))
  (export $"#func2 run" (@name "run") (;2;) "run" (func $run))
)
