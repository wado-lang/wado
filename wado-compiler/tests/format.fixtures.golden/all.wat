(component
  (type $types-instance-type (;0;)
    (instance
      (type (;0;) (enum "io" "illegal-byte-sequence" "pipe"))
      (export (;1;) "error-code" (type (eq 0)))
    )
  )
  (import "wasi:cli/types@0.3.0-rc-2026-01-06" (instance $wasi:cli/types@0.3.0-rc-2026-01-06 (;0;) (type $types-instance-type)))
  (alias export $wasi:cli/types@0.3.0-rc-2026-01-06 "error-code" (type $error-code (;1;)))
  (type $stream-u8 (;2;) (stream u8))
  (type $result-unit (;3;) (result))
  (core module $mem-mod (;0;)
    (type (;0;) (func (param i32)))
    (type (;1;) (func (param i32 i32 i32 i32) (result i32)))
    (type (;2;) (func (param i32 i32 i32 i32) (result i32)))
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
    (func $debug_realloc (;2;) (type 2) (param i32 i32 i32 i32) (result i32)
      (local i32 i32)
      (if ;; label = @1
        (i32.eq
          (local.get 3)
          (i32.const 0))
        (then
          (if ;; label = @2
            (i32.gt_s
              (local.get 1)
              (i32.const 0))
            (then
              (memory.fill
                (local.get 0)
                (i32.const 255)
                (local.get 1))))
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
  (core func $task.return (;1;) (canon task.return (result $result-unit) (memory $memory)))
  (core module $main-mod (;1;)
    (rec
      (type (;0;) (sub (struct (field (mut i32)))))
      (type (;1;) (array (mut i8)))
      (type (;2;) (sub (struct (field (mut (ref 1))) (field (mut i32)))))
      (type (;3;) (sub (struct (field (mut (ref 0))))))
      (type (;4;) (sub (struct (field (mut i32)))))
      (type (;5;) (sub (struct)))
      (type (;6;) (sub (struct)))
      (type (;7;) (sub (struct)))
      (type (;8;) (sub (struct)))
      (type (;9;) (sub (struct)))
      (type (;10;) (sub (struct)))
      (type (;11;) (array (mut i64)))
      (type (;12;) (sub (struct (field (mut (ref 11))) (field (mut i32)))))
      (type (;13;) (func (param structref i32) (result i32)))
      (type (;14;) (sub (struct (field structref) (field (ref 13)))))
      (type (;15;) (func (param structref i32 i32) (result i32)))
      (type (;16;) (sub (struct (field structref) (field (ref 15)))))
      (type (;17;) (func (param structref i32) (result i32)))
      (type (;18;) (sub (struct (field structref) (field (ref 17)))))
      (type (;19;) (func (param structref (ref 2)) (result (ref 2))))
      (type (;20;) (sub (struct (field structref) (field (ref 19)))))
      (type (;21;) (func (param structref)))
      (type (;22;) (sub (struct (field structref) (field (ref 21)))))
    )
    (type (;23;) (func (param i32 i32 i32 i32) (result i32)))
    (type (;24;) (func (param i32)))
    (type (;25;) (func))
    (type (;26;) (func))
    (type (;27;) (func (param structref (ref 2)) (result (ref 1) i32)))
    (import "mem" "realloc" (func (;0;) (type 23)))
    (import "wasi" "task-return" (func (;1;) (type 24)))
    (import "mem" "memory" (memory (;0;) 1))
    (global (;0;) (mut i32) (i32.const 0))
    (export "run" (func $__cm_export__run))
    (func $run (;2;) (type 25)
      (block ;; label = @1
        (if ;; label = @2
          (global.get 0)
          (then
            (br 1 (;@1;))))
        (global.set 0
          (i32.const 1)))
    )
    (func $__cm_export__run (;3;) (type 26)
      (block ;; label = @1
        (if ;; label = @2
          (global.get 0)
          (then
            (br 1 (;@1;))))
        (global.set 0
          (i32.const 1)))
      (call 1
        (i32.const 0))
    )
    (func $__closure_wrapper_0 (;4;) (type 13) (param structref i32) (result i32)
      (local (ref null 10))
      (local.set 2
        (ref.cast (ref 10)
          (local.get 0)))
      (unreachable)
      (unreachable)
    )
    (func $__closure_wrapper_1 (;5;) (type 15) (param structref i32 i32) (result i32)
      (local (ref null 9))
      (local.set 3
        (ref.cast (ref 9)
          (local.get 0)))
      (unreachable)
      (unreachable)
    )
    (func $__closure_wrapper_2 (;6;) (type 17) (param structref i32) (result i32)
      (local (ref null 8))
      (local.set 2
        (ref.cast (ref 8)
          (local.get 0)))
      (unreachable)
      (unreachable)
    )
    (func $__closure_wrapper_3 (;7;) (type 13) (param structref i32) (result i32)
      (local (ref null 7))
      (local.set 2
        (ref.cast (ref 7)
          (local.get 0)))
      (unreachable)
      (unreachable)
    )
    (func $__closure_wrapper_4 (;8;) (type 13) (param structref i32) (result i32)
      (local (ref null 6))
      (local.set 2
        (ref.cast (ref 6)
          (local.get 0)))
      (unreachable)
      (unreachable)
    )
    (func $__closure_wrapper_5 (;9;) (type 27) (param structref (ref 2)) (result (ref 1) i32)
      (local (ref null 5))
      (local.set 2
        (ref.cast (ref 5)
          (local.get 0)))
      (unreachable)
      (unreachable)
    )
    (func $__closure_wrapper_6 (;10;) (type 13) (param structref i32) (result i32)
      (local (ref null 4))
      (local.set 2
        (ref.cast (ref 4)
          (local.get 0)))
      (unreachable)
      (unreachable)
    )
    (func $__closure_wrapper_7 (;11;) (type 21) (param structref)
      (local (ref null 3))
      (local.set 1
        (ref.cast (ref 3)
          (local.get 0)))
      (unreachable)
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
  (func $run (;0;) (type $run-func-type) (canon lift (core func $run-core) async (memory $memory)))
  (export $"#func1 run" (@name "run") (;1;) "run" (func $run))
)
