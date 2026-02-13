(module $wado_bundled_fts.wasm
  (type (;0;) (func (param f64 i32 i32 i32) (result i32)))
  (type (;1;) (func (param i32 i32)))
  (type (;2;) (func (param i32 f64 i32)))
  (type (;3;) (func (param i32 f64)))
  (type (;4;) (func (param i32 i32 i64 i32 i32 i32) (result i32)))
  (type (;5;) (func (param i32 i32 i64 i32 i32) (result i32)))
  (type (;6;) (func (param i32 i32 i64 i32)))
  (type (;7;) (func (param i32 i32 i32)))
  (type (;8;) (func (param f64 i32 i32) (result i32)))
  (type (;9;) (func (param f32 i32) (result i32)))
  (type (;10;) (func (param i64 i32) (result i64)))
  (type (;11;) (func (param f32 i32 i32 i32) (result i32)))
  (type (;12;) (func (param f32 i32 i32) (result i32)))
  (type (;13;) (func (param f64 i32) (result i32)))
  (type (;14;) (func))
  (type (;15;) (func (param i32 i64 i64 i64 i64)))
  (type (;16;) (func (param i32 i64 i64 i32)))
  (memory (;0;) 17)
  (global $__stack_pointer (;0;) (mut i32) i32.const 1048576)
  (global (;1;) i32 i32.const 1049384)
  (global (;2;) i32 i32.const 1049392)
  (export "memory" (memory 0))
  (export "f32_to_buffer" (func $f32_to_buffer))
  (export "f32_to_buffer_exp" (func $f32_to_buffer_exp))
  (export "f32_to_buffer_fixed" (func $f32_to_buffer_fixed))
  (export "f64_to_buffer" (func $f64_to_buffer))
  (export "f64_to_buffer_exp" (func $f64_to_buffer_exp))
  (export "f64_to_buffer_fixed" (func $f64_to_buffer_fixed))
  (export "__data_end" (global 1))
  (export "__heap_base" (global 2))
  (func $_ZN16wado_bundled_fts11f64_fmt_exp17h24a28c59be25ab6aE (;0;) (type 0) (param f64 i32 i32 i32) (result i32)
    (local i32 i32 i32 i32 i32 i64)
    global.get $__stack_pointer
    i32.const 16
    i32.sub
    local.tee 4
    global.set $__stack_pointer
    block ;; label = @1
      block ;; label = @2
        block ;; label = @3
          block ;; label = @4
            block ;; label = @5
              block ;; label = @6
                block ;; label = @7
                  block ;; label = @8
                    local.get 0
                    local.get 0
                    f64.ne
                    br_if 0 (;@8;)
                    local.get 0
                    f64.const inf (;=inf;)
                    f64.eq
                    br_if 1 (;@7;)
                    local.get 0
                    f64.const -inf (;=-inf;)
                    f64.eq
                    br_if 2 (;@6;)
                    i32.const 69
                    i32.const 101
                    local.get 2
                    select
                    local.set 5
                    block ;; label = @9
                      local.get 0
                      f64.const 0x0p+0 (;=0;)
                      f64.eq
                      br_if 0 (;@9;)
                      local.get 0
                      f64.const 0x0p+0 (;=0;)
                      f64.lt
                      br_if 4 (;@5;)
                      i32.const 0
                      local.set 2
                      br 7 (;@2;)
                    end
                    i32.const 0
                    local.set 6
                    local.get 0
                    i64.reinterpret_f64
                    i64.const -1
                    i64.le_s
                    br_if 4 (;@4;)
                    br 5 (;@3;)
                  end
                  local.get 3
                  i32.const 2
                  i32.add
                  i32.const 0
                  i32.load8_u offset=1048581
                  i32.store8
                  local.get 3
                  i32.const 0
                  i32.load16_u offset=1048579 align=1
                  i32.store16 align=1
                  i32.const 3
                  local.set 2
                  br 6 (;@1;)
                end
                local.get 3
                i32.const 2
                i32.add
                i32.const 0
                i32.load8_u offset=1048578
                i32.store8
                local.get 3
                i32.const 0
                i32.load16_u offset=1048576 align=1
                i32.store16 align=1
                i32.const 3
                local.set 2
                br 5 (;@1;)
              end
              local.get 3
              i32.const 1718511917
              i32.store align=1
              i32.const 4
              local.set 2
              br 4 (;@1;)
            end
            local.get 3
            i32.const 45
            i32.store8
            local.get 0
            f64.neg
            local.set 0
            i32.const 1
            local.set 2
            br 2 (;@2;)
          end
          local.get 3
          i32.const 45
          i32.store8
          i32.const 1
          local.set 6
        end
        local.get 3
        local.get 6
        i32.add
        local.tee 2
        i32.const 48
        i32.store8
        block ;; label = @3
          block ;; label = @4
            local.get 1
            i32.const 0
            i32.lt_s
            br_if 0 (;@4;)
            block ;; label = @5
              local.get 1
              br_if 0 (;@5;)
              local.get 2
              i32.const 48
              i32.store8 offset=2
              local.get 2
              local.get 5
              i32.store8 offset=1
              local.get 6
              i32.const 3
              i32.add
              local.set 2
              br 4 (;@1;)
            end
            local.get 2
            i32.const 46
            i32.store8 offset=1
            local.get 6
            i32.const 2
            i32.or
            local.tee 7
            local.set 2
            local.get 1
            local.set 8
            block ;; label = @5
              loop ;; label = @6
                local.get 2
                i32.const 32
                i32.eq
                br_if 1 (;@5;)
                local.get 3
                local.get 2
                i32.add
                i32.const 48
                i32.store8
                local.get 2
                i32.const 1
                i32.add
                local.set 2
                local.get 8
                i32.const -1
                i32.add
                local.tee 8
                br_if 0 (;@6;)
              end
              block ;; label = @6
                local.get 7
                local.get 1
                i32.add
                local.tee 2
                i32.const 32
                i32.lt_u
                br_if 0 (;@6;)
                local.get 2
                i32.const 32
                call $_ZN4core9panicking18panic_bounds_check17h62ab6f5933ba978dE
                unreachable
              end
              local.get 3
              local.get 2
              i32.add
              local.get 5
              i32.store8
              local.get 1
              local.get 6
              i32.add
              local.tee 8
              i32.const 3
              i32.add
              local.tee 2
              i32.const 32
              i32.ge_u
              br_if 2 (;@3;)
              local.get 3
              local.get 2
              i32.add
              i32.const 48
              i32.store8
              local.get 8
              i32.const 4
              i32.add
              local.set 2
              br 4 (;@1;)
            end
            local.get 2
            i32.const 32
            call $_ZN4core9panicking18panic_bounds_check17h62ab6f5933ba978dE
            unreachable
          end
          local.get 2
          i32.const 48
          i32.store8 offset=2
          local.get 2
          local.get 5
          i32.store8 offset=1
          local.get 6
          i32.const 3
          i32.add
          local.set 2
          br 2 (;@1;)
        end
        local.get 2
        i32.const 32
        call $_ZN4core9panicking18panic_bounds_check17h62ab6f5933ba978dE
        unreachable
      end
      block ;; label = @2
        block ;; label = @3
          local.get 1
          i32.const 0
          i32.lt_s
          br_if 0 (;@3;)
          local.get 4
          local.get 0
          local.get 1
          i32.const 1
          i32.add
          local.tee 8
          i32.const 1
          local.get 8
          i32.const 1
          i32.gt_s
          select
          local.tee 8
          i32.const 18
          local.get 8
          i32.const 18
          i32.lt_s
          select
          call $_ZN5fpfmt11fixed_width17hf26acbcd246ad846E
          br 1 (;@2;)
        end
        local.get 4
        local.get 0
        call $_ZN5fpfmt5short17hcb22e12847c1a5c4E
      end
      local.get 3
      local.get 2
      i32.add
      i32.const 32
      local.get 2
      i32.sub
      local.get 4
      i64.load
      local.tee 9
      local.get 4
      i32.load offset=8
      i32.const 64
      local.get 9
      i64.clz
      i32.wrap_i64
      i32.sub
      i32.const 78913
      i32.mul
      i32.const 18
      i32.shr_u
      local.tee 3
      local.get 9
      local.get 3
      i32.const 3
      i32.shl
      i64.load offset=1048592
      i64.ge_u
      i32.add
      local.get 5
      call $_ZN16wado_bundled_fts7fmt_exp17hd40175610150452fE
      local.get 2
      i32.add
      local.set 2
    end
    local.get 4
    i32.const 16
    i32.add
    global.set $__stack_pointer
    local.get 2
  )
  (func $_ZN4core9panicking18panic_bounds_check17h62ab6f5933ba978dE (;1;) (type 1) (param i32 i32)
    call $_ZN4core9panicking9panic_fmt17hcb6b2b4be1f4be38E
    unreachable
  )
  (func $_ZN5fpfmt11fixed_width17hf26acbcd246ad846E (;2;) (type 2) (param i32 f64 i32)
    (local i32 i64 i64 i32 i32 i32 i32 i64 i64)
    global.get $__stack_pointer
    i32.const 80
    i32.sub
    local.tee 3
    global.set $__stack_pointer
    local.get 1
    i64.reinterpret_f64
    local.tee 4
    i64.const 11
    i64.shl
    local.set 5
    block ;; label = @1
      block ;; label = @2
        local.get 4
        i64.const 52
        i64.shr_u
        i32.wrap_i64
        i32.const 2047
        i32.and
        local.tee 6
        br_if 0 (;@2;)
        local.get 5
        i64.const 9223372036854773760
        i64.and
        local.tee 4
        local.get 4
        i64.clz
        local.tee 4
        i64.shl
        local.set 5
        i32.const -1085
        local.get 4
        i32.wrap_i64
        i32.sub
        local.set 7
        br 1 (;@1;)
      end
      local.get 6
      i32.const -1086
      i32.add
      local.set 7
      local.get 5
      i64.const -9223372036854775808
      i64.or
      local.set 5
    end
    block ;; label = @1
      block ;; label = @2
        local.get 7
        i32.const -78913
        i32.mul
        i32.const -4971520
        i32.add
        i32.const 18
        i32.shr_s
        local.get 2
        i32.add
        local.tee 6
        i32.const 27
        i32.div_s
        local.tee 8
        local.get 6
        local.get 8
        i32.const 27
        i32.mul
        i32.sub
        local.tee 8
        i32.const 31
        i32.shr_s
        i32.add
        i32.const 13
        i32.add
        local.tee 9
        i32.const 26
        i32.ge_u
        br_if 0 (;@2;)
        local.get 3
        i32.const 32
        i32.add
        local.get 9
        i32.const 4
        i32.shl
        local.tee 9
        i64.load offset=1048968
        local.get 9
        i64.load offset=1048976
        local.tee 4
        i64.const 0
        i64.ne
        i64.extend_i32_u
        i64.sub
        i64.const 0
        local.get 8
        i32.const 27
        i32.add
        local.get 8
        local.get 8
        i32.const 0
        i32.lt_s
        select
        i32.const 3
        i32.shl
        i64.load offset=1048752
        local.tee 10
        i64.const 0
        call $__multi3
        local.get 3
        i32.const 16
        i32.add
        i64.const 0
        local.get 4
        i64.sub
        i64.const 0
        local.get 10
        i64.const 0
        call $__multi3
        local.get 3
        local.get 3
        i64.load offset=24
        local.tee 10
        local.get 3
        i64.load offset=32
        i64.add
        local.tee 4
        local.get 3
        i64.load offset=16
        i64.const 0
        i64.ne
        i64.extend_i32_u
        i64.add
        local.tee 11
        local.get 3
        i64.load offset=40
        local.get 4
        local.get 10
        i64.lt_u
        i64.extend_i32_u
        i64.add
        local.get 11
        local.get 4
        i64.lt_u
        i64.extend_i32_u
        i64.add
        local.tee 4
        local.get 4
        i64.const 63
        i64.shr_s
        i32.wrap_i64
        i32.const 1
        i32.add
        call $__ashlti3
        local.get 3
        i32.const -3
        local.get 7
        local.get 6
        i32.const 108853
        i32.mul
        i32.const 15
        i32.shr_s
        i32.add
        i32.sub
        i32.store offset=72
        local.get 3
        i64.const 0
        local.get 3
        i64.load
        local.tee 4
        i64.sub
        i64.store offset=64
        local.get 3
        local.get 4
        i64.const 0
        i64.ne
        i64.extend_i32_u
        local.get 3
        i64.load offset=8
        i64.add
        i64.store offset=56
        local.get 5
        local.get 3
        i32.const 56
        i32.add
        call $_ZN5fpfmt6uscale17h620c53c8994a3d2aE
        local.set 4
        local.get 2
        i32.const 19
        i32.gt_u
        br_if 1 (;@1;)
        block ;; label = @3
          local.get 4
          local.get 4
          i64.const 2
          i64.shr_u
          i64.const 1
          i64.and
          i64.add
          i64.const 1
          i64.add
          i64.const 2
          i64.shr_u
          local.tee 5
          local.get 2
          i32.const 3
          i32.shl
          i64.load offset=1048592
          i64.lt_u
          br_if 0 (;@3;)
          local.get 4
          i64.const 10
          i64.div_u
          local.tee 5
          i64.const 2
          i64.shr_u
          i64.const 1
          i64.and
          local.get 4
          i64.const 1
          i64.and
          local.get 4
          local.get 5
          i64.const 10
          i64.mul
          i64.sub
          i64.const 0
          i64.ne
          i64.extend_i32_u
          i64.or
          local.get 5
          i64.or
          i64.add
          i64.const 1
          i64.add
          i64.const 2
          i64.shr_u
          local.set 5
          local.get 6
          i32.const -1
          i32.add
          local.set 6
        end
        local.get 0
        local.get 5
        i64.store
        local.get 0
        i32.const 0
        local.get 6
        i32.sub
        i32.store offset=8
        local.get 3
        i32.const 80
        i32.add
        global.set $__stack_pointer
        return
      end
      local.get 9
      i32.const 26
      call $_ZN4core9panicking18panic_bounds_check17h62ab6f5933ba978dE
      unreachable
    end
    local.get 2
    i32.const 20
    call $_ZN4core9panicking18panic_bounds_check17h62ab6f5933ba978dE
    unreachable
  )
  (func $_ZN5fpfmt5short17hcb22e12847c1a5c4E (;3;) (type 3) (param i32 f64)
    (local i32 i64 i64 i32 i32 i32 i64 i32 i32 i32 i64 i64)
    global.get $__stack_pointer
    i32.const 80
    i32.sub
    local.tee 2
    global.set $__stack_pointer
    local.get 1
    i64.reinterpret_f64
    local.tee 3
    i64.const 11
    i64.shl
    local.set 4
    block ;; label = @1
      block ;; label = @2
        block ;; label = @3
          block ;; label = @4
            local.get 3
            i64.const 52
            i64.shr_u
            i32.wrap_i64
            i32.const 2047
            i32.and
            local.tee 5
            br_if 0 (;@4;)
            local.get 4
            i64.const 9223372036854773760
            i64.and
            local.tee 3
            local.get 3
            i64.clz
            local.tee 4
            i64.shl
            local.set 3
            i32.const -1085
            local.get 4
            i32.wrap_i64
            i32.sub
            local.set 6
            br 1 (;@3;)
          end
          local.get 5
          i32.const -1086
          i32.add
          local.set 6
          local.get 4
          i64.const -9223372036854775808
          i64.or
          local.tee 3
          i64.const -9223372036854775808
          i64.ne
          br_if 0 (;@3;)
          local.get 5
          i32.const 1
          i32.ne
          br_if 1 (;@2;)
        end
        i32.const -1074
        local.get 6
        i32.sub
        i32.const 11
        local.get 6
        i32.const -1085
        i32.lt_s
        select
        local.tee 7
        local.get 6
        i32.add
        i32.const 78913
        i32.mul
        i32.const 18
        i32.shr_s
        local.set 5
        i64.const -1
        local.get 7
        i32.const -1
        i32.add
        i32.const 63
        i32.and
        i64.extend_i32_u
        i64.shl
        local.get 3
        i64.add
        local.set 8
        br 1 (;@1;)
      end
      local.get 5
      i32.const 631305
      i32.mul
      i32.const -678914538
      i32.add
      i32.const 21
      i32.shr_s
      local.set 5
      i64.const 9223372036854775296
      local.set 8
      i32.const 11
      local.set 7
      i64.const -9223372036854775808
      local.set 3
    end
    block ;; label = @1
      i32.const 0
      local.get 5
      i32.const 27
      i32.rem_s
      local.tee 9
      i32.sub
      local.tee 10
      i32.const 31
      i32.shr_s
      local.get 5
      i32.const -27
      i32.div_s
      i32.const 13
      i32.add
      i32.const 65535
      i32.and
      i32.add
      local.tee 11
      i32.const 26
      i32.ge_u
      br_if 0 (;@1;)
      local.get 2
      i32.const 32
      i32.add
      local.get 11
      i32.const 4
      i32.shl
      local.tee 11
      i64.load offset=1048968
      local.get 11
      i64.load offset=1048976
      local.tee 4
      i64.const 0
      i64.ne
      i64.extend_i32_u
      i64.sub
      i64.const 0
      local.get 10
      i32.const 27
      i32.add
      local.get 10
      local.get 9
      i32.const 0
      i32.gt_s
      select
      i32.const 3
      i32.shl
      i64.load offset=1048752
      local.tee 12
      i64.const 0
      call $__multi3
      local.get 2
      i32.const 16
      i32.add
      i64.const 0
      local.get 4
      i64.sub
      i64.const 0
      local.get 12
      i64.const 0
      call $__multi3
      local.get 2
      local.get 2
      i64.load offset=24
      local.tee 12
      local.get 2
      i64.load offset=32
      i64.add
      local.tee 4
      local.get 2
      i64.load offset=16
      i64.const 0
      i64.ne
      i64.extend_i32_u
      i64.add
      local.tee 13
      local.get 2
      i64.load offset=40
      local.get 4
      local.get 12
      i64.lt_u
      i64.extend_i32_u
      i64.add
      local.get 13
      local.get 4
      i64.lt_u
      i64.extend_i32_u
      i64.add
      local.tee 4
      local.get 4
      i64.const 63
      i64.shr_s
      i32.wrap_i64
      i32.const 1
      i32.add
      call $__ashlti3
      local.get 2
      i32.const -3
      local.get 6
      local.get 5
      i32.const -108853
      i32.mul
      i32.const 15
      i32.shr_s
      i32.add
      i32.sub
      i32.store offset=72
      local.get 2
      i64.const 0
      local.get 2
      i64.load
      local.tee 4
      i64.sub
      i64.store offset=64
      local.get 2
      local.get 4
      i64.const 0
      i64.ne
      i64.extend_i32_u
      local.get 2
      i64.load offset=8
      i64.add
      i64.store offset=56
      local.get 8
      local.get 2
      i32.const 56
      i32.add
      call $_ZN5fpfmt6uscale17h620c53c8994a3d2aE
      local.set 8
      block ;; label = @2
        block ;; label = @3
          block ;; label = @4
            i64.const 1
            local.get 7
            i32.const -1
            i32.add
            i32.const 63
            i32.and
            i64.extend_i32_u
            i64.shl
            local.get 3
            i64.add
            local.get 2
            i32.const 56
            i32.add
            call $_ZN5fpfmt6uscale17h620c53c8994a3d2aE
            local.get 3
            local.get 7
            i32.const 63
            i32.and
            i64.extend_i32_u
            i64.shr_u
            i64.const 1
            i64.and
            local.tee 12
            i64.sub
            local.tee 13
            i64.const 40
            i64.div_u
            local.tee 4
            i64.const 10
            i64.mul
            local.get 12
            local.get 8
            i64.add
            i64.const 3
            i64.add
            i64.const 2
            i64.shr_u
            local.tee 8
            i64.ge_u
            br_if 0 (;@4;)
            local.get 8
            local.get 13
            i64.const 2
            i64.shr_u
            i64.lt_u
            br_if 1 (;@3;)
            local.get 8
            local.set 4
            br 2 (;@2;)
          end
          local.get 5
          i32.const 1
          i32.add
          local.set 5
          local.get 4
          i64.const -3689348814741910323
          i64.mul
          i64.const 63
          i64.rotl
          local.tee 3
          i64.const 1844674407370955161
          i64.gt_u
          br_if 1 (;@2;)
          local.get 3
          i64.const -4078282918271054303
          i64.mul
          i64.const 56
          i64.rotl
          local.tee 4
          local.get 3
          local.get 4
          i64.const 184467440738
          i64.lt_u
          local.tee 6
          select
          local.tee 3
          i64.const -3276141747490816367
          i64.mul
          i64.const 60
          i64.rotl
          local.tee 4
          local.get 3
          local.get 4
          i64.const 1844674407370956
          i64.lt_u
          local.tee 7
          select
          local.tee 3
          i64.const -8116567392432202711
          i64.mul
          i64.const 62
          i64.rotl
          local.tee 4
          local.get 3
          local.get 4
          i64.const 184467440737095517
          i64.lt_u
          local.tee 10
          select
          local.tee 3
          i64.const -3689348814741910323
          i64.mul
          i64.const 63
          i64.rotl
          local.tee 4
          local.get 3
          local.get 4
          i64.const 1844674407370955162
          i64.lt_u
          local.tee 11
          select
          local.set 4
          i32.const 9
          i32.const 1
          local.get 6
          select
          local.get 5
          i32.add
          local.tee 5
          i32.const 4
          i32.add
          local.get 5
          local.get 7
          select
          local.tee 5
          i32.const 2
          i32.add
          local.get 5
          local.get 10
          select
          local.get 11
          i32.add
          local.set 5
          br 1 (;@2;)
        end
        local.get 3
        local.get 2
        i32.const 56
        i32.add
        call $_ZN5fpfmt6uscale17h620c53c8994a3d2aE
        local.tee 3
        local.get 3
        i64.const 2
        i64.shr_u
        i64.const 1
        i64.and
        i64.add
        i64.const 1
        i64.add
        i64.const 2
        i64.shr_u
        local.set 4
      end
      local.get 0
      local.get 5
      i32.store offset=8
      local.get 0
      local.get 4
      i64.store
      local.get 2
      i32.const 80
      i32.add
      global.set $__stack_pointer
      return
    end
    local.get 11
    i32.const 26
    call $_ZN4core9panicking18panic_bounds_check17h62ab6f5933ba978dE
    unreachable
  )
  (func $_ZN16wado_bundled_fts7fmt_exp17hd40175610150452fE (;4;) (type 4) (param i32 i32 i64 i32 i32 i32) (result i32)
    (local i32 i32 i32)
    local.get 0
    local.get 1
    local.get 2
    local.get 4
    call $_ZN16wado_bundled_fts12write_digits17h1647d1324138129fE
    local.get 4
    local.get 3
    i32.add
    local.tee 6
    i32.const -1
    i32.add
    local.set 7
    block ;; label = @1
      local.get 4
      i32.const 2
      i32.lt_u
      br_if 0 (;@1;)
      local.get 4
      local.set 3
      loop ;; label = @2
        local.get 0
        local.get 3
        i32.add
        local.tee 8
        local.get 8
        i32.const -1
        i32.add
        i32.load8_u
        i32.store8
        local.get 3
        i32.const -1
        i32.add
        local.tee 3
        i32.const 1
        i32.gt_u
        br_if 0 (;@2;)
      end
      local.get 0
      i32.const 46
      i32.store8 offset=1
      local.get 4
      i32.const 1
      i32.add
      local.set 4
    end
    local.get 0
    local.get 4
    i32.add
    local.get 5
    i32.store8
    local.get 4
    i32.const 1
    i32.add
    local.set 3
    block ;; label = @1
      local.get 7
      i32.const 0
      i32.ge_s
      br_if 0 (;@1;)
      local.get 0
      local.get 3
      i32.add
      i32.const 45
      i32.store8
      i32.const 1
      local.get 6
      i32.sub
      local.set 7
      local.get 4
      i32.const 2
      i32.add
      local.set 3
    end
    block ;; label = @1
      block ;; label = @2
        block ;; label = @3
          block ;; label = @4
            block ;; label = @5
              block ;; label = @6
                local.get 7
                i32.const 99
                i32.gt_u
                br_if 0 (;@6;)
                block ;; label = @7
                  local.get 7
                  i32.const 9
                  i32.gt_u
                  br_if 0 (;@7;)
                  block ;; label = @8
                    local.get 3
                    local.get 1
                    i32.ge_u
                    br_if 0 (;@8;)
                    i32.const 1
                    local.set 4
                    local.get 3
                    local.set 8
                    br 7 (;@1;)
                  end
                  i32.const 23
                  i32.const 23
                  call $_ZN4core9panicking18panic_bounds_check17h62ab6f5933ba978dE
                  unreachable
                end
                local.get 3
                local.get 1
                i32.ge_u
                br_if 2 (;@4;)
                local.get 0
                local.get 3
                i32.add
                local.get 7
                i32.const 255
                i32.and
                i32.const 10
                i32.div_u
                local.tee 4
                i32.const 48
                i32.or
                i32.store8
                local.get 3
                i32.const 1
                i32.add
                local.tee 8
                local.get 1
                i32.ge_u
                br_if 1 (;@5;)
                local.get 4
                i32.const -10
                i32.mul
                local.get 7
                i32.add
                local.set 7
                i32.const 2
                local.set 4
                br 5 (;@1;)
              end
              local.get 3
              local.get 1
              i32.ge_u
              br_if 2 (;@3;)
              local.get 0
              local.get 3
              i32.add
              local.get 7
              i32.const 100
              i32.div_u
              i32.const 48
              i32.add
              i32.store8
              block ;; label = @6
                local.get 3
                i32.const 1
                i32.add
                local.tee 8
                local.get 1
                i32.lt_u
                br_if 0 (;@6;)
                local.get 8
                local.get 1
                call $_ZN4core9panicking18panic_bounds_check17h62ab6f5933ba978dE
                unreachable
              end
              local.get 0
              local.get 8
              i32.add
              local.get 7
              i32.const 10
              i32.div_u
              local.tee 4
              i32.const 10
              i32.rem_u
              i32.const 48
              i32.or
              i32.store8
              local.get 3
              i32.const 2
              i32.add
              local.tee 8
              local.get 1
              i32.lt_u
              br_if 3 (;@2;)
            end
            local.get 8
            local.get 1
            call $_ZN4core9panicking18panic_bounds_check17h62ab6f5933ba978dE
            unreachable
          end
          i32.const 23
          i32.const 23
          call $_ZN4core9panicking18panic_bounds_check17h62ab6f5933ba978dE
          unreachable
        end
        i32.const 23
        i32.const 23
        call $_ZN4core9panicking18panic_bounds_check17h62ab6f5933ba978dE
        unreachable
      end
      local.get 4
      i32.const 246
      i32.mul
      local.get 7
      i32.add
      local.set 7
      i32.const 3
      local.set 4
    end
    local.get 0
    local.get 8
    i32.add
    local.get 7
    i32.const 48
    i32.or
    i32.store8
    local.get 3
    local.get 4
    i32.add
  )
  (func $_ZN16wado_bundled_fts12fmt_shortest17h46d411f7a4784c84E (;5;) (type 5) (param i32 i32 i64 i32 i32) (result i32)
    (local i32 i32 i32 i32 i32)
    block ;; label = @1
      local.get 4
      local.get 3
      i32.add
      local.tee 5
      i32.const 3
      i32.add
      i32.const 20
      i32.lt_u
      br_if 0 (;@1;)
      local.get 0
      local.get 1
      local.get 2
      local.get 3
      local.get 4
      i32.const 101
      call $_ZN16wado_bundled_fts7fmt_exp17hd40175610150452fE
      return
    end
    block ;; label = @1
      block ;; label = @2
        block ;; label = @3
          block ;; label = @4
            local.get 3
            i32.const -1
            i32.gt_s
            br_if 0 (;@4;)
            block ;; label = @5
              local.get 5
              i32.const -1
              i32.add
              i32.const -1
              i32.le_s
              br_if 0 (;@5;)
              local.get 0
              local.get 1
              local.get 2
              local.get 4
              call $_ZN16wado_bundled_fts12write_digits17h1647d1324138129fE
              block ;; label = @6
                local.get 4
                local.get 5
                i32.le_u
                br_if 0 (;@6;)
                local.get 4
                local.set 3
                loop ;; label = @7
                  block ;; label = @8
                    local.get 3
                    i32.const -1
                    i32.add
                    local.tee 6
                    local.get 1
                    i32.lt_u
                    br_if 0 (;@8;)
                    i32.const -1
                    local.get 1
                    call $_ZN4core9panicking18panic_bounds_check17h62ab6f5933ba978dE
                    unreachable
                  end
                  local.get 0
                  local.get 3
                  i32.add
                  local.tee 3
                  local.get 3
                  i32.const -1
                  i32.add
                  i32.load8_u
                  i32.store8
                  local.get 6
                  local.set 3
                  local.get 6
                  local.get 5
                  i32.gt_u
                  br_if 0 (;@7;)
                end
              end
              local.get 5
              local.get 1
              i32.ge_u
              br_if 2 (;@3;)
              local.get 0
              local.get 5
              i32.add
              i32.const 46
              i32.store8
              local.get 4
              i32.const 1
              i32.add
              return
            end
            local.get 0
            i32.const 11824
            i32.store16 align=1
            block ;; label = @5
              block ;; label = @6
                block ;; label = @7
                  local.get 5
                  i32.eqz
                  br_if 0 (;@7;)
                  local.get 0
                  i32.const 2
                  i32.add
                  local.set 7
                  local.get 1
                  i32.const -2
                  i32.add
                  local.set 8
                  i32.const 0
                  local.set 6
                  i32.const 0
                  local.get 5
                  i32.sub
                  local.set 9
                  loop ;; label = @8
                    local.get 8
                    local.get 6
                    i32.eq
                    br_if 2 (;@6;)
                    local.get 7
                    local.get 6
                    i32.add
                    i32.const 48
                    i32.store8
                    local.get 9
                    local.get 6
                    i32.const 1
                    i32.add
                    local.tee 6
                    i32.ne
                    br_if 0 (;@8;)
                  end
                end
                local.get 1
                i32.const 2
                local.get 5
                i32.sub
                local.tee 6
                i32.lt_u
                br_if 1 (;@5;)
                local.get 0
                local.get 6
                i32.add
                local.get 1
                local.get 6
                i32.sub
                local.get 2
                local.get 4
                call $_ZN16wado_bundled_fts12write_digits17h1647d1324138129fE
                i32.const 2
                local.get 3
                i32.sub
                return
              end
              local.get 6
              i32.const 2
              i32.add
              local.get 1
              call $_ZN4core9panicking18panic_bounds_check17h62ab6f5933ba978dE
              unreachable
            end
            local.get 6
            local.get 1
            local.get 1
            call $_ZN4core5slice5index16slice_index_fail17h41740fce9a5ba04bE
            unreachable
          end
          local.get 0
          local.get 1
          local.get 2
          local.get 4
          call $_ZN16wado_bundled_fts12write_digits17h1647d1324138129fE
          block ;; label = @4
            local.get 3
            i32.eqz
            br_if 0 (;@4;)
            loop ;; label = @5
              local.get 1
              local.get 4
              i32.eq
              br_if 4 (;@1;)
              local.get 0
              local.get 4
              i32.add
              i32.const 48
              i32.store8
              local.get 4
              i32.const 1
              i32.add
              local.set 4
              local.get 3
              i32.const -1
              i32.add
              local.tee 3
              br_if 0 (;@5;)
            end
          end
          local.get 5
          local.get 1
          i32.ge_u
          br_if 0 (;@3;)
          local.get 0
          local.get 5
          i32.add
          i32.const 46
          i32.store8
          local.get 5
          i32.const 1
          i32.add
          local.tee 4
          local.get 1
          i32.ge_u
          br_if 1 (;@2;)
          local.get 0
          local.get 4
          i32.add
          i32.const 48
          i32.store8
          local.get 5
          i32.const 2
          i32.add
          return
        end
        local.get 5
        local.get 1
        call $_ZN4core9panicking18panic_bounds_check17h62ab6f5933ba978dE
        unreachable
      end
      local.get 4
      local.get 1
      call $_ZN4core9panicking18panic_bounds_check17h62ab6f5933ba978dE
      unreachable
    end
    local.get 4
    local.get 1
    call $_ZN4core9panicking18panic_bounds_check17h62ab6f5933ba978dE
    unreachable
  )
  (func $_ZN16wado_bundled_fts12write_digits17h1647d1324138129fE (;6;) (type 6) (param i32 i32 i64 i32)
    (local i32 i64)
    block ;; label = @1
      block ;; label = @2
        local.get 3
        i32.eqz
        br_if 0 (;@2;)
        local.get 3
        i32.const -1
        i32.add
        local.tee 4
        local.set 3
        loop ;; label = @3
          local.get 4
          local.get 1
          i32.ge_u
          br_if 2 (;@1;)
          local.get 0
          local.get 3
          i32.add
          local.get 2
          i64.const 10
          i64.div_u
          local.tee 5
          i64.const 246
          i64.mul
          local.get 2
          i64.add
          i32.wrap_i64
          i32.const 48
          i32.or
          i32.store8
          local.get 5
          local.set 2
          local.get 3
          i32.const -1
          i32.add
          local.tee 3
          i32.const -1
          i32.ne
          br_if 0 (;@3;)
        end
      end
      return
    end
    local.get 3
    local.get 1
    call $_ZN4core9panicking18panic_bounds_check17h62ab6f5933ba978dE
    unreachable
  )
  (func $_ZN4core5slice5index16slice_index_fail17h41740fce9a5ba04bE (;7;) (type 7) (param i32 i32 i32)
    call $_ZN4core9panicking9panic_fmt17hcb6b2b4be1f4be38E
    unreachable
  )
  (func $_ZN16wado_bundled_fts13f64_fmt_fixed17h01be26eb6efc2210E (;8;) (type 8) (param f64 i32 i32) (result i32)
    (local i32 i32 i32 i32 i32 i64 i32 i32 i32 i32 i32 i32)
    global.get $__stack_pointer
    i32.const 16
    i32.sub
    local.tee 3
    global.set $__stack_pointer
    block ;; label = @1
      block ;; label = @2
        block ;; label = @3
          block ;; label = @4
            block ;; label = @5
              block ;; label = @6
                local.get 0
                local.get 0
                f64.ne
                br_if 0 (;@6;)
                local.get 0
                f64.const inf (;=inf;)
                f64.eq
                br_if 1 (;@5;)
                local.get 0
                f64.const -inf (;=-inf;)
                f64.eq
                br_if 2 (;@4;)
                block ;; label = @7
                  local.get 0
                  f64.const 0x0p+0 (;=0;)
                  f64.eq
                  br_if 0 (;@7;)
                  i32.const 0
                  local.set 4
                  local.get 0
                  f64.const 0x0p+0 (;=0;)
                  f64.lt
                  br_if 4 (;@3;)
                  br 5 (;@2;)
                end
                i32.const 0
                local.set 5
                block ;; label = @7
                  local.get 0
                  i64.reinterpret_f64
                  i64.const -1
                  i64.gt_s
                  br_if 0 (;@7;)
                  local.get 2
                  i32.const 45
                  i32.store8
                  i32.const 1
                  local.set 5
                end
                local.get 2
                local.get 5
                i32.add
                i32.const 11824
                i32.store16 align=1
                block ;; label = @7
                  block ;; label = @8
                    local.get 1
                    i32.eqz
                    br_if 0 (;@8;)
                    local.get 5
                    i32.const 2
                    i32.or
                    local.set 6
                    local.get 1
                    local.set 7
                    loop ;; label = @9
                      local.get 6
                      i32.const 400
                      i32.eq
                      br_if 2 (;@7;)
                      local.get 2
                      local.get 6
                      i32.add
                      i32.const 48
                      i32.store8
                      local.get 6
                      i32.const 1
                      i32.add
                      local.set 6
                      local.get 7
                      i32.const -1
                      i32.add
                      local.tee 7
                      br_if 0 (;@9;)
                    end
                  end
                  local.get 1
                  local.get 5
                  i32.add
                  i32.const 2
                  i32.add
                  local.set 6
                  br 6 (;@1;)
                end
                local.get 6
                i32.const 400
                call $_ZN4core9panicking18panic_bounds_check17h62ab6f5933ba978dE
                unreachable
              end
              local.get 2
              i32.const 2
              i32.add
              i32.const 0
              i32.load8_u offset=1048581
              i32.store8
              local.get 2
              i32.const 0
              i32.load16_u offset=1048579 align=1
              i32.store16 align=1
              i32.const 3
              local.set 6
              br 4 (;@1;)
            end
            local.get 2
            i32.const 2
            i32.add
            i32.const 0
            i32.load8_u offset=1048578
            i32.store8
            local.get 2
            i32.const 0
            i32.load16_u offset=1048576 align=1
            i32.store16 align=1
            i32.const 3
            local.set 6
            br 3 (;@1;)
          end
          local.get 2
          i32.const 1718511917
          i32.store align=1
          i32.const 4
          local.set 6
          br 2 (;@1;)
        end
        local.get 2
        i32.const 45
        i32.store8
        local.get 0
        f64.neg
        local.set 0
        i32.const 1
        local.set 4
      end
      local.get 3
      local.get 0
      call $_ZN5fpfmt5short17hcb22e12847c1a5c4E
      block ;; label = @2
        block ;; label = @3
          i32.const 64
          local.get 3
          i64.load
          local.tee 8
          i64.clz
          i32.wrap_i64
          i32.sub
          i32.const 78913
          i32.mul
          i32.const 18
          i32.shr_u
          local.tee 6
          local.get 8
          local.get 6
          i32.const 3
          i32.shl
          i64.load offset=1048592
          i64.ge_u
          i32.add
          local.tee 7
          local.get 3
          i32.load offset=8
          i32.add
          local.tee 6
          i32.const 1
          i32.lt_s
          br_if 0 (;@3;)
          local.get 6
          local.get 1
          i32.add
          local.tee 6
          i32.const 18
          local.get 6
          i32.const 18
          i32.lt_s
          select
          local.set 6
          br 1 (;@2;)
        end
        local.get 1
        local.get 6
        i32.sub
        local.tee 6
        i32.const 1
        local.get 6
        i32.const 1
        i32.gt_s
        select
        local.tee 6
        i32.const 18
        local.get 6
        i32.const 18
        i32.lt_s
        select
        local.set 6
      end
      local.get 3
      local.get 0
      local.get 6
      i32.const 1
      local.get 6
      i32.const 1
      i32.gt_s
      select
      local.tee 6
      i32.const 17
      local.get 6
      i32.const 17
      i32.lt_s
      select
      local.get 6
      local.get 6
      local.get 7
      i32.gt_u
      select
      call $_ZN5fpfmt11fixed_width17hf26acbcd246ad846E
      local.get 2
      local.get 4
      i32.add
      local.set 2
      i32.const 400
      local.get 4
      i32.sub
      local.set 5
      block ;; label = @2
        block ;; label = @3
          i32.const 64
          local.get 3
          i64.load
          local.tee 8
          i64.clz
          i32.wrap_i64
          i32.sub
          i32.const 78913
          i32.mul
          i32.const 18
          i32.shr_u
          local.tee 9
          local.get 8
          local.get 9
          i32.const 3
          i32.shl
          i64.load offset=1048592
          i64.ge_u
          local.tee 10
          i32.add
          local.tee 7
          local.get 3
          i32.load offset=8
          local.tee 11
          i32.add
          local.tee 12
          i32.const 1
          i32.lt_s
          br_if 0 (;@3;)
          local.get 2
          local.get 5
          local.get 8
          local.get 7
          call $_ZN16wado_bundled_fts12write_digits17h1647d1324138129fE
          block ;; label = @4
            block ;; label = @5
              block ;; label = @6
                block ;; label = @7
                  block ;; label = @8
                    local.get 12
                    local.get 7
                    i32.ge_u
                    br_if 0 (;@8;)
                    local.get 7
                    local.set 6
                    loop ;; label = @9
                      local.get 2
                      local.get 6
                      i32.add
                      local.tee 13
                      local.get 13
                      i32.const -1
                      i32.add
                      i32.load8_u
                      i32.store8
                      local.get 6
                      i32.const -1
                      i32.add
                      local.tee 6
                      local.get 12
                      i32.gt_u
                      br_if 0 (;@9;)
                    end
                    local.get 2
                    local.get 12
                    i32.add
                    i32.const 46
                    i32.store8
                    local.get 12
                    i32.const 1
                    i32.add
                    local.set 12
                    block ;; label = @9
                      local.get 1
                      i32.const 0
                      local.get 11
                      i32.sub
                      i32.le_u
                      br_if 0 (;@9;)
                      local.get 11
                      local.get 1
                      i32.add
                      local.tee 13
                      i32.eqz
                      br_if 0 (;@9;)
                      local.get 7
                      i32.const 1
                      i32.add
                      local.set 6
                      loop ;; label = @10
                        local.get 6
                        local.get 5
                        i32.ge_u
                        br_if 3 (;@7;)
                        local.get 2
                        local.get 6
                        i32.add
                        i32.const 48
                        i32.store8
                        local.get 6
                        i32.const 1
                        i32.add
                        local.set 6
                        local.get 13
                        i32.const -1
                        i32.add
                        local.tee 13
                        br_if 0 (;@10;)
                      end
                    end
                    local.get 12
                    local.get 1
                    i32.add
                    local.set 6
                    br 6 (;@2;)
                  end
                  block ;; label = @8
                    local.get 11
                    i32.eqz
                    br_if 0 (;@8;)
                    loop ;; label = @9
                      local.get 5
                      local.get 7
                      i32.eq
                      br_if 5 (;@4;)
                      local.get 2
                      local.get 7
                      i32.add
                      i32.const 48
                      i32.store8
                      local.get 7
                      i32.const 1
                      i32.add
                      local.set 7
                      local.get 11
                      i32.const -1
                      i32.add
                      local.tee 11
                      br_if 0 (;@9;)
                    end
                  end
                  local.get 12
                  local.get 5
                  i32.ge_u
                  br_if 1 (;@6;)
                  local.get 2
                  local.get 12
                  i32.add
                  i32.const 46
                  i32.store8
                  local.get 12
                  i32.const 1
                  i32.add
                  local.set 13
                  block ;; label = @8
                    local.get 1
                    i32.eqz
                    br_if 0 (;@8;)
                    local.get 13
                    local.set 6
                    local.get 1
                    local.set 7
                    loop ;; label = @9
                      local.get 6
                      local.get 5
                      i32.ge_u
                      br_if 4 (;@5;)
                      local.get 2
                      local.get 6
                      i32.add
                      i32.const 48
                      i32.store8
                      local.get 6
                      i32.const 1
                      i32.add
                      local.set 6
                      local.get 7
                      i32.const -1
                      i32.add
                      local.tee 7
                      br_if 0 (;@9;)
                    end
                  end
                  local.get 13
                  local.get 1
                  i32.add
                  local.set 6
                  br 5 (;@2;)
                end
                local.get 6
                local.get 5
                call $_ZN4core9panicking18panic_bounds_check17h62ab6f5933ba978dE
                unreachable
              end
              local.get 12
              local.get 5
              call $_ZN4core9panicking18panic_bounds_check17h62ab6f5933ba978dE
              unreachable
            end
            local.get 6
            local.get 5
            call $_ZN4core9panicking18panic_bounds_check17h62ab6f5933ba978dE
            unreachable
          end
          local.get 7
          local.get 5
          call $_ZN4core9panicking18panic_bounds_check17h62ab6f5933ba978dE
          unreachable
        end
        local.get 2
        i32.const 11824
        i32.store16 align=1
        block ;; label = @3
          block ;; label = @4
            local.get 1
            i32.const 0
            local.get 12
            i32.sub
            i32.gt_u
            br_if 0 (;@4;)
            block ;; label = @5
              local.get 1
              i32.eqz
              br_if 0 (;@5;)
              i32.const 398
              local.get 4
              i32.sub
              local.set 7
              local.get 2
              i32.const 2
              i32.add
              local.set 2
              i32.const 0
              local.set 6
              loop ;; label = @6
                local.get 7
                local.get 6
                i32.eq
                br_if 3 (;@3;)
                local.get 2
                local.get 6
                i32.add
                i32.const 48
                i32.store8
                local.get 1
                local.get 6
                i32.const 1
                i32.add
                local.tee 6
                i32.ne
                br_if 0 (;@6;)
              end
            end
            local.get 1
            i32.const 2
            i32.add
            local.set 6
            br 2 (;@2;)
          end
          block ;; label = @4
            block ;; label = @5
              block ;; label = @6
                local.get 12
                i32.eqz
                br_if 0 (;@6;)
                i32.const 398
                local.get 4
                i32.sub
                local.set 13
                local.get 2
                i32.const 2
                i32.add
                local.set 14
                i32.const 0
                local.set 6
                i32.const 0
                local.get 11
                local.get 9
                i32.add
                local.get 10
                i32.add
                i32.sub
                local.set 11
                loop ;; label = @7
                  local.get 13
                  local.get 6
                  i32.eq
                  br_if 2 (;@5;)
                  local.get 14
                  local.get 6
                  i32.add
                  i32.const 48
                  i32.store8
                  local.get 11
                  local.get 6
                  i32.const 1
                  i32.add
                  local.tee 6
                  i32.ne
                  br_if 0 (;@7;)
                end
              end
              local.get 5
              i32.const 2
              local.get 12
              i32.sub
              local.tee 6
              i32.lt_u
              br_if 1 (;@4;)
              local.get 2
              local.get 6
              i32.add
              local.get 5
              local.get 6
              i32.sub
              local.get 8
              local.get 7
              local.get 12
              local.get 1
              i32.add
              local.tee 6
              local.get 7
              local.get 6
              i32.lt_u
              select
              local.tee 6
              call $_ZN16wado_bundled_fts12write_digits17h1647d1324138129fE
              block ;; label = @6
                block ;; label = @7
                  local.get 6
                  local.get 12
                  i32.sub
                  local.tee 6
                  local.get 1
                  i32.ge_u
                  br_if 0 (;@7;)
                  local.get 2
                  i32.const 2
                  i32.add
                  local.set 2
                  loop ;; label = @8
                    local.get 6
                    i32.const 2
                    i32.add
                    local.tee 7
                    local.get 5
                    i32.ge_u
                    br_if 2 (;@6;)
                    local.get 2
                    local.get 6
                    i32.add
                    i32.const 48
                    i32.store8
                    local.get 6
                    i32.const 1
                    i32.add
                    local.tee 6
                    local.get 1
                    i32.lt_u
                    br_if 0 (;@8;)
                  end
                end
                local.get 1
                i32.const 2
                i32.add
                local.set 6
                br 4 (;@2;)
              end
              local.get 7
              local.get 5
              call $_ZN4core9panicking18panic_bounds_check17h62ab6f5933ba978dE
              unreachable
            end
            local.get 6
            i32.const 2
            i32.add
            local.get 5
            call $_ZN4core9panicking18panic_bounds_check17h62ab6f5933ba978dE
            unreachable
          end
          local.get 6
          local.get 5
          local.get 5
          call $_ZN4core5slice5index16slice_index_fail17h41740fce9a5ba04bE
          unreachable
        end
        local.get 6
        i32.const 2
        i32.add
        local.get 5
        call $_ZN4core9panicking18panic_bounds_check17h62ab6f5933ba978dE
        unreachable
      end
      local.get 6
      local.get 4
      i32.add
      local.set 6
    end
    local.get 3
    i32.const 16
    i32.add
    global.set $__stack_pointer
    local.get 6
  )
  (func $f32_to_buffer (;9;) (type 9) (param f32 i32) (result i32)
    (local i32 i32 i32 i32 f64 i32 i32 i32 i64 i64 i64 i64 i32)
    global.get $__stack_pointer
    i32.const 112
    i32.sub
    local.tee 2
    global.set $__stack_pointer
    local.get 2
    i32.const 64
    i32.add
    i64.const 0
    i64.store
    local.get 2
    i32.const 56
    i32.add
    i64.const 0
    i64.store
    local.get 2
    i64.const 0
    i64.store offset=48
    block ;; label = @1
      block ;; label = @2
        block ;; label = @3
          block ;; label = @4
            block ;; label = @5
              block ;; label = @6
                block ;; label = @7
                  block ;; label = @8
                    local.get 0
                    local.get 0
                    f32.ne
                    br_if 0 (;@8;)
                    local.get 0
                    f32.const inf (;=inf;)
                    f32.eq
                    br_if 1 (;@7;)
                    local.get 0
                    f32.const -inf (;=-inf;)
                    f32.eq
                    br_if 2 (;@6;)
                    block ;; label = @9
                      local.get 0
                      f32.const 0x0p+0 (;=0;)
                      f32.eq
                      br_if 0 (;@9;)
                      local.get 2
                      i32.const 48
                      i32.add
                      local.set 3
                      block ;; label = @10
                        local.get 0
                        f32.const 0x0p+0 (;=0;)
                        f32.lt
                        br_if 0 (;@10;)
                        i32.const 0
                        local.set 4
                        br 7 (;@3;)
                      end
                      local.get 2
                      i32.const 48
                      i32.add
                      i32.const 1
                      i32.or
                      local.set 3
                      local.get 2
                      i32.const 45
                      i32.store8 offset=48
                      local.get 0
                      f32.neg
                      local.set 0
                      i32.const 1
                      local.set 4
                      br 6 (;@3;)
                    end
                    local.get 0
                    i32.reinterpret_f32
                    i32.const -1
                    i32.gt_s
                    br_if 3 (;@5;)
                    local.get 2
                    i32.const 808333357
                    i32.store offset=48
                    i32.const 4
                    local.set 5
                    br 6 (;@2;)
                  end
                  local.get 2
                  i32.const 0
                  i32.load8_u offset=1048581
                  i32.store8 offset=50
                  local.get 2
                  i32.const 0
                  i32.load16_u offset=1048579 align=1
                  i32.store16 offset=48
                  br 3 (;@4;)
                end
                local.get 2
                i32.const 0
                i32.load8_u offset=1048578
                i32.store8 offset=50
                local.get 2
                i32.const 0
                i32.load16_u offset=1048576 align=1
                i32.store16 offset=48
                br 2 (;@4;)
              end
              local.get 2
              i32.const 1718511917
              i32.store offset=48
              i32.const 4
              local.set 5
              br 3 (;@2;)
            end
            local.get 2
            i32.const 0
            i32.load8_u offset=1048584
            i32.store8 offset=50
            local.get 2
            i32.const 0
            i32.load16_u offset=1048582 align=1
            i32.store16 offset=48
          end
          i32.const 3
          local.set 5
          br 1 (;@2;)
        end
        local.get 0
        f64.promote_f32
        local.set 6
        i32.const 1
        local.set 7
        block ;; label = @3
          loop ;; label = @4
            local.get 2
            i32.const 72
            i32.add
            local.get 6
            local.get 7
            call $_ZN5fpfmt11fixed_width17hf26acbcd246ad846E
            local.get 2
            i32.load offset=80
            local.tee 8
            i32.const 27
            i32.div_s
            local.tee 5
            local.get 8
            local.get 5
            i32.const 27
            i32.mul
            i32.sub
            local.tee 5
            i32.const 31
            i32.shr_s
            i32.add
            i32.const 13
            i32.add
            local.tee 9
            i32.const 26
            i32.ge_u
            br_if 3 (;@1;)
            local.get 2
            i64.load offset=72
            local.set 10
            local.get 2
            i32.const 32
            i32.add
            local.get 9
            i32.const 4
            i32.shl
            local.tee 9
            i64.load offset=1048968
            local.get 9
            i64.load offset=1048976
            local.tee 11
            i64.const 0
            i64.ne
            i64.extend_i32_u
            i64.sub
            i64.const 0
            local.get 5
            i32.const 27
            i32.add
            local.get 5
            local.get 5
            i32.const 0
            i32.lt_s
            select
            i32.const 3
            i32.shl
            i64.load offset=1048752
            local.tee 12
            i64.const 0
            call $__multi3
            local.get 2
            i32.const 16
            i32.add
            i64.const 0
            local.get 11
            i64.sub
            i64.const 0
            local.get 12
            i64.const 0
            call $__multi3
            local.get 2
            local.get 2
            i64.load offset=24
            local.tee 12
            local.get 2
            i64.load offset=32
            i64.add
            local.tee 11
            local.get 2
            i64.load offset=16
            i64.const 0
            i64.ne
            i64.extend_i32_u
            i64.add
            local.tee 13
            local.get 2
            i64.load offset=40
            local.get 11
            local.get 12
            i64.lt_u
            i64.extend_i32_u
            i64.add
            local.get 13
            local.get 11
            i64.lt_u
            i64.extend_i32_u
            i64.add
            local.tee 11
            local.get 11
            i64.const 63
            i64.shr_s
            i32.wrap_i64
            i32.const 1
            i32.add
            call $__ashlti3
            local.get 2
            local.get 10
            i64.clz
            local.tee 11
            i32.wrap_i64
            local.tee 5
            local.get 8
            i32.const 108853
            i32.mul
            i32.const 15
            i32.shr_s
            local.tee 9
            local.get 5
            local.get 9
            i32.sub
            i32.const -11
            i32.add
            local.tee 9
            i32.const 1074
            local.get 9
            i32.const 1074
            i32.lt_s
            select
            local.tee 9
            i32.add
            i32.sub
            i32.const -3
            i32.add
            i32.store offset=104
            local.get 2
            i64.const 0
            local.get 2
            i64.load
            local.tee 12
            i64.sub
            i64.store offset=96
            local.get 2
            local.get 12
            i64.const 0
            i64.ne
            i64.extend_i32_u
            local.get 2
            i64.load offset=8
            i64.add
            i64.store offset=88
            local.get 10
            local.get 11
            i64.shl
            local.get 2
            i32.const 88
            i32.add
            call $_ZN5fpfmt6uscale17h620c53c8994a3d2aE
            local.tee 11
            local.get 11
            i64.const 36028797018963965
            i64.gt_u
            local.tee 14
            i64.extend_i32_u
            i64.shr_u
            local.tee 12
            local.get 11
            i64.const 1
            i64.and
            i64.or
            local.get 12
            i64.const 2
            i64.shr_u
            i64.const 1
            i64.and
            i64.add
            i64.const 1
            i64.add
            local.tee 12
            i64.const 2
            i64.shr_u
            local.set 11
            block ;; label = @5
              local.get 12
              i64.const 18014398509481984
              i64.and
              i64.eqz
              br_if 0 (;@5;)
              local.get 11
              i64.const 4607182418800017407
              i64.and
              local.get 14
              local.get 9
              i32.sub
              i32.const 1075
              i32.add
              i64.extend_i32_u
              i64.const 52
              i64.shl
              i64.or
              local.set 11
            end
            local.get 0
            local.get 11
            f64.reinterpret_i64
            f32.demote_f64
            f32.eq
            br_if 1 (;@3;)
            local.get 7
            i32.const 1
            i32.add
            local.tee 7
            i32.const 10
            i32.ne
            br_if 0 (;@4;)
          end
          local.get 2
          i32.const 72
          i32.add
          local.get 6
          i32.const 9
          call $_ZN5fpfmt11fixed_width17hf26acbcd246ad846E
          local.get 2
          i64.load offset=72
          local.tee 10
          i64.clz
          i32.wrap_i64
          local.set 5
          local.get 2
          i32.load offset=80
          local.set 8
        end
        local.get 3
        i32.const 24
        local.get 4
        i32.sub
        local.get 10
        local.get 8
        i32.const 64
        local.get 5
        i32.sub
        i32.const 78913
        i32.mul
        i32.const 18
        i32.shr_u
        local.tee 5
        local.get 10
        local.get 5
        i32.const 3
        i32.shl
        i64.load offset=1048592
        i64.ge_u
        i32.add
        call $_ZN16wado_bundled_fts12fmt_shortest17h46d411f7a4784c84E
        local.get 4
        i32.add
        local.tee 5
        i32.const 25
        i32.lt_u
        br_if 0 (;@2;)
        i32.const 0
        local.get 5
        i32.const 24
        call $_ZN4core5slice5index16slice_index_fail17h41740fce9a5ba04bE
        unreachable
      end
      block ;; label = @2
        local.get 5
        i32.eqz
        br_if 0 (;@2;)
        local.get 1
        local.get 2
        i32.const 48
        i32.add
        local.get 5
        memory.copy
      end
      local.get 2
      i32.const 112
      i32.add
      global.set $__stack_pointer
      local.get 5
      return
    end
    local.get 9
    i32.const 26
    call $_ZN4core9panicking18panic_bounds_check17h62ab6f5933ba978dE
    unreachable
  )
  (func $_ZN5fpfmt6uscale17h620c53c8994a3d2aE (;10;) (type 10) (param i64 i32) (result i64)
    (local i32 i64 i64 i64)
    global.get $__stack_pointer
    i32.const 32
    i32.sub
    local.tee 2
    global.set $__stack_pointer
    local.get 2
    i32.const 16
    i32.add
    local.get 1
    i64.load
    i64.const 0
    local.get 0
    i64.const 0
    call $__multi3
    local.get 1
    i64.load32_u offset=16
    local.tee 3
    i64.const 63
    i64.and
    local.set 4
    block ;; label = @1
      block ;; label = @2
        local.get 2
        i64.load offset=24
        local.tee 5
        i64.const -1
        local.get 3
        i64.shl
        i64.const -1
        i64.xor
        i64.and
        i64.const 0
        i64.eq
        br_if 0 (;@2;)
        i64.const 1
        local.set 0
        br 1 (;@1;)
      end
      local.get 2
      i64.load offset=16
      local.set 3
      local.get 2
      local.get 1
      i64.load offset=8
      i64.const 0
      local.get 0
      i64.const 0
      call $__multi3
      local.get 5
      local.get 3
      local.get 2
      i64.load offset=8
      local.tee 0
      i64.lt_u
      i64.extend_i32_u
      i64.sub
      local.set 5
      local.get 3
      local.get 0
      i64.sub
      i64.const 1
      i64.gt_u
      i64.extend_i32_u
      local.set 0
    end
    local.get 2
    i32.const 32
    i32.add
    global.set $__stack_pointer
    local.get 5
    local.get 4
    i64.shr_u
    local.get 0
    i64.or
  )
  (func $f32_to_buffer_exp (;11;) (type 11) (param f32 i32 i32 i32) (result i32)
    (local i32)
    global.get $__stack_pointer
    i32.const 32
    i32.sub
    local.tee 4
    global.set $__stack_pointer
    local.get 4
    i32.const 24
    i32.add
    i64.const 0
    i64.store
    local.get 4
    i32.const 16
    i32.add
    i64.const 0
    i64.store
    local.get 4
    i32.const 8
    i32.add
    i64.const 0
    i64.store
    local.get 4
    i64.const 0
    i64.store
    block ;; label = @1
      local.get 0
      f64.promote_f32
      local.get 1
      local.get 2
      i32.const 0
      i32.ne
      local.get 4
      call $_ZN16wado_bundled_fts11f64_fmt_exp17h24a28c59be25ab6aE
      local.tee 2
      i32.const 33
      i32.ge_u
      br_if 0 (;@1;)
      block ;; label = @2
        local.get 2
        i32.eqz
        br_if 0 (;@2;)
        local.get 3
        local.get 4
        local.get 2
        memory.copy
      end
      local.get 4
      i32.const 32
      i32.add
      global.set $__stack_pointer
      local.get 2
      return
    end
    i32.const 0
    local.get 2
    i32.const 32
    call $_ZN4core5slice5index16slice_index_fail17h41740fce9a5ba04bE
    unreachable
  )
  (func $f32_to_buffer_fixed (;12;) (type 12) (param f32 i32 i32) (result i32)
    (local i32)
    global.get $__stack_pointer
    i32.const 400
    i32.sub
    local.tee 3
    global.set $__stack_pointer
    block ;; label = @1
      i32.const 400
      i32.eqz
      br_if 0 (;@1;)
      local.get 3
      i32.const 0
      i32.const 400
      memory.fill
    end
    block ;; label = @1
      local.get 0
      f64.promote_f32
      local.get 1
      local.get 3
      call $_ZN16wado_bundled_fts13f64_fmt_fixed17h01be26eb6efc2210E
      local.tee 1
      i32.const 401
      i32.ge_u
      br_if 0 (;@1;)
      block ;; label = @2
        local.get 1
        i32.eqz
        br_if 0 (;@2;)
        local.get 1
        i32.eqz
        br_if 0 (;@2;)
        local.get 2
        local.get 3
        local.get 1
        memory.copy
      end
      local.get 3
      i32.const 400
      i32.add
      global.set $__stack_pointer
      local.get 1
      return
    end
    i32.const 0
    local.get 1
    i32.const 400
    call $_ZN4core5slice5index16slice_index_fail17h41740fce9a5ba04bE
    unreachable
  )
  (func $f64_to_buffer (;13;) (type 13) (param f64 i32) (result i32)
    (local i32 i32 i32 i64 i32)
    global.get $__stack_pointer
    i32.const 48
    i32.sub
    local.tee 2
    global.set $__stack_pointer
    local.get 2
    i32.const 24
    i32.add
    i64.const 0
    i64.store
    local.get 2
    i32.const 16
    i32.add
    i64.const 0
    i64.store
    local.get 2
    i32.const 8
    i32.add
    i64.const 0
    i64.store
    local.get 2
    i64.const 0
    i64.store
    block ;; label = @1
      block ;; label = @2
        block ;; label = @3
          block ;; label = @4
            block ;; label = @5
              block ;; label = @6
                local.get 0
                local.get 0
                f64.ne
                br_if 0 (;@6;)
                local.get 0
                f64.const inf (;=inf;)
                f64.eq
                br_if 1 (;@5;)
                local.get 0
                f64.const -inf (;=-inf;)
                f64.eq
                br_if 2 (;@4;)
                block ;; label = @7
                  block ;; label = @8
                    block ;; label = @9
                      local.get 0
                      f64.const 0x0p+0 (;=0;)
                      f64.eq
                      br_if 0 (;@9;)
                      local.get 0
                      f64.const 0x0p+0 (;=0;)
                      f64.lt
                      br_if 1 (;@8;)
                      i32.const 0
                      local.set 3
                      local.get 2
                      local.set 4
                      br 2 (;@7;)
                    end
                    local.get 0
                    i64.reinterpret_f64
                    i64.const -1
                    i64.gt_s
                    br_if 5 (;@3;)
                    local.get 2
                    i32.const 808333357
                    i32.store
                    i32.const 4
                    local.set 3
                    br 7 (;@1;)
                  end
                  local.get 2
                  i32.const 1
                  i32.or
                  local.set 4
                  local.get 2
                  i32.const 45
                  i32.store8
                  local.get 0
                  f64.neg
                  local.set 0
                  i32.const 1
                  local.set 3
                end
                local.get 2
                i32.const 32
                i32.add
                local.get 0
                call $_ZN5fpfmt5short17hcb22e12847c1a5c4E
                local.get 4
                i32.const 32
                local.get 3
                i32.sub
                local.get 2
                i64.load offset=32
                local.tee 5
                local.get 2
                i32.load offset=40
                i32.const 64
                local.get 5
                i64.clz
                i32.wrap_i64
                i32.sub
                i32.const 78913
                i32.mul
                i32.const 18
                i32.shr_u
                local.tee 6
                local.get 5
                local.get 6
                i32.const 3
                i32.shl
                i64.load offset=1048592
                i64.ge_u
                i32.add
                call $_ZN16wado_bundled_fts12fmt_shortest17h46d411f7a4784c84E
                local.get 3
                i32.add
                local.tee 3
                i32.const 33
                i32.lt_u
                br_if 5 (;@1;)
                i32.const 0
                local.get 3
                i32.const 32
                call $_ZN4core5slice5index16slice_index_fail17h41740fce9a5ba04bE
                unreachable
              end
              local.get 2
              i32.const 0
              i32.load8_u offset=1048581
              i32.store8 offset=2
              local.get 2
              i32.const 0
              i32.load16_u offset=1048579 align=1
              i32.store16
              br 3 (;@2;)
            end
            local.get 2
            i32.const 0
            i32.load8_u offset=1048578
            i32.store8 offset=2
            local.get 2
            i32.const 0
            i32.load16_u offset=1048576 align=1
            i32.store16
            br 2 (;@2;)
          end
          local.get 2
          i32.const 1718511917
          i32.store
          i32.const 4
          local.set 3
          br 2 (;@1;)
        end
        local.get 2
        i32.const 0
        i32.load8_u offset=1048584
        i32.store8 offset=2
        local.get 2
        i32.const 0
        i32.load16_u offset=1048582 align=1
        i32.store16
      end
      i32.const 3
      local.set 3
    end
    block ;; label = @1
      local.get 3
      i32.eqz
      br_if 0 (;@1;)
      local.get 1
      local.get 2
      local.get 3
      memory.copy
    end
    local.get 2
    i32.const 48
    i32.add
    global.set $__stack_pointer
    local.get 3
  )
  (func $f64_to_buffer_exp (;14;) (type 0) (param f64 i32 i32 i32) (result i32)
    (local i32)
    global.get $__stack_pointer
    i32.const 32
    i32.sub
    local.tee 4
    global.set $__stack_pointer
    local.get 4
    i32.const 24
    i32.add
    i64.const 0
    i64.store
    local.get 4
    i32.const 16
    i32.add
    i64.const 0
    i64.store
    local.get 4
    i32.const 8
    i32.add
    i64.const 0
    i64.store
    local.get 4
    i64.const 0
    i64.store
    block ;; label = @1
      local.get 0
      local.get 1
      local.get 2
      i32.const 0
      i32.ne
      local.get 4
      call $_ZN16wado_bundled_fts11f64_fmt_exp17h24a28c59be25ab6aE
      local.tee 2
      i32.const 33
      i32.ge_u
      br_if 0 (;@1;)
      block ;; label = @2
        local.get 2
        i32.eqz
        br_if 0 (;@2;)
        local.get 3
        local.get 4
        local.get 2
        memory.copy
      end
      local.get 4
      i32.const 32
      i32.add
      global.set $__stack_pointer
      local.get 2
      return
    end
    i32.const 0
    local.get 2
    i32.const 32
    call $_ZN4core5slice5index16slice_index_fail17h41740fce9a5ba04bE
    unreachable
  )
  (func $f64_to_buffer_fixed (;15;) (type 8) (param f64 i32 i32) (result i32)
    (local i32)
    global.get $__stack_pointer
    i32.const 400
    i32.sub
    local.tee 3
    global.set $__stack_pointer
    block ;; label = @1
      i32.const 400
      i32.eqz
      br_if 0 (;@1;)
      local.get 3
      i32.const 0
      i32.const 400
      memory.fill
    end
    block ;; label = @1
      local.get 0
      local.get 1
      local.get 3
      call $_ZN16wado_bundled_fts13f64_fmt_fixed17h01be26eb6efc2210E
      local.tee 1
      i32.const 401
      i32.ge_u
      br_if 0 (;@1;)
      block ;; label = @2
        local.get 1
        i32.eqz
        br_if 0 (;@2;)
        local.get 1
        i32.eqz
        br_if 0 (;@2;)
        local.get 2
        local.get 3
        local.get 1
        memory.copy
      end
      local.get 3
      i32.const 400
      i32.add
      global.set $__stack_pointer
      local.get 1
      return
    end
    i32.const 0
    local.get 1
    i32.const 400
    call $_ZN4core5slice5index16slice_index_fail17h41740fce9a5ba04bE
    unreachable
  )
  (func $_ZN4core9panicking9panic_fmt17hcb6b2b4be1f4be38E (;16;) (type 14)
    unreachable
  )
  (func $__multi3 (;17;) (type 15) (param i32 i64 i64 i64 i64)
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
  (func $__ashlti3 (;18;) (type 16) (param i32 i64 i64 i32)
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
  (data $.rodata (;0;) (i32.const 1048576) "infNaN0.0\00\00\00\00\00\00\00\01\00\00\00\00\00\00\00\0a\00\00\00\00\00\00\00d\00\00\00\00\00\00\00\e8\03\00\00\00\00\00\00\10'\00\00\00\00\00\00\a0\86\01\00\00\00\00\00@B\0f\00\00\00\00\00\80\96\98\00\00\00\00\00\00\e1\f5\05\00\00\00\00\00\ca\9a;\00\00\00\00\00\e4\0bT\02\00\00\00\00\e8vH\17\00\00\00\00\10\a5\d4\e8\00\00\00\00\a0rN\18\09\00\00\00@z\10\f3Z\00\00\00\80\c6\a4~\8d\03\00\00\00\c1o\f2\86#\00\00\00\8a]xEc\01\00\00d\a7\b3\b6\e0\0d\00\00\e8\89\04#\c7\8a\00\00\00\00\00\00\00\80\00\00\00\00\00\00\00\a0\00\00\00\00\00\00\00\c8\00\00\00\00\00\00\00\fa\00\00\00\00\00\00@\9c\00\00\00\00\00\00P\c3\00\00\00\00\00\00$\f4\00\00\00\00\00\80\96\98\00\00\00\00\00 \bc\be\00\00\00\00\00(k\ee\00\00\00\00\00\f9\02\95\00\00\00\00@\b7C\ba\00\00\00\00\10\a5\d4\e8\00\00\00\00*\e7\84\91\00\00\00\80\f4 \e6\b5\00\00\00\a01\a9_\e3\00\00\00\04\bf\c9\1b\8e\00\00\00\c5.\bc\a2\b1\00\00@v:k\0b\de\00\00\e8\89\04#\c7\8a\00\00b\ac\c5\ebx\ad\00\80z\17\b7&\d7\d8\00\90\acn2x\86\87\00\b4W\0a?\16h\a9\00\a1\ed\cc\ce\1b\c2\d3\a0\84\14@aQY\84\c8\a5\19\90\b9\a5o\a5\af\11X\0c\ac\a4I\80\87\9d\82\88\92v\a4\df\eb5\ce]J\89B\cfF\8ay}S\b3\f9\ad\22&\ed8#Xl\a7\b1\0d\09\f5G\0d\d5PiN\22\e2uO>\87n]\fb\17Y\bb\88\a5Ih\96\90\f5[\7f\daav\95\af\8a[\c6P\ed\9d4\c4,9\80\b0L0Ui\b2\86rB?\f5*\88b\86\93\8ec\11}\8d\84K\81\ab\fb\0ak\04\b3)X\e6\ed\ae\d5\ee\5cZK\f3\ec\dd\e4PF\1a\12\ba\ec\1b\93\9e\9d\b2\0cmV&\ba\91\8c\85N\96\90\07\ef*\07\f8\95\c5\a3\c2A\ab\90g\d5\f2<F\c0\bdf\8d\1d\05M\1eu\a4Z\d0(\c4yG\d9\c3\b3\1ehUI~\e0\91\b7\d1t\9e\82\cb\aa0\9b]\a1\88\00\00\00\00\00\00\00\80\00\00\00\00\00\00\00\00:\0f \f4'\8f\cb\ce\00\00\00\00\00\00\00\00RlN\a6@<\0c\a7\dc&\98\a0Ioof\fe\da\e8\b4\99\ac\f0\86\5c\8e\12\c2D\d7_\96\ea\8dp\1ad\ee\01\dajk3\df\b7\90\f1\17\e5\e9\01\b1E\e7\1a\b0p\80\d1\080\a2?\a1~\c2\eb\fb\e9\adA\8e\f8\8c{A\ecp\a7\eb\82.$*(\ef\d3\e5\05Z\92W7\97\e9p\e2.\ce7\06J\a7\b9m\c9\e8(\d4\c1j\92\da\9c\b6\1f\0a=\f8\95q\06\9b\ea\efPB\b5\d0\dc\f2<\a7\01J\f2\13s\c3\98\c6\c4\9cC\08O\e8\09\815\b8\c37\ff\b8\13\7f\d0y\f5\aa\1b\e3\b4\92\db\19\9e.\b9|\95=]\f8\93")
  (@producers
    (language "Rust" "")
    (processed-by "rustc" "1.92.0 (ded5c06cf 2025-12-08)")
  )
  (@custom "target_features" (after data) "\08+\0bbulk-memory+\0fbulk-memory-opt+\16call-indirect-overlong+\0amultivalue+\0fmutable-globals+\13nontrapping-fptoint+\0freference-types+\08sign-ext")
)
