(module $wado_bundled.wasm
  (type (;0;) (func (param f32 i32) (result i32)))
  (type (;1;) (func (param f64 i32) (result i32)))
  (type (;2;) (func (param i64 i32)))
  (type (;3;) (func (param i32 i64 i64 i32)))
  (type (;4;) (func (param i32 i64 i64 i64 i64)))
  (memory (;0;) 17)
  (global $__stack_pointer (;0;) (mut i32) i32.const 1048576)
  (global (;1;) i32 i32.const 1049611)
  (global (;2;) i32 i32.const 1049616)
  (export "memory" (memory 0))
  (export "f32_to_buffer" (func $f32_to_buffer))
  (export "f64_to_buffer" (func $f64_to_buffer))
  (export "__data_end" (global 1))
  (export "__heap_base" (global 2))
  (func $f32_to_buffer (;0;) (type 0) (param f32 i32) (result i32)
    (local i32 i32 i32 i32)
    global.get $__stack_pointer
    i32.const 32
    i32.sub
    local.tee 2
    global.set $__stack_pointer
    block ;; label = @1
      block ;; label = @2
        block ;; label = @3
          local.get 0
          i32.reinterpret_f32
          local.tee 3
          i32.const 2147483647
          i32.and
          i32.const 2139095040
          i32.lt_s
          br_if 0 (;@3;)
          i32.const 1048583
          i32.const 1048576
          i32.const 1048579
          local.get 3
          i32.const -1
          i32.gt_s
          local.tee 4
          select
          local.get 3
          i32.const 8388607
          i32.and
          local.tee 3
          select
          local.set 5
          i32.const 3
          i32.const 3
          i32.const 4
          local.get 4
          select
          local.get 3
          select
          local.set 3
          br 1 (;@2;)
        end
        local.get 2
        i32.const 8
        i32.add
        local.set 5
        local.get 0
        local.get 2
        i32.const 8
        i32.add
        call $_ZN3ryu6pretty8format3217hd00639a7ea3ab53dE
        local.tee 3
        br_if 0 (;@2;)
        i32.const 0
        local.set 3
        br 1 (;@1;)
      end
      local.get 3
      i32.eqz
      br_if 0 (;@1;)
      local.get 1
      local.get 5
      local.get 3
      memory.copy
    end
    local.get 2
    i32.const 32
    i32.add
    global.set $__stack_pointer
    local.get 3
  )
  (func $f64_to_buffer (;1;) (type 1) (param f64 i32) (result i32)
    (local i32 i64 i32 i32 i32)
    global.get $__stack_pointer
    i32.const 32
    i32.sub
    local.tee 2
    global.set $__stack_pointer
    block ;; label = @1
      block ;; label = @2
        block ;; label = @3
          local.get 0
          i64.reinterpret_f64
          local.tee 3
          i64.const 9223372036854775807
          i64.and
          i64.const 9218868437227405312
          i64.lt_s
          br_if 0 (;@3;)
          i32.const 1048576
          i32.const 1048579
          local.get 3
          i64.const -1
          i64.gt_s
          local.tee 4
          select
          i32.const 1048583
          local.get 3
          i64.const 4503599627370495
          i64.and
          i64.eqz
          local.tee 5
          select
          local.set 6
          i32.const 3
          i32.const 4
          local.get 4
          select
          i32.const 3
          local.get 5
          select
          local.set 4
          br 1 (;@2;)
        end
        local.get 2
        i32.const 8
        i32.add
        local.set 6
        local.get 0
        local.get 2
        i32.const 8
        i32.add
        call $_ZN3ryu6pretty8format6417h1c34065fb67249adE
        local.tee 4
        br_if 0 (;@2;)
        i32.const 0
        local.set 4
        br 1 (;@1;)
      end
      local.get 4
      i32.eqz
      br_if 0 (;@1;)
      local.get 1
      local.get 6
      local.get 4
      memory.copy
    end
    local.get 2
    i32.const 32
    i32.add
    global.set $__stack_pointer
    local.get 4
  )
  (func $_ZN3ryu6pretty8format3217hd00639a7ea3ab53dE (;2;) (type 0) (param f32 i32) (result i32)
    (local i32 i32 i32 i32 i32 i32 i32 i32 i32 i32 i32 i32 i32 i32 i32 i64 i64 i32 i32 i64 i64 i64 i64 i32)
    global.get $__stack_pointer
    i32.const 512
    i32.sub
    local.tee 2
    global.set $__stack_pointer
    local.get 0
    i32.reinterpret_f32
    local.tee 3
    i32.const 8388607
    i32.and
    local.set 4
    local.get 3
    i32.const 23
    i32.shr_u
    i32.const 255
    i32.and
    local.set 5
    i32.const 0
    local.set 6
    block ;; label = @1
      local.get 3
      i32.const 0
      i32.ge_s
      br_if 0 (;@1;)
      local.get 1
      i32.const 45
      i32.store8
      i32.const 1
      local.set 6
    end
    block ;; label = @1
      block ;; label = @2
        block ;; label = @3
          block ;; label = @4
            block ;; label = @5
              local.get 5
              local.get 4
              i32.or
              i32.eqz
              br_if 0 (;@5;)
              local.get 4
              i32.const 0
              i32.ne
              local.get 5
              i32.const 2
              i32.lt_u
              i32.or
              local.tee 7
              i32.const -1
              i32.xor
              local.get 4
              i32.const 8388608
              i32.or
              local.get 4
              local.get 5
              select
              local.tee 8
              i32.const 2
              i32.shl
              local.tee 4
              i32.add
              local.set 3
              local.get 8
              i32.const 1
              i32.and
              local.set 9
              local.get 4
              i32.const 2
              i32.or
              local.set 8
              block ;; label = @6
                block ;; label = @7
                  block ;; label = @8
                    block ;; label = @9
                      local.get 5
                      i32.const -152
                      i32.add
                      i32.const -151
                      local.get 5
                      select
                      local.tee 10
                      i32.const -1
                      i32.gt_s
                      br_if 0 (;@9;)
                      local.get 10
                      i32.const -732923
                      i32.mul
                      local.tee 11
                      i32.const 20
                      i32.shr_u
                      local.tee 12
                      local.get 12
                      local.get 10
                      i32.add
                      local.tee 13
                      i32.const 32337073
                      i32.mul
                      i32.const 19
                      i32.shr_u
                      i32.sub
                      i32.const 60
                      i32.add
                      local.set 14
                      i32.const 0
                      local.get 13
                      i32.sub
                      local.tee 5
                      i32.const 255
                      i32.and
                      i32.const 26
                      i32.div_u
                      local.tee 10
                      i32.const 4
                      i32.shl
                      local.tee 15
                      i32.const 1049160
                      i32.add
                      local.set 16
                      local.get 15
                      i64.load offset=1049168
                      local.tee 17
                      local.set 18
                      block ;; label = @10
                        local.get 5
                        local.get 10
                        i32.const 26
                        i32.mul
                        local.tee 19
                        i32.eq
                        local.tee 15
                        br_if 0 (;@10;)
                        local.get 2
                        i32.const 496
                        i32.add
                        local.get 5
                        local.get 19
                        i32.sub
                        i32.const 3
                        i32.shl
                        i64.load offset=1048952
                        local.tee 18
                        i64.const 0
                        local.get 16
                        i64.load
                        i64.const 0
                        call $__multi3
                        local.get 2
                        i32.const 480
                        i32.add
                        local.get 18
                        i64.const 0
                        local.get 17
                        i64.const 0
                        call $__multi3
                        local.get 2
                        i32.const 464
                        i32.add
                        local.get 2
                        i64.load offset=496
                        local.get 2
                        i64.load offset=504
                        local.get 13
                        i32.const -1217359
                        i32.mul
                        i32.const 19
                        i32.shr_u
                        local.get 10
                        i32.const 31651334
                        i32.mul
                        i32.const 19
                        i32.shr_u
                        i32.sub
                        local.tee 20
                        i32.const 127
                        i32.and
                        call $__lshrti3
                        local.get 2
                        i32.const 448
                        i32.add
                        local.get 2
                        i64.load offset=480
                        local.get 2
                        i64.load offset=488
                        i32.const 64
                        local.get 20
                        i32.sub
                        i32.const 127
                        i32.and
                        call $__ashlti3
                        local.get 2
                        i64.load offset=472
                        local.get 2
                        i64.load offset=456
                        i64.add
                        local.get 2
                        i64.load offset=464
                        local.tee 21
                        local.get 2
                        i64.load offset=448
                        i64.add
                        local.tee 18
                        local.get 21
                        i64.lt_u
                        i64.extend_i32_u
                        i64.add
                        local.get 18
                        local.get 5
                        i32.const 2
                        i32.shr_u
                        i32.const 1073741820
                        i32.and
                        i32.load offset=1048788
                        local.get 5
                        i32.const 1
                        i32.shl
                        i32.shr_u
                        i32.const 3
                        i32.and
                        i64.extend_i32_u
                        i64.add
                        local.get 18
                        i64.lt_u
                        i64.extend_i32_u
                        i64.add
                        local.set 18
                      end
                      local.get 18
                      i64.const 4294967295
                      i64.and
                      local.get 4
                      i64.extend_i32_u
                      local.tee 22
                      i64.mul
                      i64.const 32
                      i64.shr_u
                      local.get 18
                      i64.const 32
                      i64.shr_u
                      local.get 22
                      i64.mul
                      i64.add
                      local.set 23
                      local.get 14
                      i32.const 63
                      i32.and
                      i32.const 32
                      i32.xor
                      i64.extend_i32_u
                      local.set 18
                      local.get 17
                      local.set 21
                      block ;; label = @10
                        local.get 15
                        br_if 0 (;@10;)
                        local.get 2
                        i32.const 432
                        i32.add
                        local.get 5
                        local.get 19
                        i32.sub
                        i32.const 3
                        i32.shl
                        i64.load offset=1048952
                        local.tee 21
                        i64.const 0
                        local.get 16
                        i64.load
                        i64.const 0
                        call $__multi3
                        local.get 2
                        i32.const 416
                        i32.add
                        local.get 21
                        i64.const 0
                        local.get 17
                        i64.const 0
                        call $__multi3
                        local.get 2
                        i32.const 400
                        i32.add
                        local.get 2
                        i64.load offset=432
                        local.get 2
                        i64.load offset=440
                        local.get 13
                        i32.const -1217359
                        i32.mul
                        i32.const 19
                        i32.shr_u
                        local.get 10
                        i32.const 31651334
                        i32.mul
                        i32.const 19
                        i32.shr_u
                        i32.sub
                        local.tee 14
                        i32.const 127
                        i32.and
                        call $__lshrti3
                        local.get 2
                        i32.const 384
                        i32.add
                        local.get 2
                        i64.load offset=416
                        local.get 2
                        i64.load offset=424
                        i32.const 64
                        local.get 14
                        i32.sub
                        i32.const 127
                        i32.and
                        call $__ashlti3
                        local.get 2
                        i64.load offset=408
                        local.get 2
                        i64.load offset=392
                        i64.add
                        local.get 2
                        i64.load offset=400
                        local.tee 24
                        local.get 2
                        i64.load offset=384
                        i64.add
                        local.tee 21
                        local.get 24
                        i64.lt_u
                        i64.extend_i32_u
                        i64.add
                        local.get 21
                        local.get 5
                        i32.const 2
                        i32.shr_u
                        i32.const 1073741820
                        i32.and
                        i32.load offset=1048788
                        local.get 5
                        i32.const 1
                        i32.shl
                        i32.shr_u
                        i32.const 3
                        i32.and
                        i64.extend_i32_u
                        i64.add
                        local.get 21
                        i64.lt_u
                        i64.extend_i32_u
                        i64.add
                        local.set 21
                      end
                      local.get 23
                      local.get 18
                      i64.shr_u
                      local.set 23
                      local.get 21
                      i64.const 4294967295
                      i64.and
                      local.get 8
                      i64.extend_i32_u
                      local.tee 24
                      i64.mul
                      i64.const 32
                      i64.shr_u
                      local.get 21
                      i64.const 32
                      i64.shr_u
                      local.get 24
                      i64.mul
                      i64.add
                      local.get 18
                      i64.shr_u
                      local.set 21
                      block ;; label = @10
                        local.get 15
                        br_if 0 (;@10;)
                        local.get 2
                        i32.const 368
                        i32.add
                        local.get 5
                        local.get 19
                        i32.sub
                        i32.const 3
                        i32.shl
                        i64.load offset=1048952
                        local.tee 24
                        i64.const 0
                        local.get 16
                        i64.load
                        i64.const 0
                        call $__multi3
                        local.get 2
                        i32.const 352
                        i32.add
                        local.get 24
                        i64.const 0
                        local.get 17
                        i64.const 0
                        call $__multi3
                        local.get 2
                        i32.const 336
                        i32.add
                        local.get 2
                        i64.load offset=368
                        local.get 2
                        i64.load offset=376
                        local.get 13
                        i32.const -1217359
                        i32.mul
                        i32.const 19
                        i32.shr_u
                        local.get 10
                        i32.const 31651334
                        i32.mul
                        i32.const 19
                        i32.shr_u
                        i32.sub
                        local.tee 8
                        i32.const 127
                        i32.and
                        call $__lshrti3
                        local.get 2
                        i32.const 320
                        i32.add
                        local.get 2
                        i64.load offset=352
                        local.get 2
                        i64.load offset=360
                        i32.const 64
                        local.get 8
                        i32.sub
                        i32.const 127
                        i32.and
                        call $__ashlti3
                        local.get 2
                        i64.load offset=344
                        local.get 2
                        i64.load offset=328
                        i64.add
                        local.get 2
                        i64.load offset=336
                        local.tee 24
                        local.get 2
                        i64.load offset=320
                        i64.add
                        local.tee 17
                        local.get 24
                        i64.lt_u
                        i64.extend_i32_u
                        i64.add
                        local.get 17
                        local.get 5
                        i32.const 2
                        i32.shr_u
                        i32.const 1073741820
                        i32.and
                        i32.load offset=1048788
                        local.get 5
                        i32.const 1
                        i32.shl
                        i32.shr_u
                        i32.const 3
                        i32.and
                        i64.extend_i32_u
                        i64.add
                        local.get 17
                        i64.lt_u
                        i64.extend_i32_u
                        i64.add
                        local.set 17
                      end
                      local.get 23
                      i32.wrap_i64
                      local.set 5
                      local.get 21
                      i32.wrap_i64
                      local.set 15
                      local.get 17
                      i64.const 4294967295
                      i64.and
                      local.get 3
                      i64.extend_i32_u
                      local.tee 21
                      i64.mul
                      i64.const 32
                      i64.shr_u
                      local.get 17
                      i64.const 32
                      i64.shr_u
                      local.get 21
                      i64.mul
                      i64.add
                      local.get 18
                      i64.shr_u
                      i32.wrap_i64
                      local.set 16
                      i32.const 0
                      local.set 19
                      local.get 11
                      i32.const 1048576
                      i32.lt_u
                      br_if 2 (;@7;)
                      i32.const 0
                      local.set 19
                      block ;; label = @10
                        local.get 15
                        i32.const -1
                        i32.add
                        i32.const 10
                        i32.div_u
                        local.get 16
                        i32.const 10
                        i32.div_u
                        i32.gt_u
                        br_if 0 (;@10;)
                        local.get 12
                        i32.const 1
                        local.get 13
                        i32.sub
                        local.tee 3
                        i32.const 1217359
                        i32.mul
                        i32.const 19
                        i32.shr_u
                        local.tee 19
                        i32.sub
                        i32.const 59
                        i32.add
                        local.set 10
                        local.get 3
                        i32.const 255
                        i32.and
                        i32.const 26
                        i32.div_u
                        local.tee 8
                        i32.const 4
                        i32.shl
                        local.tee 14
                        i64.load offset=1049168
                        local.set 17
                        block ;; label = @11
                          local.get 3
                          local.get 8
                          i32.const 26
                          i32.mul
                          local.tee 20
                          i32.eq
                          br_if 0 (;@11;)
                          local.get 2
                          i32.const 304
                          i32.add
                          local.get 3
                          local.get 20
                          i32.sub
                          i32.const 3
                          i32.shl
                          i64.load offset=1048952
                          local.tee 18
                          i64.const 0
                          local.get 14
                          i32.const 1049160
                          i32.add
                          i64.load
                          i64.const 0
                          call $__multi3
                          local.get 2
                          i32.const 288
                          i32.add
                          local.get 18
                          i64.const 0
                          local.get 17
                          i64.const 0
                          call $__multi3
                          local.get 2
                          i32.const 272
                          i32.add
                          local.get 2
                          i64.load offset=304
                          local.get 2
                          i64.load offset=312
                          local.get 19
                          local.get 8
                          i32.const 31651334
                          i32.mul
                          i32.const 19
                          i32.shr_u
                          i32.sub
                          local.tee 8
                          i32.const 127
                          i32.and
                          call $__lshrti3
                          local.get 2
                          i32.const 256
                          i32.add
                          local.get 2
                          i64.load offset=288
                          local.get 2
                          i64.load offset=296
                          i32.const 64
                          local.get 8
                          i32.sub
                          i32.const 127
                          i32.and
                          call $__ashlti3
                          local.get 2
                          i64.load offset=280
                          local.get 2
                          i64.load offset=264
                          i64.add
                          local.get 2
                          i64.load offset=272
                          local.tee 18
                          local.get 2
                          i64.load offset=256
                          i64.add
                          local.tee 17
                          local.get 18
                          i64.lt_u
                          i64.extend_i32_u
                          i64.add
                          local.get 17
                          local.get 3
                          i32.const 2
                          i32.shr_u
                          i32.const 1073741820
                          i32.and
                          i32.load offset=1048788
                          local.get 3
                          i32.const 1
                          i32.shl
                          i32.shr_u
                          i32.const 3
                          i32.and
                          i64.extend_i32_u
                          i64.add
                          local.get 17
                          i64.lt_u
                          i64.extend_i32_u
                          i64.add
                          local.set 17
                        end
                        local.get 17
                        i64.const 4294967295
                        i64.and
                        local.get 22
                        i64.mul
                        i64.const 32
                        i64.shr_u
                        local.get 17
                        i64.const 32
                        i64.shr_u
                        local.get 22
                        i64.mul
                        i64.add
                        local.get 10
                        i32.const 63
                        i32.and
                        i32.const 32
                        i32.xor
                        i64.extend_i32_u
                        i64.shr_u
                        i32.wrap_i64
                        i32.const 10
                        i32.rem_u
                        local.set 19
                      end
                      local.get 11
                      i32.const 2097152
                      i32.lt_u
                      br_if 2 (;@7;)
                      i32.const 0
                      local.set 7
                      local.get 11
                      i32.const 32505856
                      i32.lt_u
                      br_if 1 (;@8;)
                      i32.const 0
                      local.set 10
                      br 5 (;@4;)
                    end
                    local.get 10
                    i32.const 78913
                    i32.mul
                    local.tee 16
                    i32.const 18
                    i32.shr_u
                    local.tee 13
                    local.get 10
                    i32.sub
                    local.tee 14
                    local.get 13
                    i32.const 1217359
                    i32.mul
                    i32.const 19
                    i32.shr_u
                    local.tee 7
                    i32.add
                    i32.const 61
                    i32.add
                    local.set 12
                    local.get 13
                    i32.const 25
                    i32.add
                    i32.const 255
                    i32.and
                    i32.const 26
                    i32.div_u
                    local.tee 5
                    i32.const 4
                    i32.shl
                    local.tee 15
                    i32.const 1049368
                    i32.add
                    local.set 19
                    local.get 15
                    i64.load offset=1049376
                    local.tee 17
                    local.set 18
                    block ;; label = @9
                      local.get 5
                      i32.const 26
                      i32.mul
                      local.tee 11
                      local.get 13
                      i32.eq
                      local.tee 15
                      br_if 0 (;@9;)
                      local.get 2
                      i32.const 240
                      i32.add
                      local.get 11
                      local.get 13
                      i32.sub
                      i32.const 3
                      i32.shl
                      i64.load offset=1048952
                      local.tee 18
                      i64.const 0
                      local.get 19
                      i64.load
                      i64.const -1
                      i64.add
                      i64.const 0
                      call $__multi3
                      local.get 2
                      i32.const 224
                      i32.add
                      local.get 18
                      i64.const 0
                      local.get 17
                      i64.const 0
                      call $__multi3
                      local.get 2
                      i32.const 208
                      i32.add
                      local.get 2
                      i64.load offset=240
                      local.get 2
                      i64.load offset=248
                      local.get 5
                      i32.const 31651334
                      i32.mul
                      i32.const 19
                      i32.shr_u
                      local.get 7
                      i32.sub
                      local.tee 20
                      i32.const 127
                      i32.and
                      call $__lshrti3
                      local.get 2
                      i32.const 192
                      i32.add
                      local.get 2
                      i64.load offset=224
                      local.get 2
                      i64.load offset=232
                      i32.const 64
                      local.get 20
                      i32.sub
                      i32.const 127
                      i32.and
                      call $__ashlti3
                      local.get 2
                      i64.load offset=200
                      local.get 2
                      i64.load offset=216
                      i64.add
                      local.get 2
                      i64.load offset=192
                      local.tee 21
                      local.get 2
                      i64.load offset=208
                      i64.add
                      local.tee 18
                      local.get 21
                      i64.lt_u
                      i64.extend_i32_u
                      i64.add
                      local.get 18
                      local.get 16
                      i32.const 20
                      i32.shr_u
                      i32.const 4092
                      i32.and
                      i32.load offset=1048872
                      local.get 13
                      i32.const 1
                      i32.shl
                      i32.shr_u
                      i32.const 3
                      i32.and
                      i64.extend_i32_u
                      i64.add
                      local.tee 21
                      local.get 18
                      i64.lt_u
                      i64.extend_i32_u
                      i64.add
                      local.get 21
                      i64.const -1
                      i64.eq
                      i64.extend_i32_u
                      i64.add
                      local.set 18
                    end
                    local.get 18
                    i64.const 1
                    i64.add
                    local.tee 18
                    i64.const 4294967295
                    i64.and
                    local.get 4
                    i64.extend_i32_u
                    local.tee 21
                    i64.mul
                    i64.const 32
                    i64.shr_u
                    local.get 18
                    i64.const 32
                    i64.shr_u
                    local.get 21
                    i64.mul
                    i64.add
                    local.set 23
                    local.get 12
                    i32.const 63
                    i32.and
                    i32.const 32
                    i32.xor
                    i64.extend_i32_u
                    local.set 18
                    local.get 17
                    local.set 22
                    block ;; label = @9
                      local.get 15
                      br_if 0 (;@9;)
                      local.get 2
                      i32.const 176
                      i32.add
                      local.get 11
                      local.get 13
                      i32.sub
                      i32.const 3
                      i32.shl
                      i64.load offset=1048952
                      local.tee 22
                      i64.const 0
                      local.get 19
                      i64.load
                      i64.const -1
                      i64.add
                      i64.const 0
                      call $__multi3
                      local.get 2
                      i32.const 160
                      i32.add
                      local.get 22
                      i64.const 0
                      local.get 17
                      i64.const 0
                      call $__multi3
                      local.get 2
                      i32.const 144
                      i32.add
                      local.get 2
                      i64.load offset=176
                      local.get 2
                      i64.load offset=184
                      local.get 5
                      i32.const 31651334
                      i32.mul
                      i32.const 19
                      i32.shr_u
                      local.get 7
                      i32.sub
                      local.tee 12
                      i32.const 127
                      i32.and
                      call $__lshrti3
                      local.get 2
                      i32.const 128
                      i32.add
                      local.get 2
                      i64.load offset=160
                      local.get 2
                      i64.load offset=168
                      i32.const 64
                      local.get 12
                      i32.sub
                      i32.const 127
                      i32.and
                      call $__ashlti3
                      local.get 2
                      i64.load offset=136
                      local.get 2
                      i64.load offset=152
                      i64.add
                      local.get 2
                      i64.load offset=128
                      local.tee 24
                      local.get 2
                      i64.load offset=144
                      i64.add
                      local.tee 22
                      local.get 24
                      i64.lt_u
                      i64.extend_i32_u
                      i64.add
                      local.get 22
                      local.get 16
                      i32.const 20
                      i32.shr_u
                      i32.const 4092
                      i32.and
                      i32.load offset=1048872
                      local.get 13
                      i32.const 1
                      i32.shl
                      i32.shr_u
                      i32.const 3
                      i32.and
                      i64.extend_i32_u
                      i64.add
                      local.tee 24
                      local.get 22
                      i64.lt_u
                      i64.extend_i32_u
                      i64.add
                      local.get 24
                      i64.const -1
                      i64.eq
                      i64.extend_i32_u
                      i64.add
                      local.set 22
                    end
                    local.get 23
                    local.get 18
                    i64.shr_u
                    local.set 23
                    local.get 22
                    i64.const 1
                    i64.add
                    local.tee 22
                    i64.const 4294967295
                    i64.and
                    local.get 8
                    i64.extend_i32_u
                    local.tee 24
                    i64.mul
                    i64.const 32
                    i64.shr_u
                    local.get 22
                    i64.const 32
                    i64.shr_u
                    local.get 24
                    i64.mul
                    i64.add
                    local.get 18
                    i64.shr_u
                    local.set 22
                    block ;; label = @9
                      local.get 15
                      br_if 0 (;@9;)
                      local.get 2
                      i32.const 112
                      i32.add
                      local.get 11
                      local.get 13
                      i32.sub
                      i32.const 3
                      i32.shl
                      i64.load offset=1048952
                      local.tee 24
                      i64.const 0
                      local.get 19
                      i64.load
                      i64.const -1
                      i64.add
                      i64.const 0
                      call $__multi3
                      local.get 2
                      i32.const 96
                      i32.add
                      local.get 24
                      i64.const 0
                      local.get 17
                      i64.const 0
                      call $__multi3
                      local.get 2
                      i32.const 80
                      i32.add
                      local.get 2
                      i64.load offset=112
                      local.get 2
                      i64.load offset=120
                      local.get 5
                      i32.const 31651334
                      i32.mul
                      i32.const 19
                      i32.shr_u
                      local.get 7
                      i32.sub
                      local.tee 5
                      i32.const 127
                      i32.and
                      call $__lshrti3
                      local.get 2
                      i32.const 64
                      i32.add
                      local.get 2
                      i64.load offset=96
                      local.get 2
                      i64.load offset=104
                      i32.const 64
                      local.get 5
                      i32.sub
                      i32.const 127
                      i32.and
                      call $__ashlti3
                      local.get 2
                      i64.load offset=72
                      local.get 2
                      i64.load offset=88
                      i64.add
                      local.get 2
                      i64.load offset=64
                      local.tee 24
                      local.get 2
                      i64.load offset=80
                      i64.add
                      local.tee 17
                      local.get 24
                      i64.lt_u
                      i64.extend_i32_u
                      i64.add
                      local.get 17
                      local.get 16
                      i32.const 20
                      i32.shr_u
                      i32.const 4092
                      i32.and
                      i32.load offset=1048872
                      local.get 13
                      i32.const 1
                      i32.shl
                      i32.shr_u
                      i32.const 3
                      i32.and
                      i64.extend_i32_u
                      i64.add
                      local.tee 24
                      local.get 17
                      i64.lt_u
                      i64.extend_i32_u
                      i64.add
                      local.get 24
                      i64.const -1
                      i64.eq
                      i64.extend_i32_u
                      i64.add
                      local.set 17
                    end
                    local.get 23
                    i32.wrap_i64
                    local.set 5
                    local.get 22
                    i32.wrap_i64
                    local.set 15
                    local.get 17
                    i64.const 1
                    i64.add
                    local.tee 17
                    i64.const 4294967295
                    i64.and
                    local.get 3
                    i64.extend_i32_u
                    local.tee 22
                    i64.mul
                    i64.const 32
                    i64.shr_u
                    local.get 17
                    i64.const 32
                    i64.shr_u
                    local.get 22
                    i64.mul
                    i64.add
                    local.get 18
                    i64.shr_u
                    i32.wrap_i64
                    local.set 16
                    i32.const 0
                    local.set 19
                    local.get 10
                    i32.const 4
                    i32.lt_u
                    br_if 2 (;@6;)
                    i32.const 0
                    local.set 7
                    i32.const 0
                    local.set 19
                    block ;; label = @9
                      local.get 15
                      i32.const -1
                      i32.add
                      i32.const 10
                      i32.div_u
                      local.get 16
                      i32.const 10
                      i32.div_u
                      i32.gt_u
                      br_if 0 (;@9;)
                      local.get 14
                      local.get 13
                      i32.const -1
                      i32.add
                      local.tee 19
                      i32.const 1217359
                      i32.mul
                      i32.const 19
                      i32.shr_u
                      local.tee 20
                      i32.add
                      i32.const 60
                      i32.add
                      local.set 12
                      local.get 13
                      i32.const 24
                      i32.add
                      i32.const 255
                      i32.and
                      i32.const 26
                      i32.div_u
                      local.tee 11
                      i32.const 4
                      i32.shl
                      local.tee 14
                      i64.load offset=1049376
                      local.set 17
                      block ;; label = @10
                        local.get 11
                        i32.const 26
                        i32.mul
                        local.tee 25
                        local.get 19
                        i32.eq
                        br_if 0 (;@10;)
                        local.get 2
                        i32.const 48
                        i32.add
                        local.get 25
                        local.get 19
                        i32.sub
                        i32.const 3
                        i32.shl
                        i64.load offset=1048952
                        local.tee 18
                        i64.const 0
                        local.get 14
                        i32.const 1049368
                        i32.add
                        i64.load
                        i64.const -1
                        i64.add
                        i64.const 0
                        call $__multi3
                        local.get 2
                        i32.const 32
                        i32.add
                        local.get 18
                        i64.const 0
                        local.get 17
                        i64.const 0
                        call $__multi3
                        local.get 2
                        i32.const 16
                        i32.add
                        local.get 2
                        i64.load offset=48
                        local.get 2
                        i64.load offset=56
                        local.get 11
                        i32.const 31651334
                        i32.mul
                        i32.const 19
                        i32.shr_u
                        local.get 20
                        i32.sub
                        local.tee 11
                        i32.const 127
                        i32.and
                        call $__lshrti3
                        local.get 2
                        local.get 2
                        i64.load offset=32
                        local.get 2
                        i64.load offset=40
                        i32.const 64
                        local.get 11
                        i32.sub
                        i32.const 127
                        i32.and
                        call $__ashlti3
                        local.get 2
                        i64.load offset=8
                        local.get 2
                        i64.load offset=24
                        i64.add
                        local.get 2
                        i64.load
                        local.tee 18
                        local.get 2
                        i64.load offset=16
                        i64.add
                        local.tee 17
                        local.get 18
                        i64.lt_u
                        i64.extend_i32_u
                        i64.add
                        local.get 17
                        local.get 19
                        i32.const 2
                        i32.shr_u
                        i32.const 1073741820
                        i32.and
                        i32.load offset=1048872
                        local.get 19
                        i32.const 1
                        i32.shl
                        i32.shr_u
                        i32.const 3
                        i32.and
                        i64.extend_i32_u
                        i64.add
                        local.tee 18
                        local.get 17
                        i64.lt_u
                        i64.extend_i32_u
                        i64.add
                        local.get 18
                        i64.const -1
                        i64.eq
                        i64.extend_i32_u
                        i64.add
                        local.set 17
                      end
                      local.get 17
                      i64.const 1
                      i64.add
                      local.tee 17
                      i64.const 4294967295
                      i64.and
                      local.get 21
                      i64.mul
                      i64.const 32
                      i64.shr_u
                      local.get 17
                      i64.const 32
                      i64.shr_u
                      local.get 21
                      i64.mul
                      i64.add
                      local.get 12
                      i32.const 63
                      i32.and
                      i32.const 32
                      i32.xor
                      i64.extend_i32_u
                      i64.shr_u
                      i32.wrap_i64
                      i32.const 10
                      i32.rem_u
                      local.set 19
                    end
                    local.get 10
                    i32.const 34
                    i32.lt_u
                    br_if 2 (;@6;)
                    i32.const 0
                    local.set 10
                    br 4 (;@4;)
                  end
                  local.get 4
                  i32.const -1
                  local.get 12
                  i32.const -1
                  i32.add
                  i32.shl
                  i32.const -1
                  i32.xor
                  i32.and
                  i32.eqz
                  local.set 10
                  br 3 (;@4;)
                end
                local.get 15
                local.get 9
                i32.sub
                local.set 15
                local.get 9
                i32.eqz
                local.get 7
                i32.and
                local.set 7
                i32.const 1
                local.set 10
                br 3 (;@3;)
              end
              block ;; label = @6
                local.get 4
                i32.const 5
                i32.rem_u
                br_if 0 (;@6;)
                i32.const 0
                local.set 3
                loop ;; label = @7
                  local.get 3
                  i32.const 1
                  i32.add
                  local.set 3
                  local.get 4
                  i32.const 5
                  i32.div_u
                  local.tee 4
                  i32.const 5
                  i32.rem_u
                  i32.eqz
                  br_if 0 (;@7;)
                end
                local.get 3
                local.get 13
                i32.ge_u
                local.set 10
                i32.const 0
                local.set 7
                br 2 (;@4;)
              end
              block ;; label = @6
                local.get 9
                i32.eqz
                br_if 0 (;@6;)
                i32.const 0
                local.set 7
                i32.const 0
                local.set 4
                block ;; label = @7
                  local.get 8
                  i32.const 5
                  i32.rem_u
                  br_if 0 (;@7;)
                  i32.const 0
                  local.set 4
                  loop ;; label = @8
                    local.get 4
                    i32.const 1
                    i32.add
                    local.set 4
                    local.get 8
                    i32.const 5
                    i32.div_u
                    local.tee 8
                    i32.const 5
                    i32.rem_u
                    i32.eqz
                    br_if 0 (;@8;)
                  end
                end
                local.get 15
                local.get 4
                local.get 13
                i32.ge_u
                i32.sub
                local.set 15
                i32.const 0
                local.set 10
                br 2 (;@4;)
              end
              i32.const 0
              local.set 10
              i32.const 0
              local.set 4
              block ;; label = @6
                local.get 3
                i32.const 5
                i32.rem_u
                br_if 0 (;@6;)
                i32.const 0
                local.set 4
                loop ;; label = @7
                  local.get 4
                  i32.const 1
                  i32.add
                  local.set 4
                  local.get 3
                  i32.const 5
                  i32.div_u
                  local.tee 3
                  i32.const 5
                  i32.rem_u
                  i32.eqz
                  br_if 0 (;@7;)
                end
              end
              local.get 4
              local.get 13
              i32.ge_u
              local.set 7
              br 1 (;@4;)
            end
            local.get 1
            local.get 6
            i32.add
            local.tee 5
            i32.const 0
            i32.load16_u offset=1049608 align=1
            i32.store16 align=1
            local.get 5
            i32.const 2
            i32.add
            i32.const 0
            i32.load8_u offset=1049610
            i32.store8
            local.get 3
            i32.const 31
            i32.shr_u
            i32.const 3
            i32.add
            local.set 7
            br 3 (;@1;)
          end
          local.get 7
          br_if 0 (;@3;)
          local.get 10
          br_if 0 (;@3;)
          i32.const 0
          local.set 4
          block ;; label = @4
            local.get 15
            i32.const 10
            i32.div_u
            local.tee 3
            local.get 16
            i32.const 10
            i32.div_u
            local.tee 8
            i32.le_u
            br_if 0 (;@4;)
            i32.const 0
            local.set 4
            loop ;; label = @5
              local.get 4
              i32.const 1
              i32.add
              local.set 4
              local.get 5
              local.tee 10
              i32.const 10
              i32.div_u
              local.set 5
              local.get 3
              i32.const 10
              i32.div_u
              local.tee 3
              local.get 8
              local.tee 16
              i32.const 10
              i32.div_u
              local.tee 8
              i32.gt_u
              br_if 0 (;@5;)
            end
            local.get 10
            local.get 5
            i32.const 10
            i32.mul
            i32.sub
            local.set 19
          end
          local.get 5
          local.get 16
          i32.eq
          local.get 19
          i32.const 255
          i32.and
          i32.const 4
          i32.gt_u
          i32.or
          local.set 3
          br 1 (;@2;)
        end
        i32.const 0
        local.set 4
        block ;; label = @3
          block ;; label = @4
            local.get 15
            i32.const 10
            i32.div_u
            local.tee 11
            local.get 16
            i32.const 10
            i32.div_u
            local.tee 12
            i32.gt_u
            br_if 0 (;@4;)
            local.get 16
            local.set 3
            local.get 5
            local.set 8
            local.get 19
            local.set 15
            br 1 (;@3;)
          end
          i32.const 0
          local.set 4
          loop ;; label = @4
            local.get 7
            local.get 12
            local.tee 3
            i32.const -10
            i32.mul
            i32.const 0
            local.get 16
            i32.sub
            i32.eq
            i32.and
            local.set 7
            local.get 4
            i32.const 1
            i32.add
            local.set 4
            local.get 10
            local.get 19
            i32.const 255
            i32.and
            i32.eqz
            i32.and
            local.set 10
            local.get 5
            local.get 5
            i32.const 10
            i32.div_u
            local.tee 8
            i32.const 10
            i32.mul
            i32.sub
            local.tee 15
            local.set 19
            local.get 8
            local.set 5
            local.get 3
            local.set 16
            local.get 11
            i32.const 10
            i32.div_u
            local.tee 11
            local.get 3
            i32.const 10
            i32.div_u
            local.tee 12
            i32.gt_u
            br_if 0 (;@4;)
          end
        end
        local.get 3
        i32.const 10
        i32.rem_u
        local.set 5
        block ;; label = @3
          block ;; label = @4
            local.get 7
            i32.eqz
            br_if 0 (;@4;)
            local.get 5
            br_if 0 (;@4;)
            loop ;; label = @5
              local.get 4
              i32.const 1
              i32.add
              local.set 4
              local.get 10
              local.get 15
              i32.const 255
              i32.and
              i32.eqz
              i32.and
              local.set 10
              local.get 8
              local.get 8
              i32.const 10
              i32.div_u
              local.tee 5
              i32.const 10
              i32.mul
              i32.sub
              local.tee 16
              local.set 15
              local.get 5
              local.set 8
              local.get 3
              i32.const 10
              i32.div_u
              local.tee 3
              i32.const 10
              i32.rem_u
              i32.eqz
              br_if 0 (;@5;)
              br 2 (;@3;)
            end
          end
          local.get 8
          local.set 5
          local.get 15
          local.set 16
        end
        local.get 5
        local.get 3
        i32.eq
        local.get 9
        i32.eqz
        local.get 7
        i32.and
        i32.const 1
        i32.xor
        i32.and
        i32.const 5
        i32.const 4
        local.get 5
        i32.const 1
        i32.and
        select
        local.get 16
        local.get 16
        i32.const 255
        i32.and
        i32.const 5
        i32.eq
        select
        local.get 16
        local.get 10
        i32.const 1
        i32.and
        select
        i32.const 255
        i32.and
        i32.const 4
        i32.gt_u
        i32.or
        local.set 3
      end
      local.get 13
      local.get 4
      i32.add
      local.set 7
      block ;; label = @2
        block ;; label = @3
          local.get 5
          local.get 3
          i32.add
          local.tee 5
          i32.const 99999999
          i32.le_u
          br_if 0 (;@3;)
          i32.const 9
          local.set 15
          br 1 (;@2;)
        end
        block ;; label = @3
          local.get 5
          i32.const 9999999
          i32.le_u
          br_if 0 (;@3;)
          i32.const 8
          local.set 15
          br 1 (;@2;)
        end
        block ;; label = @3
          local.get 5
          i32.const 999999
          i32.le_u
          br_if 0 (;@3;)
          i32.const 7
          local.set 15
          br 1 (;@2;)
        end
        block ;; label = @3
          local.get 5
          i32.const 99999
          i32.le_u
          br_if 0 (;@3;)
          i32.const 6
          local.set 15
          br 1 (;@2;)
        end
        block ;; label = @3
          local.get 5
          i32.const 9999
          i32.le_u
          br_if 0 (;@3;)
          i32.const 5
          local.set 15
          br 1 (;@2;)
        end
        block ;; label = @3
          local.get 5
          i32.const 999
          i32.le_u
          br_if 0 (;@3;)
          i32.const 4
          local.set 15
          br 1 (;@2;)
        end
        block ;; label = @3
          local.get 5
          i32.const 99
          i32.le_u
          br_if 0 (;@3;)
          i32.const 3
          local.set 15
          br 1 (;@2;)
        end
        i32.const 2
        i32.const 1
        local.get 5
        i32.const 9
        i32.gt_u
        select
        local.set 15
      end
      local.get 15
      local.get 7
      i32.add
      local.set 16
      block ;; label = @2
        block ;; label = @3
          block ;; label = @4
            block ;; label = @5
              block ;; label = @6
                block ;; label = @7
                  block ;; label = @8
                    block ;; label = @9
                      block ;; label = @10
                        block ;; label = @11
                          block ;; label = @12
                            block ;; label = @13
                              block ;; label = @14
                                local.get 7
                                i32.const 0
                                i32.lt_s
                                br_if 0 (;@14;)
                                local.get 16
                                i32.const 14
                                i32.lt_s
                                br_if 1 (;@13;)
                              end
                              local.get 16
                              i32.const -1
                              i32.add
                              local.tee 7
                              i32.const 13
                              i32.lt_u
                              br_if 1 (;@12;)
                              local.get 16
                              i32.const 5
                              i32.add
                              i32.const 6
                              i32.lt_u
                              br_if 2 (;@11;)
                              local.get 15
                              i32.const 1
                              i32.ne
                              br_if 4 (;@9;)
                              local.get 1
                              local.get 6
                              i32.add
                              local.tee 4
                              i32.const 101
                              i32.store8 offset=1
                              local.get 4
                              local.get 5
                              i32.const 48
                              i32.add
                              i32.store8
                              local.get 1
                              local.get 6
                              i32.const 2
                              i32.or
                              local.tee 4
                              i32.add
                              local.set 5
                              local.get 7
                              i32.const -1
                              i32.le_s
                              br_if 3 (;@10;)
                              local.get 7
                              local.set 3
                              br 11 (;@2;)
                            end
                            local.get 1
                            local.get 6
                            i32.add
                            local.get 15
                            i32.add
                            local.set 19
                            block ;; label = @13
                              local.get 5
                              i32.const 10000
                              i32.ge_u
                              br_if 0 (;@13;)
                              local.get 19
                              local.set 4
                              local.get 5
                              local.set 3
                              br 10 (;@3;)
                            end
                            local.get 15
                            local.get 6
                            i32.add
                            local.get 1
                            i32.add
                            i32.const -4
                            i32.add
                            local.set 4
                            loop ;; label = @13
                              local.get 4
                              local.get 5
                              i32.const 10000
                              i32.div_u
                              local.tee 3
                              i32.const -10000
                              i32.mul
                              local.get 5
                              i32.add
                              local.tee 8
                              i32.const 100
                              i32.div_u
                              local.tee 10
                              i32.const 1
                              i32.shl
                              i32.load16_u offset=1048586 align=1
                              i32.store16 align=1
                              local.get 4
                              i32.const 2
                              i32.add
                              local.get 8
                              local.get 10
                              i32.const 100
                              i32.mul
                              i32.sub
                              i32.const 1
                              i32.shl
                              i32.load16_u offset=1048586 align=1
                              i32.store16 align=1
                              local.get 4
                              i32.const -4
                              i32.add
                              local.set 4
                              local.get 5
                              i32.const 99999999
                              i32.gt_u
                              local.set 8
                              local.get 3
                              local.set 5
                              local.get 8
                              br_if 0 (;@13;)
                              br 9 (;@4;)
                            end
                          end
                          local.get 1
                          local.get 6
                          local.get 15
                          i32.add
                          i32.const 1
                          i32.add
                          local.tee 7
                          i32.add
                          local.set 4
                          block ;; label = @12
                            local.get 5
                            i32.const 10000
                            i32.ge_u
                            br_if 0 (;@12;)
                            local.get 4
                            local.set 8
                            local.get 5
                            local.set 3
                            br 7 (;@5;)
                          end
                          loop ;; label = @12
                            local.get 4
                            i32.const -4
                            i32.add
                            local.tee 8
                            local.get 5
                            i32.const 10000
                            i32.div_u
                            local.tee 3
                            i32.const -10000
                            i32.mul
                            local.get 5
                            i32.add
                            local.tee 10
                            i32.const 100
                            i32.div_u
                            local.tee 15
                            i32.const 1
                            i32.shl
                            i32.load16_u offset=1048586 align=1
                            i32.store16 align=1
                            local.get 4
                            i32.const -2
                            i32.add
                            local.get 10
                            local.get 15
                            i32.const 100
                            i32.mul
                            i32.sub
                            i32.const 1
                            i32.shl
                            i32.load16_u offset=1048586 align=1
                            i32.store16 align=1
                            local.get 5
                            i32.const 99999999
                            i32.gt_u
                            local.set 10
                            local.get 3
                            local.set 5
                            local.get 8
                            local.set 4
                            local.get 10
                            br_if 0 (;@12;)
                            br 7 (;@5;)
                          end
                        end
                        local.get 1
                        local.get 6
                        i32.add
                        local.tee 3
                        i32.const 11824
                        i32.store16 align=1
                        i32.const 2
                        local.get 16
                        i32.sub
                        local.set 4
                        block ;; label = @11
                          local.get 16
                          i32.const -1
                          i32.gt_s
                          br_if 0 (;@11;)
                          local.get 4
                          i32.const 3
                          local.get 4
                          i32.const 3
                          i32.gt_u
                          select
                          i32.const -2
                          i32.add
                          local.tee 8
                          i32.eqz
                          br_if 0 (;@11;)
                          local.get 3
                          i32.const 2
                          i32.add
                          i32.const 48
                          local.get 8
                          memory.fill
                        end
                        local.get 1
                        local.get 15
                        local.get 6
                        i32.add
                        local.get 4
                        i32.add
                        local.tee 7
                        i32.add
                        local.set 4
                        local.get 5
                        i32.const 10000
                        i32.ge_u
                        br_if 2 (;@8;)
                        local.get 4
                        local.set 8
                        local.get 5
                        local.set 3
                        br 4 (;@6;)
                      end
                      local.get 5
                      i32.const 45
                      i32.store8
                      local.get 5
                      i32.const 1
                      i32.add
                      local.set 5
                      i32.const 1
                      local.get 16
                      i32.sub
                      local.tee 3
                      i32.const 9
                      i32.gt_s
                      br_if 7 (;@2;)
                      local.get 5
                      local.get 3
                      i32.const 48
                      i32.add
                      i32.store8
                      i32.const 2
                      local.get 4
                      i32.add
                      local.set 7
                      br 8 (;@1;)
                    end
                    local.get 1
                    local.get 15
                    local.get 6
                    i32.add
                    local.tee 11
                    i32.add
                    i32.const 1
                    i32.add
                    local.set 19
                    block ;; label = @9
                      local.get 5
                      i32.const 10000
                      i32.ge_u
                      br_if 0 (;@9;)
                      local.get 19
                      local.set 8
                      local.get 5
                      local.set 3
                      br 2 (;@7;)
                    end
                    local.get 19
                    local.set 4
                    loop ;; label = @9
                      local.get 4
                      i32.const -4
                      i32.add
                      local.tee 8
                      local.get 5
                      i32.const 10000
                      i32.div_u
                      local.tee 3
                      i32.const -10000
                      i32.mul
                      local.get 5
                      i32.add
                      local.tee 10
                      i32.const 100
                      i32.div_u
                      local.tee 15
                      i32.const 1
                      i32.shl
                      i32.load16_u offset=1048586 align=1
                      i32.store16 align=1
                      local.get 4
                      i32.const -2
                      i32.add
                      local.get 10
                      local.get 15
                      i32.const 100
                      i32.mul
                      i32.sub
                      i32.const 1
                      i32.shl
                      i32.load16_u offset=1048586 align=1
                      i32.store16 align=1
                      local.get 5
                      i32.const 99999999
                      i32.gt_u
                      local.set 10
                      local.get 3
                      local.set 5
                      local.get 8
                      local.set 4
                      local.get 10
                      br_if 0 (;@9;)
                      br 2 (;@7;)
                    end
                  end
                  loop ;; label = @8
                    local.get 4
                    i32.const -4
                    i32.add
                    local.tee 8
                    local.get 5
                    i32.const 10000
                    i32.div_u
                    local.tee 3
                    i32.const -10000
                    i32.mul
                    local.get 5
                    i32.add
                    local.tee 10
                    i32.const 100
                    i32.div_u
                    local.tee 15
                    i32.const 1
                    i32.shl
                    i32.load16_u offset=1048586 align=1
                    i32.store16 align=1
                    local.get 4
                    i32.const -2
                    i32.add
                    local.get 10
                    local.get 15
                    i32.const 100
                    i32.mul
                    i32.sub
                    i32.const 1
                    i32.shl
                    i32.load16_u offset=1048586 align=1
                    i32.store16 align=1
                    local.get 5
                    i32.const 99999999
                    i32.gt_u
                    local.set 10
                    local.get 3
                    local.set 5
                    local.get 8
                    local.set 4
                    local.get 10
                    br_if 0 (;@8;)
                    br 2 (;@6;)
                  end
                end
                block ;; label = @7
                  block ;; label = @8
                    local.get 3
                    i32.const 99
                    i32.gt_u
                    br_if 0 (;@8;)
                    local.get 3
                    local.set 5
                    br 1 (;@7;)
                  end
                  local.get 8
                  i32.const -2
                  i32.add
                  local.tee 8
                  local.get 3
                  local.get 3
                  i32.const 65535
                  i32.and
                  i32.const 100
                  i32.div_u
                  local.tee 5
                  i32.const 100
                  i32.mul
                  i32.sub
                  i32.const 65535
                  i32.and
                  i32.const 1
                  i32.shl
                  i32.load16_u offset=1048586 align=1
                  i32.store16 align=1
                end
                block ;; label = @7
                  block ;; label = @8
                    local.get 5
                    i32.const 9
                    i32.gt_u
                    br_if 0 (;@8;)
                    local.get 8
                    i32.const -1
                    i32.add
                    local.get 5
                    i32.const 48
                    i32.or
                    i32.store8
                    br 1 (;@7;)
                  end
                  local.get 8
                  i32.const -2
                  i32.add
                  local.get 5
                  i32.const 1
                  i32.shl
                  i32.load16_u offset=1048586 align=1
                  i32.store16 align=1
                end
                local.get 1
                local.get 6
                i32.add
                local.tee 5
                local.get 5
                i32.load8_u offset=1
                i32.store8
                local.get 5
                i32.const 46
                i32.store8 offset=1
                local.get 19
                i32.const 101
                i32.store8
                local.get 1
                local.get 11
                i32.const 2
                i32.add
                local.tee 4
                i32.add
                local.set 5
                block ;; label = @7
                  block ;; label = @8
                    local.get 7
                    i32.const -1
                    i32.le_s
                    br_if 0 (;@8;)
                    local.get 7
                    local.set 3
                    br 1 (;@7;)
                  end
                  local.get 5
                  i32.const 45
                  i32.store8
                  local.get 5
                  i32.const 1
                  i32.add
                  local.set 5
                  i32.const 1
                  local.get 16
                  i32.sub
                  local.tee 3
                  i32.const 9
                  i32.gt_s
                  br_if 0 (;@7;)
                  local.get 5
                  local.get 3
                  i32.const 48
                  i32.add
                  i32.store8
                  i32.const 2
                  local.get 4
                  i32.add
                  local.set 7
                  br 6 (;@1;)
                end
                local.get 5
                local.get 3
                i32.const 1
                i32.shl
                i32.const 1048586
                i32.add
                i32.load16_u align=1
                i32.store16 align=1
                local.get 7
                i32.const 31
                i32.shr_u
                i32.const 2
                i32.or
                local.get 4
                i32.add
                local.set 7
                br 5 (;@1;)
              end
              block ;; label = @6
                block ;; label = @7
                  local.get 3
                  i32.const 99
                  i32.gt_u
                  br_if 0 (;@7;)
                  local.get 3
                  local.set 5
                  br 1 (;@6;)
                end
                local.get 8
                i32.const -2
                i32.add
                local.tee 8
                local.get 3
                local.get 3
                i32.const 65535
                i32.and
                i32.const 100
                i32.div_u
                local.tee 5
                i32.const 100
                i32.mul
                i32.sub
                i32.const 65535
                i32.and
                i32.const 1
                i32.shl
                i32.load16_u offset=1048586 align=1
                i32.store16 align=1
              end
              block ;; label = @6
                local.get 5
                i32.const 9
                i32.gt_u
                br_if 0 (;@6;)
                local.get 8
                i32.const -1
                i32.add
                local.get 5
                i32.const 48
                i32.or
                i32.store8
                br 5 (;@1;)
              end
              local.get 8
              i32.const -2
              i32.add
              local.get 5
              i32.const 1
              i32.shl
              i32.load16_u offset=1048586 align=1
              i32.store16 align=1
              br 4 (;@1;)
            end
            block ;; label = @5
              block ;; label = @6
                local.get 3
                i32.const 99
                i32.gt_u
                br_if 0 (;@6;)
                local.get 3
                local.set 5
                br 1 (;@5;)
              end
              local.get 8
              i32.const -2
              i32.add
              local.tee 8
              local.get 3
              local.get 3
              i32.const 65535
              i32.and
              i32.const 100
              i32.div_u
              local.tee 5
              i32.const 100
              i32.mul
              i32.sub
              i32.const 65535
              i32.and
              i32.const 1
              i32.shl
              i32.load16_u offset=1048586 align=1
              i32.store16 align=1
            end
            block ;; label = @5
              block ;; label = @6
                local.get 5
                i32.const 9
                i32.gt_u
                br_if 0 (;@6;)
                local.get 8
                i32.const -1
                i32.add
                local.get 5
                i32.const 48
                i32.or
                i32.store8
                br 1 (;@5;)
              end
              local.get 8
              i32.const -2
              i32.add
              local.get 5
              i32.const 1
              i32.shl
              i32.load16_u offset=1048586 align=1
              i32.store16 align=1
            end
            local.get 1
            local.get 6
            i32.add
            local.set 5
            block ;; label = @5
              local.get 16
              i32.eqz
              br_if 0 (;@5;)
              local.get 5
              local.get 5
              i32.const 1
              i32.add
              local.get 16
              memory.copy
            end
            local.get 5
            local.get 16
            i32.add
            i32.const 46
            i32.store8
            br 3 (;@1;)
          end
          local.get 4
          i32.const 4
          i32.add
          local.set 4
        end
        block ;; label = @3
          block ;; label = @4
            local.get 3
            i32.const 99
            i32.gt_u
            br_if 0 (;@4;)
            local.get 3
            local.set 5
            br 1 (;@3;)
          end
          local.get 4
          i32.const -2
          i32.add
          local.tee 4
          local.get 3
          local.get 3
          i32.const 65535
          i32.and
          i32.const 100
          i32.div_u
          local.tee 5
          i32.const 100
          i32.mul
          i32.sub
          i32.const 65535
          i32.and
          i32.const 1
          i32.shl
          i32.load16_u offset=1048586 align=1
          i32.store16 align=1
        end
        block ;; label = @3
          block ;; label = @4
            local.get 5
            i32.const 9
            i32.gt_u
            br_if 0 (;@4;)
            local.get 4
            i32.const -1
            i32.add
            local.get 5
            i32.const 48
            i32.or
            i32.store8
            br 1 (;@3;)
          end
          local.get 4
          i32.const -2
          i32.add
          local.get 5
          i32.const 1
          i32.shl
          i32.load16_u offset=1048586 align=1
          i32.store16 align=1
        end
        block ;; label = @3
          local.get 15
          local.get 16
          i32.ge_s
          br_if 0 (;@3;)
          local.get 7
          i32.eqz
          br_if 0 (;@3;)
          local.get 19
          i32.const 48
          local.get 7
          memory.fill
        end
        local.get 1
        local.get 16
        local.get 6
        i32.add
        local.tee 5
        i32.add
        i32.const 12334
        i32.store16 align=1
        local.get 5
        i32.const 2
        i32.add
        local.set 7
        br 1 (;@1;)
      end
      local.get 5
      local.get 3
      i32.const 1
      i32.shl
      i32.const 1048586
      i32.add
      i32.load16_u align=1
      i32.store16 align=1
      local.get 7
      i32.const 31
      i32.shr_u
      i32.const 2
      i32.or
      local.get 4
      i32.add
      local.set 7
    end
    local.get 2
    i32.const 512
    i32.add
    global.set $__stack_pointer
    local.get 7
  )
  (func $_ZN3ryu6pretty8format6417h1c34065fb67249adE (;3;) (type 1) (param f64 i32) (result i32)
    (local i32 i64 i64 i32 i32 i32 i64 i32 i32 i32 i32 i32 i64 i64 i64 i64)
    global.get $__stack_pointer
    i32.const 416
    i32.sub
    local.tee 2
    global.set $__stack_pointer
    local.get 0
    i64.reinterpret_f64
    local.tee 3
    i64.const 4503599627370495
    i64.and
    local.set 4
    local.get 3
    i64.const 52
    i64.shr_u
    i32.wrap_i64
    local.set 5
    i32.const 0
    local.set 6
    block ;; label = @1
      local.get 3
      i64.const 0
      i64.ge_s
      br_if 0 (;@1;)
      local.get 1
      i32.const 45
      i32.store8
      i32.const 1
      local.set 6
    end
    local.get 5
    i32.const 2047
    i32.and
    local.set 5
    block ;; label = @1
      block ;; label = @2
        block ;; label = @3
          block ;; label = @4
            block ;; label = @5
              block ;; label = @6
                block ;; label = @7
                  block ;; label = @8
                    block ;; label = @9
                      block ;; label = @10
                        block ;; label = @11
                          block ;; label = @12
                            local.get 4
                            i64.const 0
                            i64.ne
                            br_if 0 (;@12;)
                            local.get 5
                            i32.eqz
                            br_if 1 (;@11;)
                          end
                          local.get 4
                          i64.const 0
                          i64.ne
                          local.get 5
                          i32.const 2
                          i32.lt_u
                          i32.or
                          local.set 7
                          local.get 4
                          i64.const 4503599627370496
                          i64.or
                          local.get 4
                          local.get 5
                          select
                          local.tee 4
                          i64.const 2
                          i64.shl
                          local.set 3
                          local.get 4
                          i64.const 1
                          i64.and
                          local.set 8
                          block ;; label = @12
                            block ;; label = @13
                              block ;; label = @14
                                block ;; label = @15
                                  local.get 5
                                  i32.const -1077
                                  i32.add
                                  i32.const -1076
                                  local.get 5
                                  select
                                  local.tee 5
                                  i32.const -1
                                  i32.gt_s
                                  br_if 0 (;@15;)
                                  local.get 5
                                  i32.const -732923
                                  i32.mul
                                  i32.const 20
                                  i32.shr_u
                                  local.get 5
                                  i32.const -1
                                  i32.ne
                                  i32.sub
                                  local.tee 9
                                  local.get 9
                                  local.get 5
                                  i32.add
                                  local.tee 10
                                  i32.const 65891505
                                  i32.mul
                                  i32.const 19
                                  i32.shr_u
                                  i32.sub
                                  i32.const 124
                                  i32.add
                                  local.set 11
                                  i32.const 0
                                  local.get 10
                                  i32.sub
                                  local.tee 5
                                  i32.const 65535
                                  i32.and
                                  i32.const 26
                                  i32.div_u
                                  local.tee 12
                                  i32.const 4
                                  i32.shl
                                  local.tee 13
                                  i64.load offset=1049168
                                  local.set 4
                                  local.get 13
                                  i64.load offset=1049160
                                  local.set 14
                                  block ;; label = @16
                                    local.get 5
                                    local.get 12
                                    i32.const 26
                                    i32.mul
                                    local.tee 13
                                    i32.eq
                                    br_if 0 (;@16;)
                                    local.get 2
                                    i32.const 400
                                    i32.add
                                    local.get 5
                                    local.get 13
                                    i32.sub
                                    i32.const 3
                                    i32.shl
                                    i64.load offset=1048952
                                    local.tee 15
                                    i64.const 0
                                    local.get 14
                                    i64.const 0
                                    call $__multi3
                                    local.get 2
                                    i32.const 384
                                    i32.add
                                    local.get 15
                                    i64.const 0
                                    local.get 4
                                    i64.const 0
                                    call $__multi3
                                    local.get 2
                                    i32.const 368
                                    i32.add
                                    local.get 2
                                    i64.load offset=400
                                    local.get 2
                                    i64.load offset=408
                                    local.get 10
                                    i32.const -1217359
                                    i32.mul
                                    i32.const 19
                                    i32.shr_u
                                    local.get 12
                                    i32.const 31651334
                                    i32.mul
                                    i32.const 19
                                    i32.shr_u
                                    i32.sub
                                    local.tee 12
                                    i32.const 127
                                    i32.and
                                    call $__lshrti3
                                    local.get 2
                                    i32.const 352
                                    i32.add
                                    local.get 2
                                    i64.load offset=384
                                    local.get 2
                                    i64.load offset=392
                                    i32.const 64
                                    local.get 12
                                    i32.sub
                                    i32.const 127
                                    i32.and
                                    call $__ashlti3
                                    local.get 2
                                    i64.load offset=376
                                    local.get 2
                                    i64.load offset=360
                                    i64.add
                                    local.get 2
                                    i64.load offset=368
                                    local.tee 14
                                    local.get 2
                                    i64.load offset=352
                                    i64.add
                                    local.tee 4
                                    local.get 14
                                    i64.lt_u
                                    i64.extend_i32_u
                                    i64.add
                                    local.get 4
                                    local.get 5
                                    i32.const 2
                                    i32.shr_u
                                    i32.const 1073741820
                                    i32.and
                                    i32.load offset=1048788
                                    local.get 5
                                    i32.const 1
                                    i32.shl
                                    i32.shr_u
                                    i32.const 3
                                    i32.and
                                    i64.extend_i32_u
                                    i64.add
                                    local.tee 14
                                    local.get 4
                                    i64.lt_u
                                    i64.extend_i32_u
                                    i64.add
                                    local.set 4
                                  end
                                  local.get 2
                                  i32.const 336
                                  i32.add
                                  local.get 14
                                  i64.const 0
                                  local.get 3
                                  i64.const 2
                                  i64.or
                                  local.tee 15
                                  i64.const 0
                                  call $__multi3
                                  local.get 2
                                  i32.const 304
                                  i32.add
                                  local.get 4
                                  i64.const 0
                                  local.get 15
                                  i64.const 0
                                  call $__multi3
                                  local.get 2
                                  i32.const 272
                                  i32.add
                                  local.get 2
                                  i64.load offset=344
                                  local.tee 15
                                  local.get 2
                                  i64.load offset=304
                                  i64.add
                                  local.tee 16
                                  local.get 2
                                  i64.load offset=312
                                  local.get 16
                                  local.get 15
                                  i64.lt_u
                                  i64.extend_i32_u
                                  i64.add
                                  local.get 11
                                  i32.const 127
                                  i32.and
                                  i32.const 64
                                  i32.xor
                                  local.tee 5
                                  call $__lshrti3
                                  local.get 2
                                  i32.const 240
                                  i32.add
                                  local.get 14
                                  i64.const 0
                                  local.get 3
                                  local.get 7
                                  i32.const -1
                                  i32.xor
                                  i64.extend_i32_s
                                  i64.add
                                  local.tee 15
                                  i64.const 0
                                  call $__multi3
                                  local.get 2
                                  i32.const 224
                                  i32.add
                                  local.get 4
                                  i64.const 0
                                  local.get 15
                                  i64.const 0
                                  call $__multi3
                                  local.get 2
                                  i32.const 208
                                  i32.add
                                  local.get 2
                                  i64.load offset=248
                                  local.tee 15
                                  local.get 2
                                  i64.load offset=224
                                  i64.add
                                  local.tee 16
                                  local.get 2
                                  i64.load offset=232
                                  local.get 16
                                  local.get 15
                                  i64.lt_u
                                  i64.extend_i32_u
                                  i64.add
                                  local.get 5
                                  call $__lshrti3
                                  local.get 2
                                  i32.const 320
                                  i32.add
                                  local.get 14
                                  i64.const 0
                                  local.get 3
                                  i64.const 0
                                  call $__multi3
                                  local.get 2
                                  i32.const 288
                                  i32.add
                                  local.get 4
                                  i64.const 0
                                  local.get 3
                                  i64.const 0
                                  call $__multi3
                                  local.get 2
                                  i32.const 256
                                  i32.add
                                  local.get 2
                                  i64.load offset=328
                                  local.tee 4
                                  local.get 2
                                  i64.load offset=288
                                  i64.add
                                  local.tee 14
                                  local.get 2
                                  i64.load offset=296
                                  local.get 14
                                  local.get 4
                                  i64.lt_u
                                  i64.extend_i32_u
                                  i64.add
                                  local.get 5
                                  call $__lshrti3
                                  local.get 2
                                  i64.load offset=256
                                  local.set 14
                                  local.get 2
                                  i64.load offset=272
                                  local.set 17
                                  local.get 2
                                  i64.load offset=208
                                  local.set 15
                                  local.get 9
                                  i32.const 2
                                  i32.lt_u
                                  br_if 1 (;@14;)
                                  i32.const 0
                                  local.set 12
                                  local.get 9
                                  i32.const 63
                                  i32.lt_u
                                  br_if 2 (;@13;)
                                  i32.const 0
                                  local.set 9
                                  br 5 (;@10;)
                                end
                                local.get 5
                                i32.const 78913
                                i32.mul
                                i32.const 18
                                i32.shr_u
                                local.get 5
                                i32.const 3
                                i32.gt_u
                                i32.sub
                                local.tee 10
                                local.get 5
                                i32.sub
                                local.get 10
                                i32.const 1217359
                                i32.mul
                                i32.const 19
                                i32.shr_u
                                local.tee 11
                                i32.add
                                i32.const 125
                                i32.add
                                local.set 9
                                local.get 10
                                i32.const 25
                                i32.add
                                i32.const 65535
                                i32.and
                                i32.const 26
                                i32.div_u
                                local.tee 5
                                i32.const 4
                                i32.shl
                                local.tee 12
                                i64.load offset=1049376
                                local.set 14
                                local.get 12
                                i64.load offset=1049368
                                local.set 15
                                block ;; label = @15
                                  local.get 5
                                  i32.const 26
                                  i32.mul
                                  local.tee 12
                                  local.get 10
                                  i32.eq
                                  br_if 0 (;@15;)
                                  local.get 2
                                  i32.const 192
                                  i32.add
                                  local.get 12
                                  local.get 10
                                  i32.sub
                                  i32.const 3
                                  i32.shl
                                  i64.load offset=1048952
                                  local.tee 4
                                  i64.const 0
                                  local.get 15
                                  i64.const -1
                                  i64.add
                                  i64.const 0
                                  call $__multi3
                                  local.get 2
                                  i32.const 176
                                  i32.add
                                  local.get 4
                                  i64.const 0
                                  local.get 14
                                  i64.const 0
                                  call $__multi3
                                  local.get 2
                                  i32.const 160
                                  i32.add
                                  local.get 2
                                  i64.load offset=192
                                  local.get 2
                                  i64.load offset=200
                                  local.get 5
                                  i32.const 31651334
                                  i32.mul
                                  i32.const 19
                                  i32.shr_u
                                  local.get 11
                                  i32.sub
                                  local.tee 5
                                  i32.const 127
                                  i32.and
                                  call $__lshrti3
                                  local.get 2
                                  i32.const 144
                                  i32.add
                                  local.get 2
                                  i64.load offset=176
                                  local.get 2
                                  i64.load offset=184
                                  i32.const 64
                                  local.get 5
                                  i32.sub
                                  i32.const 127
                                  i32.and
                                  call $__ashlti3
                                  local.get 2
                                  i64.load offset=152
                                  local.get 2
                                  i64.load offset=168
                                  i64.add
                                  local.get 2
                                  i64.load offset=144
                                  local.tee 14
                                  local.get 2
                                  i64.load offset=160
                                  i64.add
                                  local.tee 4
                                  local.get 14
                                  i64.lt_u
                                  i64.extend_i32_u
                                  i64.add
                                  local.get 4
                                  local.get 10
                                  i32.const 2
                                  i32.shr_u
                                  i32.const 1073741820
                                  i32.and
                                  i32.load offset=1048872
                                  local.get 10
                                  i32.const 1
                                  i32.shl
                                  i32.shr_u
                                  i32.const 3
                                  i32.and
                                  i64.extend_i32_u
                                  i64.add
                                  local.tee 14
                                  local.get 4
                                  i64.lt_u
                                  i64.extend_i32_u
                                  i64.add
                                  local.get 14
                                  i64.const 1
                                  i64.add
                                  local.tee 15
                                  i64.eqz
                                  i64.extend_i32_u
                                  i64.add
                                  local.set 14
                                end
                                local.get 2
                                i32.const 128
                                i32.add
                                local.get 15
                                i64.const 0
                                local.get 3
                                i64.const 2
                                i64.or
                                local.tee 4
                                i64.const 0
                                call $__multi3
                                local.get 2
                                i32.const 96
                                i32.add
                                local.get 14
                                i64.const 0
                                local.get 4
                                i64.const 0
                                call $__multi3
                                local.get 2
                                i32.const 64
                                i32.add
                                local.get 2
                                i64.load offset=136
                                local.tee 16
                                local.get 2
                                i64.load offset=96
                                i64.add
                                local.tee 17
                                local.get 2
                                i64.load offset=104
                                local.get 17
                                local.get 16
                                i64.lt_u
                                i64.extend_i32_u
                                i64.add
                                local.get 9
                                i32.const 127
                                i32.and
                                i32.const 64
                                i32.xor
                                local.tee 5
                                call $__lshrti3
                                local.get 2
                                i32.const 32
                                i32.add
                                local.get 15
                                i64.const 0
                                local.get 3
                                local.get 7
                                i32.const -1
                                i32.xor
                                i64.extend_i32_s
                                i64.add
                                local.tee 16
                                i64.const 0
                                call $__multi3
                                local.get 2
                                i32.const 16
                                i32.add
                                local.get 14
                                i64.const 0
                                local.get 16
                                i64.const 0
                                call $__multi3
                                local.get 2
                                local.get 2
                                i64.load offset=40
                                local.tee 16
                                local.get 2
                                i64.load offset=16
                                i64.add
                                local.tee 17
                                local.get 2
                                i64.load offset=24
                                local.get 17
                                local.get 16
                                i64.lt_u
                                i64.extend_i32_u
                                i64.add
                                local.get 5
                                call $__lshrti3
                                local.get 2
                                i32.const 112
                                i32.add
                                local.get 15
                                i64.const 0
                                local.get 3
                                i64.const 0
                                call $__multi3
                                local.get 2
                                i32.const 80
                                i32.add
                                local.get 14
                                i64.const 0
                                local.get 3
                                i64.const 0
                                call $__multi3
                                local.get 2
                                i32.const 48
                                i32.add
                                local.get 2
                                i64.load offset=120
                                local.tee 14
                                local.get 2
                                i64.load offset=80
                                i64.add
                                local.tee 15
                                local.get 2
                                i64.load offset=88
                                local.get 15
                                local.get 14
                                i64.lt_u
                                i64.extend_i32_u
                                i64.add
                                local.get 5
                                call $__lshrti3
                                i32.const 0
                                local.set 12
                                local.get 2
                                i64.load offset=48
                                local.set 14
                                local.get 2
                                i64.load offset=64
                                local.set 17
                                local.get 2
                                i64.load
                                local.set 15
                                local.get 10
                                i32.const 22
                                i32.lt_u
                                br_if 2 (;@12;)
                                i32.const 0
                                local.set 9
                                br 4 (;@10;)
                              end
                              local.get 17
                              local.get 8
                              i64.sub
                              local.set 17
                              local.get 8
                              i64.eqz
                              local.get 7
                              i32.and
                              local.set 12
                              i32.const 1
                              local.set 9
                              br 4 (;@9;)
                            end
                            local.get 3
                            i64.const -1
                            local.get 9
                            i64.extend_i32_u
                            i64.shl
                            i64.const -1
                            i64.xor
                            i64.and
                            i64.eqz
                            local.set 9
                            br 2 (;@10;)
                          end
                          block ;; label = @12
                            local.get 3
                            i64.const 5
                            i64.div_u
                            i32.wrap_i64
                            i32.const -5
                            i32.mul
                            i32.const 0
                            local.get 3
                            i32.wrap_i64
                            i32.sub
                            i32.ne
                            br_if 0 (;@12;)
                            i32.const -1
                            local.set 5
                            loop ;; label = @13
                              local.get 5
                              i32.const 1
                              i32.add
                              local.set 5
                              local.get 3
                              i64.const -3689348814741910323
                              i64.mul
                              local.tee 3
                              i64.const 3689348814741910324
                              i64.lt_u
                              br_if 0 (;@13;)
                            end
                            local.get 5
                            local.get 10
                            i32.ge_u
                            local.set 9
                            i32.const 0
                            local.set 12
                            br 2 (;@10;)
                          end
                          block ;; label = @12
                            local.get 8
                            i64.eqz
                            br_if 0 (;@12;)
                            i32.const -1
                            local.set 5
                            loop ;; label = @13
                              local.get 5
                              i32.const 1
                              i32.add
                              local.set 5
                              local.get 4
                              i64.const -3689348814741910323
                              i64.mul
                              local.tee 4
                              i64.const 3689348814741910324
                              i64.lt_u
                              br_if 0 (;@13;)
                            end
                            local.get 17
                            local.get 5
                            local.get 10
                            i32.ge_u
                            i64.extend_i32_u
                            i64.sub
                            local.set 17
                            i32.const 0
                            local.set 12
                            i32.const 0
                            local.set 9
                            br 2 (;@10;)
                          end
                          local.get 7
                          i64.extend_i32_u
                          i64.const -1
                          i64.xor
                          local.get 3
                          i64.add
                          local.set 3
                          i32.const -1
                          local.set 5
                          loop ;; label = @12
                            local.get 5
                            i32.const 1
                            i32.add
                            local.set 5
                            local.get 3
                            i64.const -3689348814741910323
                            i64.mul
                            local.tee 3
                            i64.const 3689348814741910324
                            i64.lt_u
                            br_if 0 (;@12;)
                          end
                          local.get 5
                          local.get 10
                          i32.ge_u
                          local.set 12
                          i32.const 0
                          local.set 9
                          br 1 (;@10;)
                        end
                        local.get 1
                        local.get 6
                        i32.add
                        local.tee 5
                        i32.const 0
                        i32.load16_u offset=1049608 align=1
                        i32.store16 align=1
                        local.get 5
                        i32.const 2
                        i32.add
                        i32.const 0
                        i32.load8_u offset=1049610
                        i32.store8
                        local.get 3
                        i64.const 63
                        i64.shr_u
                        i32.wrap_i64
                        i32.const 3
                        i32.add
                        local.set 7
                        br 9 (;@1;)
                      end
                      local.get 12
                      br_if 0 (;@9;)
                      local.get 9
                      i32.eqz
                      br_if 1 (;@8;)
                    end
                    i32.const 0
                    local.set 7
                    local.get 17
                    i64.const 10
                    i64.div_u
                    local.tee 16
                    local.get 15
                    i64.const 10
                    i64.div_u
                    local.tee 17
                    i64.gt_u
                    br_if 1 (;@7;)
                    i32.const 0
                    local.set 5
                    local.get 15
                    local.set 3
                    local.get 14
                    local.set 4
                    br 2 (;@6;)
                  end
                  i32.const 0
                  local.set 5
                  local.get 17
                  i64.const 100
                  i64.div_u
                  local.tee 3
                  local.get 15
                  i64.const 100
                  i64.div_u
                  local.tee 16
                  i64.gt_u
                  br_if 2 (;@5;)
                  i32.const 0
                  local.set 7
                  local.get 15
                  local.set 16
                  local.get 17
                  local.set 3
                  local.get 14
                  local.set 15
                  br 4 (;@3;)
                end
                i32.const 0
                local.set 5
                i32.const 0
                local.set 7
                loop ;; label = @7
                  local.get 12
                  local.get 17
                  local.tee 3
                  i32.wrap_i64
                  i32.const -10
                  i32.mul
                  i32.const 0
                  local.get 15
                  i32.wrap_i64
                  i32.sub
                  i32.eq
                  i32.and
                  local.set 12
                  local.get 5
                  i32.const 1
                  i32.add
                  local.set 5
                  local.get 9
                  local.get 7
                  i32.const 255
                  i32.and
                  i32.eqz
                  i32.and
                  local.set 9
                  local.get 14
                  i64.const 10
                  i64.div_u
                  local.tee 4
                  i32.wrap_i64
                  i32.const -10
                  i32.mul
                  local.get 14
                  i32.wrap_i64
                  i32.add
                  local.set 7
                  local.get 4
                  local.set 14
                  local.get 3
                  local.set 15
                  local.get 16
                  i64.const 10
                  i64.div_u
                  local.tee 16
                  local.get 3
                  i64.const 10
                  i64.div_u
                  local.tee 17
                  i64.gt_u
                  br_if 0 (;@7;)
                end
              end
              block ;; label = @6
                block ;; label = @7
                  local.get 12
                  i32.eqz
                  br_if 0 (;@7;)
                  local.get 3
                  i64.const 10
                  i64.div_u
                  local.tee 14
                  i32.wrap_i64
                  i32.const -10
                  i32.mul
                  i32.const 0
                  local.get 3
                  i32.wrap_i64
                  i32.sub
                  i32.eq
                  br_if 1 (;@6;)
                end
                local.get 4
                local.set 15
                br 2 (;@4;)
              end
              loop ;; label = @6
                local.get 14
                i32.wrap_i64
                local.set 11
                local.get 5
                i32.const 1
                i32.add
                local.set 5
                local.get 9
                local.get 7
                i32.const 255
                i32.and
                i32.eqz
                i32.and
                local.set 9
                local.get 4
                i64.const 10
                i64.div_u
                local.tee 15
                i32.wrap_i64
                i32.const -10
                i32.mul
                local.get 4
                i32.wrap_i64
                i32.add
                local.set 7
                local.get 14
                local.set 3
                local.get 14
                i64.const 10
                i64.div_u
                local.tee 16
                local.set 14
                local.get 15
                local.set 4
                local.get 16
                i32.wrap_i64
                i32.const -10
                i32.mul
                i32.const 0
                local.get 11
                i32.sub
                i32.eq
                br_if 0 (;@6;)
                br 2 (;@4;)
              end
            end
            local.get 14
            i64.const 100
            i64.div_u
            local.tee 15
            i32.wrap_i64
            i32.const -100
            i32.mul
            local.get 14
            i32.wrap_i64
            i32.add
            i32.const 49
            i32.gt_u
            local.set 7
            i32.const 2
            local.set 5
            br 1 (;@3;)
          end
          local.get 15
          local.get 3
          i64.eq
          local.get 8
          i64.eqz
          local.get 12
          i32.and
          i32.const 1
          i32.xor
          i32.and
          i32.const 4
          i32.const 5
          local.get 15
          i64.const 1
          i64.and
          i64.eqz
          select
          local.get 7
          local.get 7
          i32.const 255
          i32.and
          i32.const 5
          i32.eq
          select
          local.get 7
          local.get 9
          i32.const 1
          i32.and
          select
          i32.const 255
          i32.and
          i32.const 4
          i32.gt_u
          i32.or
          local.set 7
          br 1 (;@2;)
        end
        block ;; label = @3
          local.get 3
          i64.const 10
          i64.div_u
          local.tee 3
          local.get 16
          i64.const 10
          i64.div_u
          local.tee 4
          i64.le_u
          br_if 0 (;@3;)
          loop ;; label = @4
            local.get 5
            i32.const 1
            i32.add
            local.set 5
            local.get 15
            local.tee 14
            i64.const 10
            i64.div_u
            local.set 15
            local.get 3
            i64.const 10
            i64.div_u
            local.tee 3
            local.get 4
            local.tee 16
            i64.const 10
            i64.div_u
            local.tee 4
            i64.gt_u
            br_if 0 (;@4;)
          end
          local.get 15
          i32.wrap_i64
          i32.const -10
          i32.mul
          local.get 14
          i32.wrap_i64
          i32.add
          i32.const 4
          i32.gt_u
          local.set 7
        end
        local.get 15
        local.get 16
        i64.eq
        local.get 7
        i32.or
        local.set 7
      end
      local.get 10
      local.get 5
      i32.add
      local.set 9
      block ;; label = @2
        block ;; label = @3
          local.get 15
          local.get 7
          i64.extend_i32_u
          i64.const 1
          i64.and
          i64.add
          local.tee 3
          i64.const 9999999999999999
          i64.le_u
          br_if 0 (;@3;)
          i32.const 17
          local.set 7
          br 1 (;@2;)
        end
        block ;; label = @3
          local.get 3
          i64.const 999999999999999
          i64.le_u
          br_if 0 (;@3;)
          i32.const 16
          local.set 7
          br 1 (;@2;)
        end
        block ;; label = @3
          local.get 3
          i64.const 99999999999999
          i64.le_u
          br_if 0 (;@3;)
          i32.const 15
          local.set 7
          br 1 (;@2;)
        end
        block ;; label = @3
          local.get 3
          i64.const 9999999999999
          i64.le_u
          br_if 0 (;@3;)
          i32.const 14
          local.set 7
          br 1 (;@2;)
        end
        block ;; label = @3
          local.get 3
          i64.const 999999999999
          i64.le_u
          br_if 0 (;@3;)
          i32.const 13
          local.set 7
          br 1 (;@2;)
        end
        block ;; label = @3
          local.get 3
          i64.const 99999999999
          i64.le_u
          br_if 0 (;@3;)
          i32.const 12
          local.set 7
          br 1 (;@2;)
        end
        block ;; label = @3
          local.get 3
          i64.const 9999999999
          i64.le_u
          br_if 0 (;@3;)
          i32.const 11
          local.set 7
          br 1 (;@2;)
        end
        block ;; label = @3
          local.get 3
          i64.const 999999999
          i64.le_u
          br_if 0 (;@3;)
          i32.const 10
          local.set 7
          br 1 (;@2;)
        end
        block ;; label = @3
          local.get 3
          i64.const 99999999
          i64.le_u
          br_if 0 (;@3;)
          i32.const 9
          local.set 7
          br 1 (;@2;)
        end
        block ;; label = @3
          local.get 3
          i64.const 9999999
          i64.le_u
          br_if 0 (;@3;)
          i32.const 8
          local.set 7
          br 1 (;@2;)
        end
        block ;; label = @3
          local.get 3
          i64.const 999999
          i64.le_u
          br_if 0 (;@3;)
          i32.const 7
          local.set 7
          br 1 (;@2;)
        end
        block ;; label = @3
          local.get 3
          i64.const 99999
          i64.le_u
          br_if 0 (;@3;)
          i32.const 6
          local.set 7
          br 1 (;@2;)
        end
        block ;; label = @3
          local.get 3
          i64.const 9999
          i64.le_u
          br_if 0 (;@3;)
          i32.const 5
          local.set 7
          br 1 (;@2;)
        end
        block ;; label = @3
          local.get 3
          i64.const 999
          i64.le_u
          br_if 0 (;@3;)
          i32.const 4
          local.set 7
          br 1 (;@2;)
        end
        block ;; label = @3
          local.get 3
          i64.const 99
          i64.le_u
          br_if 0 (;@3;)
          i32.const 3
          local.set 7
          br 1 (;@2;)
        end
        i32.const 2
        i32.const 1
        local.get 3
        i64.const 9
        i64.gt_u
        select
        local.set 7
      end
      local.get 7
      local.get 9
      i32.add
      local.set 5
      block ;; label = @2
        block ;; label = @3
          block ;; label = @4
            block ;; label = @5
              block ;; label = @6
                block ;; label = @7
                  block ;; label = @8
                    block ;; label = @9
                      block ;; label = @10
                        block ;; label = @11
                          local.get 9
                          i32.const 0
                          i32.lt_s
                          br_if 0 (;@11;)
                          local.get 5
                          i32.const 17
                          i32.lt_s
                          br_if 1 (;@10;)
                        end
                        local.get 5
                        i32.const -1
                        i32.add
                        local.tee 9
                        i32.const 16
                        i32.lt_u
                        br_if 1 (;@9;)
                        local.get 5
                        i32.const 4
                        i32.add
                        i32.const 5
                        i32.lt_u
                        br_if 2 (;@8;)
                        local.get 7
                        i32.const 1
                        i32.ne
                        br_if 5 (;@5;)
                        local.get 1
                        local.get 6
                        i32.add
                        local.tee 7
                        i32.const 101
                        i32.store8 offset=1
                        local.get 7
                        local.get 3
                        i32.wrap_i64
                        i32.const 48
                        i32.add
                        i32.store8
                        local.get 1
                        local.get 6
                        i32.const 2
                        i32.or
                        local.tee 12
                        i32.add
                        local.set 7
                        local.get 9
                        i32.const 0
                        i32.lt_s
                        br_if 3 (;@7;)
                        local.get 9
                        local.set 5
                        br 4 (;@6;)
                      end
                      local.get 3
                      local.get 1
                      local.get 6
                      i32.add
                      local.get 7
                      i32.add
                      call $_ZN3ryu6pretty8mantissa19write_mantissa_long17h12d0ca81b2e76182E
                      block ;; label = @10
                        local.get 7
                        local.get 5
                        i32.ge_s
                        br_if 0 (;@10;)
                        local.get 9
                        i32.eqz
                        br_if 0 (;@10;)
                        local.get 1
                        local.get 7
                        i32.add
                        local.get 6
                        i32.add
                        i32.const 48
                        local.get 9
                        memory.fill
                      end
                      local.get 1
                      local.get 5
                      local.get 6
                      i32.add
                      local.tee 5
                      i32.add
                      i32.const 12334
                      i32.store16 align=1
                      local.get 5
                      i32.const 2
                      i32.add
                      local.set 7
                      br 8 (;@1;)
                    end
                    local.get 3
                    local.get 1
                    local.get 6
                    local.get 7
                    i32.add
                    i32.const 1
                    i32.add
                    local.tee 7
                    i32.add
                    call $_ZN3ryu6pretty8mantissa19write_mantissa_long17h12d0ca81b2e76182E
                    local.get 1
                    local.get 6
                    i32.add
                    local.set 9
                    block ;; label = @9
                      local.get 5
                      i32.eqz
                      br_if 0 (;@9;)
                      local.get 9
                      local.get 9
                      i32.const 1
                      i32.add
                      local.get 5
                      memory.copy
                    end
                    local.get 9
                    local.get 5
                    i32.add
                    i32.const 46
                    i32.store8
                    br 7 (;@1;)
                  end
                  local.get 1
                  local.get 6
                  i32.add
                  local.tee 12
                  i32.const 11824
                  i32.store16 align=1
                  i32.const 2
                  local.get 5
                  i32.sub
                  local.set 9
                  block ;; label = @8
                    local.get 5
                    i32.const -1
                    i32.gt_s
                    br_if 0 (;@8;)
                    local.get 9
                    i32.const 3
                    local.get 9
                    i32.const 3
                    i32.gt_u
                    select
                    i32.const -2
                    i32.add
                    local.tee 5
                    i32.eqz
                    br_if 0 (;@8;)
                    local.get 12
                    i32.const 2
                    i32.add
                    i32.const 48
                    local.get 5
                    memory.fill
                  end
                  local.get 3
                  local.get 1
                  local.get 7
                  local.get 6
                  i32.add
                  local.get 9
                  i32.add
                  local.tee 7
                  i32.add
                  call $_ZN3ryu6pretty8mantissa19write_mantissa_long17h12d0ca81b2e76182E
                  br 6 (;@1;)
                end
                local.get 7
                i32.const 45
                i32.store8
                i32.const 1
                local.get 5
                i32.sub
                local.set 5
                local.get 7
                i32.const 1
                i32.add
                local.set 7
              end
              local.get 5
              i32.const 99
              i32.gt_s
              br_if 1 (;@4;)
              block ;; label = @6
                local.get 5
                i32.const 9
                i32.gt_s
                br_if 0 (;@6;)
                local.get 7
                local.get 5
                i32.const 48
                i32.add
                i32.store8
                local.get 9
                i32.const 31
                i32.shr_u
                i32.const 1
                i32.add
                local.get 12
                i32.add
                local.set 7
                br 5 (;@1;)
              end
              local.get 7
              local.get 5
              i32.const 1
              i32.shl
              i32.load16_u offset=1048586 align=1
              i32.store16 align=1
              local.get 9
              i32.const 31
              i32.shr_u
              i32.const 2
              i32.or
              local.get 12
              i32.add
              local.set 7
              br 4 (;@1;)
            end
            local.get 3
            local.get 1
            local.get 7
            local.get 6
            i32.add
            local.tee 12
            i32.add
            local.tee 11
            i32.const 1
            i32.add
            call $_ZN3ryu6pretty8mantissa19write_mantissa_long17h12d0ca81b2e76182E
            local.get 1
            local.get 6
            i32.add
            local.tee 7
            local.get 7
            i32.load8_u offset=1
            i32.store8
            local.get 7
            i32.const 46
            i32.store8 offset=1
            local.get 11
            i32.const 101
            i32.store8 offset=1
            local.get 1
            local.get 12
            i32.const 2
            i32.add
            local.tee 12
            i32.add
            local.set 7
            local.get 9
            i32.const 0
            i32.lt_s
            br_if 1 (;@3;)
            local.get 9
            local.set 5
            br 2 (;@2;)
          end
          local.get 7
          local.get 5
          i32.const 100
          i32.div_u
          local.tee 11
          i32.const 48
          i32.add
          i32.store8
          local.get 7
          local.get 5
          local.get 11
          i32.const 100
          i32.mul
          i32.sub
          i32.const 1
          i32.shl
          i32.load16_u offset=1048586 align=1
          i32.store16 offset=1 align=1
          local.get 9
          i32.const 31
          i32.shr_u
          i32.const 3
          i32.add
          local.get 12
          i32.add
          local.set 7
          br 2 (;@1;)
        end
        local.get 7
        i32.const 45
        i32.store8
        i32.const 1
        local.get 5
        i32.sub
        local.set 5
        local.get 7
        i32.const 1
        i32.add
        local.set 7
      end
      block ;; label = @2
        local.get 5
        i32.const 99
        i32.gt_s
        br_if 0 (;@2;)
        block ;; label = @3
          local.get 5
          i32.const 9
          i32.gt_s
          br_if 0 (;@3;)
          local.get 7
          local.get 5
          i32.const 48
          i32.add
          i32.store8
          local.get 9
          i32.const 31
          i32.shr_u
          i32.const 1
          i32.add
          local.get 12
          i32.add
          local.set 7
          br 2 (;@1;)
        end
        local.get 7
        local.get 5
        i32.const 1
        i32.shl
        i32.load16_u offset=1048586 align=1
        i32.store16 align=1
        local.get 9
        i32.const 31
        i32.shr_u
        i32.const 2
        i32.or
        local.get 12
        i32.add
        local.set 7
        br 1 (;@1;)
      end
      local.get 7
      local.get 5
      i32.const 100
      i32.div_u
      local.tee 11
      i32.const 48
      i32.add
      i32.store8
      local.get 7
      local.get 5
      local.get 11
      i32.const 100
      i32.mul
      i32.sub
      i32.const 1
      i32.shl
      i32.load16_u offset=1048586 align=1
      i32.store16 offset=1 align=1
      local.get 9
      i32.const 31
      i32.shr_u
      i32.const 3
      i32.add
      local.get 12
      i32.add
      local.set 7
    end
    local.get 2
    i32.const 416
    i32.add
    global.set $__stack_pointer
    local.get 7
  )
  (func $_ZN3ryu6pretty8mantissa19write_mantissa_long17h12d0ca81b2e76182E (;4;) (type 2) (param i64 i32)
    (local i32 i64 i32 i32 i32 i32)
    block ;; label = @1
      block ;; label = @2
        local.get 0
        i64.const 4294967296
        i64.ge_u
        br_if 0 (;@2;)
        local.get 1
        local.set 2
        local.get 0
        local.set 3
        br 1 (;@1;)
      end
      local.get 1
      i32.const -8
      i32.add
      local.tee 2
      local.get 0
      i64.const 100000000
      i64.div_u
      local.tee 3
      i64.const 4194967296
      i64.mul
      local.get 0
      i64.add
      i32.wrap_i64
      local.tee 4
      i32.const 10000
      i32.div_u
      local.tee 5
      i32.const 10000
      i32.rem_u
      local.tee 6
      i32.const 65535
      i32.and
      i32.const 100
      i32.div_u
      local.tee 7
      i32.const 1
      i32.shl
      i32.load16_u offset=1048586 align=1
      i32.store16 align=1
      local.get 1
      i32.const -4
      i32.add
      local.get 4
      local.get 5
      i32.const 10000
      i32.mul
      i32.sub
      local.tee 4
      i32.const 65535
      i32.and
      i32.const 100
      i32.div_u
      local.tee 5
      i32.const 1
      i32.shl
      i32.load16_u offset=1048586 align=1
      i32.store16 align=1
      local.get 1
      i32.const -6
      i32.add
      local.get 6
      local.get 7
      i32.const 100
      i32.mul
      i32.sub
      i32.const 65535
      i32.and
      i32.const 1
      i32.shl
      i32.load16_u offset=1048586 align=1
      i32.store16 align=1
      local.get 1
      i32.const -2
      i32.add
      local.get 4
      local.get 5
      i32.const 100
      i32.mul
      i32.sub
      i32.const 65535
      i32.and
      i32.const 1
      i32.shl
      i32.load16_u offset=1048586 align=1
      i32.store16 align=1
    end
    block ;; label = @1
      block ;; label = @2
        local.get 3
        i32.wrap_i64
        local.tee 1
        i32.const 10000
        i32.ge_u
        br_if 0 (;@2;)
        local.get 1
        local.set 4
        br 1 (;@1;)
      end
      local.get 2
      i32.const -4
      i32.add
      local.set 2
      loop ;; label = @2
        local.get 2
        local.get 1
        i32.const 10000
        i32.div_u
        local.tee 4
        i32.const -10000
        i32.mul
        local.get 1
        i32.add
        local.tee 5
        i32.const 100
        i32.div_u
        local.tee 6
        i32.const 1
        i32.shl
        i32.load16_u offset=1048586 align=1
        i32.store16 align=1
        local.get 2
        i32.const 2
        i32.add
        local.get 5
        local.get 6
        i32.const 100
        i32.mul
        i32.sub
        i32.const 1
        i32.shl
        i32.load16_u offset=1048586 align=1
        i32.store16 align=1
        local.get 2
        i32.const -4
        i32.add
        local.set 2
        local.get 1
        i32.const 99999999
        i32.gt_u
        local.set 5
        local.get 4
        local.set 1
        local.get 5
        br_if 0 (;@2;)
      end
      local.get 2
      i32.const 4
      i32.add
      local.set 2
    end
    block ;; label = @1
      block ;; label = @2
        local.get 4
        i32.const 99
        i32.gt_u
        br_if 0 (;@2;)
        local.get 4
        local.set 1
        br 1 (;@1;)
      end
      local.get 2
      i32.const -2
      i32.add
      local.tee 2
      local.get 4
      local.get 4
      i32.const 65535
      i32.and
      i32.const 100
      i32.div_u
      local.tee 1
      i32.const 100
      i32.mul
      i32.sub
      i32.const 65535
      i32.and
      i32.const 1
      i32.shl
      i32.load16_u offset=1048586 align=1
      i32.store16 align=1
    end
    block ;; label = @1
      local.get 1
      i32.const 9
      i32.gt_u
      br_if 0 (;@1;)
      local.get 2
      i32.const -1
      i32.add
      local.get 1
      i32.const 48
      i32.or
      i32.store8
      return
    end
    local.get 2
    i32.const -2
    i32.add
    local.get 1
    i32.const 1
    i32.shl
    i32.load16_u offset=1048586 align=1
    i32.store16 align=1
  )
  (func $__lshrti3 (;5;) (type 3) (param i32 i64 i64 i32)
    (local i64)
    block ;; label = @1
      block ;; label = @2
        local.get 3
        i32.const 64
        i32.and
        br_if 0 (;@2;)
        local.get 3
        i32.eqz
        br_if 1 (;@1;)
        local.get 2
        i32.const 0
        local.get 3
        i32.sub
        i32.const 63
        i32.and
        i64.extend_i32_u
        i64.shl
        local.get 1
        local.get 3
        i32.const 63
        i32.and
        i64.extend_i32_u
        local.tee 4
        i64.shr_u
        i64.or
        local.set 1
        local.get 2
        local.get 4
        i64.shr_u
        local.set 2
        br 1 (;@1;)
      end
      local.get 2
      local.get 3
      i32.const 63
      i32.and
      i64.extend_i32_u
      i64.shr_u
      local.set 1
      i64.const 0
      local.set 2
    end
    local.get 0
    local.get 1
    i64.store
    local.get 0
    local.get 2
    i64.store offset=8
  )
  (func $__multi3 (;6;) (type 4) (param i32 i64 i64 i64 i64)
    (local i64 i64 i64 i64 i64 i64)
    local.get 0
    local.get 3
    i64.const 4294967295
    i64.and
    local.tee 5
    local.get 1
    i64.const 4294967295
    i64.and
    local.tee 6
    i64.mul
    local.tee 7
    local.get 3
    i64.const 32
    i64.shr_u
    local.tee 8
    local.get 6
    i64.mul
    local.tee 6
    local.get 5
    local.get 1
    i64.const 32
    i64.shr_u
    local.tee 9
    i64.mul
    i64.add
    local.tee 5
    i64.const 32
    i64.shl
    i64.add
    local.tee 10
    i64.store
    local.get 0
    local.get 8
    local.get 9
    i64.mul
    local.get 5
    local.get 6
    i64.lt_u
    i64.extend_i32_u
    i64.const 32
    i64.shl
    local.get 5
    i64.const 32
    i64.shr_u
    i64.or
    i64.add
    local.get 10
    local.get 7
    i64.lt_u
    i64.extend_i32_u
    i64.add
    local.get 4
    local.get 1
    i64.mul
    local.get 3
    local.get 2
    i64.mul
    i64.add
    i64.add
    i64.store offset=8
  )
  (func $__ashlti3 (;7;) (type 3) (param i32 i64 i64 i32)
    (local i64)
    block ;; label = @1
      block ;; label = @2
        local.get 3
        i32.const 64
        i32.and
        br_if 0 (;@2;)
        local.get 3
        i32.eqz
        br_if 1 (;@1;)
        local.get 2
        local.get 3
        i32.const 63
        i32.and
        i64.extend_i32_u
        local.tee 4
        i64.shl
        local.get 1
        i32.const 0
        local.get 3
        i32.sub
        i32.const 63
        i32.and
        i64.extend_i32_u
        i64.shr_u
        i64.or
        local.set 2
        local.get 1
        local.get 4
        i64.shl
        local.set 1
        br 1 (;@1;)
      end
      local.get 1
      local.get 3
      i32.const 63
      i32.and
      i64.extend_i32_u
      i64.shl
      local.set 2
      i64.const 0
      local.set 1
    end
    local.get 0
    local.get 1
    i64.store
    local.get 0
    local.get 2
    i64.store offset=8
  )
  (data $.rodata (;0;) (i32.const 1048576) "inf-infNaN00010203040506070809101112131415161718192021222324252627282930313233343536373839404142434445464748495051525354555657585960616263646566676869707172737475767778798081828384858687888990919293949596979899\00\00\00\00\00\00\00\00\00\00\00\00\00\00\00\00\00\00\00\00\00@\95YiYUUTU\15UUV\04\05\15A\10TU@EQUD@EPDPUUE\00@\00@@\04D\96eUVUE@ETQA\15@U\91UUUU@Q\05\01\00\00TETTEU\05\04\00\10\04\10\14\04@\00\00\00\01@UU\15AT\04\00\00D\00\01\00\00\00\00@A\00\00DPDEPT\00UUTUeQ\00@\00@\01\00\00\01\00\05\01\00\11TQQTUU\05\00\00\00\00\00\00\00\00\01\00\00\00\00\00\00\00\05\00\00\00\00\00\00\00\19\00\00\00\00\00\00\00}\00\00\00\00\00\00\00q\02\00\00\00\00\00\005\0c\00\00\00\00\00\00\09=\00\00\00\00\00\00-1\01\00\00\00\00\00\e1\f5\05\00\00\00\00\00e\cd\1d\00\00\00\00\00\f9\02\95\00\00\00\00\00\dd\0e\e9\02\00\00\00\00QJ\8d\0e\00\00\00\00\95s\c2H\00\00\00\00\e9A\cck\01\00\00\00\8dI\fd\1a\07\00\00\00\c1o\f2\86#\00\00\00\c5.\bc\a2\b1\00\00\00\d9\e9\ac-x\03\00\00=\91`\e4X\11\00\001\d6\e2u\bcV\00\00\f5.nM\ae\b1\01\00\c9\ea&\83gx\08\00\ed\95\c2\8f\05Z*\00\a1\ed\cc\ce\1b\c2\d3\00%\a4\00\0a\8b\ca\22\04\00\00\00\00\00\00\00\00\00\00\00\00\00\00\00\10\00\00\00\00\00\00\00\00\b94\032\b7\f4\ad\14\10\db\1a\b3\08\92T\0e\0d0}\95\14G\ba\1af\08\8fM&\ad\c6m\f5\98\bf\85\e2\b7E\11\ca\96\85=\92\bd\1d\eb\fc\a1\18`\dc\efR\16<\92\ae\22\0b\b8\c1\b4\83\9d-[\05b\da\1c0L~\8fN\8b\b2[\16\f4R\9f\8bV\a5\12\fb\d4\82vC\ed\8a\f0\8f\e7\f91\15e\19\18P\f1\9b\d9J\13\ee\b4(L\f0\a6\86\c1%\1f\03_\c2p\cb\9eI\16\e6B\88\9cD\eb \14\b0e\086\adn\a5\85\85\f0\ca\14\e2\fd\03\1a\0b\89\99y\d5\b1=\09\d8\da\97:5\eb\cf\10\ac6?^s\bb8\cf>gR\faD\af\ba\15\01\00\00\00\00\00\00\00\00\00\00\00\00\00\00 4Pe\c0_\c9\a6R\bb\13\cb\ae\c4@\c2\18\06\c8\dfq\00\d5\a8|\f5o\0f\daX\fc'\13nGV5}$ e\02\c7\e7h\e4\8c\a4\1d\e9\e6\02h\d7\cd9ayw\fc\c2@[\ef\16y\8c\deC\ff\a7Q\f9\91\f3\b2x\f5\bd\be\11\e8W\e9\d6\e8\be\e8{\b0T\ac\8f\84\8du\1b\ea#\a4\99\e9\f9\d3\8b\b7\a3q@a\da>\15\ce\e3>\cbs\f9H\08\8c\97\b4'\d5\1bp\10\a2\bf\ef\b9\eb\852\15M\b4M\b4\9b\bbo\19\96\b6\07l\f8\e7\ee\ad6\d9\b4\f5\915\ae\13\22\22\18\afNjhM\91\da\aa=O@t\1e\9f\bd\9e\e0\06\a1\c0\98W\c2\a7\fd\a4\0e\90\17\0e}Iqs\e3 \8f\b2 \d8v\05\14;\12\85=t4\81\13C\b0\ad)z_'\f45\1c0.0")
  (@producers
    (language "Rust" "")
    (processed-by "rustc" "1.92.0 (ded5c06cf 2025-12-08)")
  )
  (@custom "target_features" (after data) "\08+\0bbulk-memory+\0fbulk-memory-opt+\16call-indirect-overlong+\0amultivalue+\0fmutable-globals+\13nontrapping-fptoint+\0freference-types+\08sign-ext")
)
