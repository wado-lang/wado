(component
  (type $result-unit (;0;) (result))
  (core module $mem-mod (;0;)
    (type (;0;) (func (result i64)))
    (type (;1;) (func (param i64) (result i32)))
    (type (;2;) (func (param i64)))
    (type (;3;) (func (param i32)))
    (type (;4;) (func (param i32 i32 i32 i32) (result i32)))
    (memory (;0;) 1)
    (global (;0;) (mut i32) (i32.const 8))
    (export "realloc" (func $realloc))
    (export "memory" (memory 0))
    (func $memory_bytes (;0;) (type 0) (result i64)
      (return
        (i64.mul
          (i64.extend_i32_s
            (memory.size))
          (i64.const 65536)))
      (unreachable)
    )
    (func $pages (;1;) (type 1) (param i64) (result i32)
      (return
        (i32.wrap_i64
          (i64.div_u
            (i64.sub
              (i64.add
                (local.get 0)
                (i64.const 65536))
              (i64.const 1))
            (i64.const 65536))))
      (unreachable)
    )
    (func $grow_memory (;2;) (type 2) (param i64)
      (local i64)
      (local.set 1
        (call $memory_bytes))
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
      (if ;; label = @1
        (i32.lt_s
          (memory.grow
            (call $pages
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
                  (local.get 1)))))
          (i32.const 0))
        (then
          (@metadata.code.branch_hint "\00")
          (if ;; label = @2
            (i32.lt_s
              (memory.grow
                (call $pages
                  (local.get 0)))
              (i32.const 0))
            (then
              (unreachable)))))
    )
    (func $fl_unlink (;3;) (type 3) (param i32)
      (local i32 i32)
      (local.set 1
        (i32.load
          (i32.add
            (local.get 0)
            (i32.const 8))))
      (if ;; label = @1
        (i32.eqz
          (local.tee 2
            (i32.load
              (i32.add
                (local.get 0)
                (i32.const 12)))))
        (then
          (local.set 0
            (i32.shr_u
              (i32.sub
                (i32.and
                  (i32.load
                    (local.get 0))
                  (i32.const -8))
                (i32.const 32))
              (i32.const 4)))
          (i32.store
            (i32.add
              (i32.const 8)
              (i32.mul
                (select (result i32)
                  (local.get 0)
                  (i32.const 32)
                  (i32.lt_u
                    (local.get 0)
                    (i32.const 32)))
                (i32.const 4)))
            (local.get 1)))
        (else
          (i32.store
            (i32.add
              (local.get 2)
              (i32.const 8))
            (local.get 1))))
      (if ;; label = @1
        (i32.ne
          (local.get 1)
          (i32.const 0))
        (then
          (i32.store
            (i32.add
              (local.get 1)
              (i32.const 12))
            (local.get 2))))
    )
    (func $realloc (;4;) (type 4) (param i32 i32 i32 i32) (result i32)
      (local i32 i32 i32 i32 i32 i32 i32 i32 i32 i32 i64 i64)
      (if ;; label = @1
        (i32.lt_u
          (global.get 0)
          (i32.const 152))
        (then
          (global.set 0
            (i32.const 152))))
      (if ;; label = @1
        (i32.eqz
          (local.get 3))
        (then
          (if ;; label = @2
            (i32.eqz
              (local.get 0))
            (then
              (return
                (i32.const 0))))
          (@metadata.code.branch_hint "\00")
          (if ;; label = @2
            (i32.eqz
              (i32.and
                (local.tee 2
                  (i32.load
                    (local.tee 0
                      (i32.sub
                        (local.get 0)
                        (i32.const 8)))))
                (i32.const 1)))
            (then
              (unreachable)))
          (local.set 2
            (i32.and
              (local.get 2)
              (i32.const -8)))
          (if ;; label = @2
            (i32.lt_u
              (local.tee 1
                (i32.add
                  (local.get 0)
                  (local.get 2)))
              (global.get 0))
            (then
              (if ;; label = @3
                (i32.eqz
                  (i32.and
                    (local.tee 5
                      (i32.load
                        (local.get 1)))
                    (i32.const 1)))
                (then
                  (call $fl_unlink
                    (local.get 1))
                  (local.set 2
                    (i32.add
                      (local.get 2)
                      (i32.and
                        (local.get 5)
                        (i32.const -8))))))))
          (if ;; label = @2
            (i32.gt_u
              (local.get 0)
              (i32.const 152))
            (then
              (if ;; label = @3
                (i32.eqz
                  (i32.and
                    (local.tee 5
                      (i32.load
                        (i32.sub
                          (local.get 0)
                          (i32.const 8))))
                    (i32.const 1)))
                (then
                  (local.set 5
                    (i32.and
                      (local.get 5)
                      (i32.const -8)))
                  (call $fl_unlink
                    (local.tee 1
                      (i32.sub
                        (local.get 0)
                        (local.get 5))))
                  (local.set 0
                    (local.get 1))
                  (local.set 2
                    (i32.add
                      (local.get 2)
                      (local.get 5)))))))
          (i32.store
            (local.get 0)
            (local.get 2))
          (i32.store
            (i32.sub
              (i32.add
                (local.get 0)
                (local.get 2))
              (i32.const 8))
            (local.get 2))
          (local.set 2
            (select (result i32)
              (local.tee 2
                (i32.shr_u
                  (i32.sub
                    (local.get 2)
                    (i32.const 32))
                  (i32.const 4)))
              (i32.const 32)
              (i32.lt_u
                (local.get 2)
                (i32.const 32))))
          (local.set 5
            (i32.load
              (i32.add
                (i32.const 8)
                (i32.mul
                  (local.get 2)
                  (i32.const 4)))))
          (i32.store
            (i32.add
              (local.get 0)
              (i32.const 8))
            (local.get 5))
          (i32.store
            (i32.add
              (local.get 0)
              (i32.const 12))
            (i32.const 0))
          (if ;; label = @2
            (i32.ne
              (local.get 5)
              (i32.const 0))
            (then
              (i32.store
                (i32.add
                  (local.get 5)
                  (i32.const 12))
                (local.get 0))))
          (i32.store
            (i32.add
              (i32.const 8)
              (i32.mul
                (local.get 2)
                (i32.const 4)))
            (local.get 0))
          (return
            (i32.const 0))))
      (@metadata.code.branch_hint "\00")
      (if ;; label = @1
        (i64.gt_u
          (local.tee 14
            (i64.add
              (i64.and
                (i64.add
                  (i64.extend_i32_u
                    (local.get 3))
                  (i64.const 15))
                (i64.const -16))
              (i64.const 16)))
          (i64.const 4294967295))
        (then
          (unreachable)))
      (local.set 0
        (select (result i32)
          (local.tee 0
            (i32.shr_u
              (i32.sub
                (local.tee 3
                  (i32.wrap_i64
                    (local.get 14)))
                (i32.const 32))
              (i32.const 4)))
          (i32.const 32)
          (i32.lt_u
            (local.get 0)
            (i32.const 32))))
      (local.set 2
        (i32.add
          (local.get 3)
          (i32.const 1)))
      (block ;; label = @1
        (loop ;; label = @2
          (br_if 1 (;@1;)
            (i32.ge_u
              (local.get 0)
              (i32.const 32)))
          (if ;; label = @3
            (i32.ne
              (local.tee 6
                (i32.load
                  (i32.add
                    (i32.const 8)
                    (i32.mul
                      (local.get 0)
                      (i32.const 4)))))
              (i32.const 0))
            (then
              (local.set 5
                (i32.and
                  (i32.load
                    (local.get 6))
                  (i32.const -8)))
              (local.set 1
                (i32.load
                  (i32.add
                    (local.get 6)
                    (i32.const 8))))
              (if ;; label = @4
                (i32.eqz
                  (local.tee 13
                    (i32.load
                      (i32.add
                        (local.get 6)
                        (i32.const 12)))))
                (then
                  (local.set 10
                    (i32.shr_u
                      (i32.sub
                        (i32.and
                          (i32.load
                            (local.get 6))
                          (i32.const -8))
                        (i32.const 32))
                      (i32.const 4)))
                  (i32.store
                    (i32.add
                      (i32.const 8)
                      (i32.mul
                        (select (result i32)
                          (local.get 10)
                          (i32.const 32)
                          (i32.lt_u
                            (local.get 10)
                            (i32.const 32)))
                        (i32.const 4)))
                    (local.get 1)))
                (else
                  (i32.store
                    (i32.add
                      (local.get 13)
                      (i32.const 8))
                    (local.get 1))))
              (if ;; label = @4
                (i32.ne
                  (local.get 1)
                  (i32.const 0))
                (then
                  (i32.store
                    (i32.add
                      (local.get 1)
                      (i32.const 12))
                    (local.get 13))))
              (if ;; label = @4
                (i32.ge_u
                  (local.tee 7
                    (i32.sub
                      (local.get 5)
                      (local.get 3)))
                  (i32.const 32))
                (then
                  (i32.store
                    (local.get 6)
                    (local.get 2))
                  (i32.store
                    (i32.sub
                      (i32.add
                        (local.get 6)
                        (local.get 3))
                      (i32.const 8))
                    (local.get 2))
                  (i32.store
                    (local.tee 9
                      (i32.add
                        (local.get 6)
                        (local.get 3)))
                    (local.get 7))
                  (i32.store
                    (i32.sub
                      (i32.add
                        (local.get 9)
                        (local.get 7))
                      (i32.const 8))
                    (local.get 7))
                  (local.set 11
                    (select (result i32)
                      (local.tee 12
                        (i32.shr_u
                          (i32.sub
                            (local.get 7)
                            (i32.const 32))
                          (i32.const 4)))
                      (i32.const 32)
                      (i32.lt_u
                        (local.get 12)
                        (i32.const 32))))
                  (local.set 8
                    (i32.load
                      (i32.add
                        (i32.const 8)
                        (i32.mul
                          (local.get 11)
                          (i32.const 4)))))
                  (i32.store
                    (i32.add
                      (local.get 9)
                      (i32.const 8))
                    (local.get 8))
                  (i32.store
                    (i32.add
                      (local.get 9)
                      (i32.const 12))
                    (i32.const 0))
                  (if ;; label = @5
                    (i32.ne
                      (local.get 8)
                      (i32.const 0))
                    (then
                      (i32.store
                        (i32.add
                          (local.get 8)
                          (i32.const 12))
                        (local.get 9))))
                  (i32.store
                    (i32.add
                      (i32.const 8)
                      (i32.mul
                        (local.get 11)
                        (i32.const 4)))
                    (local.get 9)))
                (else
                  (local.set 4
                    (i32.add
                      (local.get 5)
                      (i32.const 1)))
                  (i32.store
                    (local.get 6)
                    (local.get 4))
                  (i32.store
                    (i32.sub
                      (i32.add
                        (local.get 6)
                        (local.get 5))
                      (i32.const 8))
                    (local.get 4))))
              (return
                (i32.add
                  (local.get 6)
                  (i32.const 8)))))
          (local.set 0
            (i32.add
              (local.get 0)
              (i32.const 1)))
          (br 0 (;@2;))))
      (local.set 4
        (i32.load
          (i32.const 136)))
      (local.set 8
        (i32.add
          (local.get 3)
          (i32.const 1)))
      (block ;; label = @1
        (loop ;; label = @2
          (br_if 1 (;@1;)
            (i32.eqz
              (local.get 4)))
          (if ;; label = @3
            (i32.ge_u
              (local.tee 0
                (i32.and
                  (i32.load
                    (local.get 4))
                  (i32.const -8)))
              (local.get 3))
            (then
              (local.set 11
                (i32.load
                  (i32.add
                    (local.get 4)
                    (i32.const 8))))
              (if ;; label = @4
                (i32.eqz
                  (local.tee 12
                    (i32.load
                      (i32.add
                        (local.get 4)
                        (i32.const 12)))))
                (then
                  (local.set 9
                    (i32.shr_u
                      (i32.sub
                        (i32.and
                          (i32.load
                            (local.get 4))
                          (i32.const -8))
                        (i32.const 32))
                      (i32.const 4)))
                  (i32.store
                    (i32.add
                      (i32.const 8)
                      (i32.mul
                        (select (result i32)
                          (local.get 9)
                          (i32.const 32)
                          (i32.lt_u
                            (local.get 9)
                            (i32.const 32)))
                        (i32.const 4)))
                    (local.get 11)))
                (else
                  (i32.store
                    (i32.add
                      (local.get 12)
                      (i32.const 8))
                    (local.get 11))))
              (if ;; label = @4
                (i32.ne
                  (local.get 11)
                  (i32.const 0))
                (then
                  (i32.store
                    (i32.add
                      (local.get 11)
                      (i32.const 12))
                    (local.get 12))))
              (if ;; label = @4
                (i32.ge_u
                  (local.tee 7
                    (i32.sub
                      (local.get 0)
                      (local.get 3)))
                  (i32.const 32))
                (then
                  (i32.store
                    (local.get 4)
                    (local.get 8))
                  (i32.store
                    (i32.sub
                      (i32.add
                        (local.get 4)
                        (local.get 3))
                      (i32.const 8))
                    (local.get 8))
                  (i32.store
                    (local.tee 10
                      (i32.add
                        (local.get 4)
                        (local.get 3)))
                    (local.get 7))
                  (i32.store
                    (i32.sub
                      (i32.add
                        (local.get 10)
                        (local.get 7))
                      (i32.const 8))
                    (local.get 7))
                  (local.set 5
                    (select (result i32)
                      (local.tee 13
                        (i32.shr_u
                          (i32.sub
                            (local.get 7)
                            (i32.const 32))
                          (i32.const 4)))
                      (i32.const 32)
                      (i32.lt_u
                        (local.get 13)
                        (i32.const 32))))
                  (local.set 2
                    (i32.load
                      (i32.add
                        (i32.const 8)
                        (i32.mul
                          (local.get 5)
                          (i32.const 4)))))
                  (i32.store
                    (i32.add
                      (local.get 10)
                      (i32.const 8))
                    (local.get 2))
                  (i32.store
                    (i32.add
                      (local.get 10)
                      (i32.const 12))
                    (i32.const 0))
                  (if ;; label = @5
                    (i32.ne
                      (local.get 2)
                      (i32.const 0))
                    (then
                      (i32.store
                        (i32.add
                          (local.get 2)
                          (i32.const 12))
                        (local.get 10))))
                  (i32.store
                    (i32.add
                      (i32.const 8)
                      (i32.mul
                        (local.get 5)
                        (i32.const 4)))
                    (local.get 10)))
                (else
                  (local.set 1
                    (i32.add
                      (local.get 0)
                      (i32.const 1)))
                  (i32.store
                    (local.get 4)
                    (local.get 1))
                  (i32.store
                    (i32.sub
                      (i32.add
                        (local.get 4)
                        (local.get 0))
                      (i32.const 8))
                    (local.get 1))))
              (return
                (i32.add
                  (local.get 4)
                  (i32.const 8)))))
          (local.set 4
            (i32.load
              (i32.add
                (local.get 4)
                (i32.const 8))))
          (br 0 (;@2;))))
      (@metadata.code.branch_hint "\00")
      (if ;; label = @1
        (i64.gt_u
          (local.tee 14
            (i64.add
              (local.tee 15
                (i64.and
                  (i64.extend_i32_u
                    (global.get 0))
                  (i64.const -1)))
              (local.get 14)))
          (i64.const 4294967295))
        (then
          (unreachable)))
      (@metadata.code.branch_hint "\00")
      (if ;; label = @1
        (i64.gt_u
          (local.get 14)
          (i64.mul
            (i64.extend_i32_s
              (memory.size))
            (i64.const 65536)))
        (then
          (call $grow_memory
            (local.get 14))))
      (global.set 0
        (i32.wrap_i64
          (local.get 14)))
      (local.set 4
        (i32.wrap_i64
          (local.get 15)))
      (local.set 8
        (i32.add
          (local.get 3)
          (i32.const 1)))
      (i32.store
        (local.get 4)
        (local.get 8))
      (i32.store
        (i32.sub
          (i32.add
            (local.get 4)
            (local.get 3))
          (i32.const 8))
        (local.get 8))
      (return
        (i32.add
          (local.get 4)
          (i32.const 8)))
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
    (import "mem" "realloc" (func (;0;) (type 0) (param i32 i32 i32 i32) (result i32)))
    (import "mem" "memory" (memory (;0;) 1))
    (import "wasi" "task-return" (func (;1;) (type 2) (param i32)))
    (export "run" (func $mess_dirty.wado/$cm_export__run))
    (func $mess_dirty.wado/$cm_export__run (;2;) (type 1)
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
