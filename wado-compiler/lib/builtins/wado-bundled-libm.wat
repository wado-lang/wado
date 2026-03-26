(module $wado_bundled_libm.wasm
  (type (;0;) (func (param f64) (result f64)))
  (type (;1;) (func (param f32) (result f32)))
  (type (;2;) (func (param f64 f64) (result f64)))
  (type (;3;) (func (param f32 f32) (result f32)))
  (type (;4;) (func (param f64 f64 f64) (result f64)))
  (type (;5;) (func (param i32 f64)))
  (type (;6;) (func (param i32 f32)))
  (type (;7;) (func (param f64 i32) (result f64)))
  (type (;8;) (func))
  (type (;9;) (func (param i32)))
  (type (;10;) (func (param f32 i32) (result f32)))
  (type (;11;) (func (param f64 f64 i32) (result f64)))
  (type (;12;) (func (param i32 i32 i32 i32 i32) (result i32)))
  (type (;13;) (func (param i32 f64 i32)))
  (type (;14;) (func (param i32 i64 i64 i32)))
  (type (;15;) (func (param i32 i64 i64 i64 i64)))
  (memory (;0;) 17)
  (global $__stack_pointer (;0;) (mut i32) (i32.const 1048576))
  (global (;1;) i32 (i32.const 1053712))
  (global (;2;) i32 (i32.const 1053712))
  (export "memory" (memory 0))
  (export "libm_acos" (func $libm_acos))
  (export "libm_acosf" (func $libm_acosf))
  (export "libm_acosh" (func $libm_acosh))
  (export "libm_acoshf" (func $libm_acoshf))
  (export "libm_asin" (func $libm_asin))
  (export "libm_asinf" (func $libm_asinf))
  (export "libm_asinh" (func $libm_asinh))
  (export "libm_asinhf" (func $libm_asinhf))
  (export "libm_atan" (func $libm_atan))
  (export "libm_atan2" (func $libm_atan2))
  (export "libm_atan2f" (func $libm_atan2f))
  (export "libm_atanf" (func $libm_atanf))
  (export "libm_atanh" (func $libm_atanh))
  (export "libm_atanhf" (func $libm_atanhf))
  (export "libm_cbrt" (func $libm_cbrt))
  (export "libm_cbrtf" (func $libm_cbrtf))
  (export "libm_cos" (func $libm_cos))
  (export "libm_cosf" (func $libm_cosf))
  (export "libm_cosh" (func $libm_cosh))
  (export "libm_coshf" (func $libm_coshf))
  (export "libm_exp" (func $libm_exp))
  (export "libm_exp2" (func $libm_exp2))
  (export "libm_exp2f" (func $libm_exp2f))
  (export "libm_expf" (func $libm_expf))
  (export "libm_expm1" (func $libm_expm1))
  (export "libm_expm1f" (func $libm_expm1f))
  (export "libm_fmod" (func $libm_fmod))
  (export "libm_fmodf" (func $libm_fmodf))
  (export "libm_hypot" (func $libm_hypot))
  (export "libm_hypotf" (func $libm_hypotf))
  (export "libm_log" (func $libm_log))
  (export "libm_log10" (func $libm_log10))
  (export "libm_log10f" (func $libm_log10f))
  (export "libm_log1p" (func $libm_log1p))
  (export "libm_log1pf" (func $libm_log1pf))
  (export "libm_log2" (func $libm_log2))
  (export "libm_log2f" (func $libm_log2f))
  (export "libm_logf" (func $libm_logf))
  (export "libm_pow" (func $libm_pow))
  (export "libm_powf" (func $libm_powf))
  (export "libm_sin" (func $libm_sin))
  (export "libm_sinf" (func $libm_sinf))
  (export "libm_sinh" (func $libm_sinh))
  (export "libm_sinhf" (func $libm_sinhf))
  (export "libm_tan" (func $libm_tan))
  (export "libm_tanf" (func $libm_tanf))
  (export "libm_tanh" (func $libm_tanh))
  (export "libm_tanhf" (func $libm_tanhf))
  (export "__data_end" (global 1))
  (export "__heap_base" (global 2))
  (func $libm_acos (;0;) (type 0) (param f64) (result f64)
    (local i64 i32 f64 f64)
    (block ;; label = @1
      (block ;; label = @2
        (block ;; label = @3
          (br_if 0 (;@3;)
            (i32.gt_u
              (local.tee 2
                (i32.and
                  (i32.wrap_i64
                    (i64.shr_u
                      (local.tee 1
                        (i64.reinterpret_f64
                          (local.get 0)))
                      (i64.const 32)))
                  (i32.const 2147483647)))
              (i32.const 1072693247)))
          (block ;; label = @4
            (br_if 0 (;@4;)
              (i32.lt_u
                (local.get 2)
                (i32.const 1071644672)))
            (block ;; label = @5
              (br_if 0 (;@5;)
                (i64.le_s
                  (local.get 1)
                  (i64.const -1)))
              (return
                (f64.add
                  (local.tee 0
                    (f64.add
                      (f64.add
                        (f64.mul
                          (f64.div
                            (f64.mul
                              (local.tee 0
                                (f64.mul
                                  (f64.sub
                                    (f64.const 0x1p+0 (;=1;))
                                    (local.get 0))
                                  (f64.const 0x1p-1 (;=0.5;))))
                              (f64.add
                                (f64.mul
                                  (local.get 0)
                                  (f64.add
                                    (f64.mul
                                      (local.get 0)
                                      (f64.add
                                        (f64.mul
                                          (local.get 0)
                                          (f64.add
                                            (f64.mul
                                              (local.get 0)
                                              (f64.add
                                                (f64.mul
                                                  (local.get 0)
                                                  (f64.const 0x1.23de10dfdf709p-15 (;=0.00003479331075960212;)))
                                                (f64.const 0x1.9efe07501b288p-11 (;=0.0007915349942898145;))))
                                            (f64.const -0x1.48228b5688f3bp-5 (;=-0.04005553450067941;))))
                                        (f64.const 0x1.9c1550e884455p-3 (;=0.20121253213486293;))))
                                    (f64.const -0x1.4d61203eb6f7dp-2 (;=-0.3255658186224009;))))
                                (f64.const 0x1.5555555555555p-3 (;=0.16666666666666666;))))
                            (f64.add
                              (f64.mul
                                (local.get 0)
                                (f64.add
                                  (f64.mul
                                    (local.get 0)
                                    (f64.add
                                      (f64.mul
                                        (local.get 0)
                                        (f64.add
                                          (f64.mul
                                            (local.get 0)
                                            (f64.const 0x1.3b8c5b12e9282p-4 (;=0.07703815055590194;)))
                                          (f64.const -0x1.6066c1b8d0159p-1 (;=-0.6882839716054533;))))
                                      (f64.const 0x1.02ae59c598ac8p+1 (;=2.0209457602335057;))))
                                  (f64.const -0x1.33a271c8a2d4bp+1 (;=-2.403394911734414;))))
                              (f64.const 0x1p+0 (;=1;))))
                          (local.tee 3
                            (call $_ZN4libm4math4sqrt4sqrt17h3b6b03f022c75fd4E
                              (local.get 0))))
                        (f64.div
                          (f64.sub
                            (local.get 0)
                            (f64.mul
                              (local.tee 4
                                (f64.reinterpret_i64
                                  (i64.and
                                    (i64.reinterpret_f64
                                      (local.get 3))
                                    (i64.const -4294967296))))
                              (local.get 4)))
                          (f64.add
                            (local.get 3)
                            (local.get 4))))
                      (local.get 4)))
                  (local.get 0))))
            (return
              (f64.add
                (local.tee 0
                  (f64.sub
                    (f64.const 0x1.921fb54442d18p+0 (;=1.5707963267948966;))
                    (f64.add
                      (local.tee 4
                        (call $_ZN4libm4math4sqrt4sqrt17h3b6b03f022c75fd4E
                          (local.tee 0
                            (f64.mul
                              (f64.add
                                (local.get 0)
                                (f64.const 0x1p+0 (;=1;)))
                              (f64.const 0x1p-1 (;=0.5;))))))
                      (f64.add
                        (f64.mul
                          (local.get 4)
                          (f64.div
                            (f64.mul
                              (local.get 0)
                              (f64.add
                                (f64.mul
                                  (local.get 0)
                                  (f64.add
                                    (f64.mul
                                      (local.get 0)
                                      (f64.add
                                        (f64.mul
                                          (local.get 0)
                                          (f64.add
                                            (f64.mul
                                              (local.get 0)
                                              (f64.add
                                                (f64.mul
                                                  (local.get 0)
                                                  (f64.const 0x1.23de10dfdf709p-15 (;=0.00003479331075960212;)))
                                                (f64.const 0x1.9efe07501b288p-11 (;=0.0007915349942898145;))))
                                            (f64.const -0x1.48228b5688f3bp-5 (;=-0.04005553450067941;))))
                                        (f64.const 0x1.9c1550e884455p-3 (;=0.20121253213486293;))))
                                    (f64.const -0x1.4d61203eb6f7dp-2 (;=-0.3255658186224009;))))
                                (f64.const 0x1.5555555555555p-3 (;=0.16666666666666666;))))
                            (f64.add
                              (f64.mul
                                (local.get 0)
                                (f64.add
                                  (f64.mul
                                    (local.get 0)
                                    (f64.add
                                      (f64.mul
                                        (local.get 0)
                                        (f64.add
                                          (f64.mul
                                            (local.get 0)
                                            (f64.const 0x1.3b8c5b12e9282p-4 (;=0.07703815055590194;)))
                                          (f64.const -0x1.6066c1b8d0159p-1 (;=-0.6882839716054533;))))
                                      (f64.const 0x1.02ae59c598ac8p+1 (;=2.0209457602335057;))))
                                  (f64.const -0x1.33a271c8a2d4bp+1 (;=-2.403394911734414;))))
                              (f64.const 0x1p+0 (;=1;)))))
                        (f64.const -0x1.1a62633145c07p-54 (;=-0.00000000000000006123233995736766;))))))
                (local.get 0))))
          (local.set 4
            (f64.const 0x1.921fb54442d18p+0 (;=1.5707963267948966;)))
          (br_if 1 (;@2;)
            (i32.lt_u
              (local.get 2)
              (i32.const 1012924417)))
          (return
            (f64.add
              (f64.sub
                (f64.sub
                  (f64.const 0x1.1a62633145c07p-54 (;=0.00000000000000006123233995736766;))
                  (f64.mul
                    (local.get 0)
                    (f64.div
                      (f64.mul
                        (local.tee 4
                          (f64.mul
                            (local.get 0)
                            (local.get 0)))
                        (f64.add
                          (f64.mul
                            (local.get 4)
                            (f64.add
                              (f64.mul
                                (local.get 4)
                                (f64.add
                                  (f64.mul
                                    (local.get 4)
                                    (f64.add
                                      (f64.mul
                                        (local.get 4)
                                        (f64.add
                                          (f64.mul
                                            (local.get 4)
                                            (f64.const 0x1.23de10dfdf709p-15 (;=0.00003479331075960212;)))
                                          (f64.const 0x1.9efe07501b288p-11 (;=0.0007915349942898145;))))
                                      (f64.const -0x1.48228b5688f3bp-5 (;=-0.04005553450067941;))))
                                  (f64.const 0x1.9c1550e884455p-3 (;=0.20121253213486293;))))
                              (f64.const -0x1.4d61203eb6f7dp-2 (;=-0.3255658186224009;))))
                          (f64.const 0x1.5555555555555p-3 (;=0.16666666666666666;))))
                      (f64.add
                        (f64.mul
                          (local.get 4)
                          (f64.add
                            (f64.mul
                              (local.get 4)
                              (f64.add
                                (f64.mul
                                  (local.get 4)
                                  (f64.add
                                    (f64.mul
                                      (local.get 4)
                                      (f64.const 0x1.3b8c5b12e9282p-4 (;=0.07703815055590194;)))
                                    (f64.const -0x1.6066c1b8d0159p-1 (;=-0.6882839716054533;))))
                                (f64.const 0x1.02ae59c598ac8p+1 (;=2.0209457602335057;))))
                            (f64.const -0x1.33a271c8a2d4bp+1 (;=-2.403394911734414;))))
                        (f64.const 0x1p+0 (;=1;))))))
                (local.get 0))
              (f64.const 0x1.921fb54442d18p+0 (;=1.5707963267948966;)))))
        (br_if 1 (;@1;)
          (i32.eqz
            (i32.or
              (i32.add
                (local.get 2)
                (i32.const -1072693248))
              (i32.wrap_i64
                (local.get 1)))))
        (local.set 4
          (f64.div
            (f64.const 0x0p+0 (;=0;))
            (f64.sub
              (local.get 0)
              (local.get 0)))))
      (return
        (local.get 4)))
    (select
      (f64.const 0x0p+0 (;=0;))
      (f64.const 0x1.921fb54442d18p+1 (;=3.141592653589793;))
      (i64.gt_s
        (local.get 1)
        (i64.const -1)))
  )
  (func $_ZN4libm4math4sqrt4sqrt17h3b6b03f022c75fd4E (;1;) (type 0) (param f64) (result f64)
    (local i32 i64 i32 f64 i64 i32 i32 i32 i32 i64 i64)
    (global.set $__stack_pointer
      (local.tee 1
        (i32.sub
          (global.get $__stack_pointer)
          (i32.const 48))))
    (block ;; label = @1
      (block ;; label = @2
        (block ;; label = @3
          (br_if 0 (;@3;)
            (i32.gt_u
              (i32.add
                (local.tee 3
                  (i32.wrap_i64
                    (i64.shr_u
                      (local.tee 2
                        (i64.reinterpret_f64
                          (local.get 0)))
                      (i64.const 52))))
                (i32.const -2047))
              (i32.const -2047)))
          (br_if 1 (;@2;)
            (i64.eq
              (local.get 2)
              (i64.const -9223372036854775808)))
          (br_if 1 (;@2;)
            (i64.eqz
              (local.get 2)))
          (br_if 1 (;@2;)
            (i64.eq
              (local.get 2)
              (i64.const 9218868437227405312)))
          (local.set 4
            (f64.const nan (;=NaN;)))
          (br_if 2 (;@1;)
            (i64.gt_u
              (local.get 2)
              (i64.const 9218868437227405312)))
          (local.set 3
            (i32.add
              (i32.and
                (i32.wrap_i64
                  (i64.shr_u
                    (local.tee 2
                      (i64.reinterpret_f64
                        (f64.mul
                          (local.get 0)
                          (f64.const 0x1p+52 (;=4503599627370496;)))))
                    (i64.const 52)))
                (i32.const 2047))
              (i32.const -52))))
        (local.set 6
          (i32.wrap_i64
            (i64.shr_u
              (local.tee 5
                (i64.shr_u
                  (i64.or
                    (i64.shl
                      (local.get 2)
                      (i64.const 11))
                    (i64.const -9223372036854775808))
                  (i64.extend_i32_u
                    (i32.and
                      (local.get 3)
                      (i32.const 1)))))
              (i64.const 32))))
        (local.set 8
          (local.tee 7
            (i32.shl
              (i32.load16_u offset=1053232
                (i32.shl
                  (i32.and
                    (i32.wrap_i64
                      (i64.shr_u
                        (local.get 2)
                        (i64.const 46)))
                    (i32.const 127))
                  (i32.const 1)))
              (i32.const 16))))
        (local.set 9
          (i32.const 0))
        (loop ;; label = @3
          (local.set 7
            (i32.and
              (i32.wrap_i64
                (local.tee 2
                  (i64.shr_u
                    (i64.mul
                      (i64.extend_i32_u
                        (local.tee 8
                          (i32.sub
                            (i32.const -1073741824)
                            (i32.wrap_i64
                              (i64.shr_u
                                (i64.mul
                                  (local.tee 2
                                    (i64.extend_i32_u
                                      (local.get 7)))
                                  (i64.extend_i32_u
                                    (local.tee 6
                                      (i32.shl
                                        (i32.wrap_i64
                                          (i64.shr_u
                                            (i64.mul
                                              (i64.extend_i32_u
                                                (local.get 8))
                                              (i64.extend_i32_u
                                                (local.get 6)))
                                            (i64.const 32)))
                                        (i32.ne
                                          (local.get 9)
                                          (i32.const 0))))))
                                (i64.const 32))))))
                      (local.get 2))
                    (i64.const 31))))
              (i32.const -2)))
          (br_if 0 (;@3;)
            (i32.ne
              (local.tee 9
                (i32.add
                  (local.get 9)
                  (i32.const 1)))
              (i32.const 2))))
        (local.set 10
          (local.tee 2
            (i64.and
              (i64.shl
                (local.get 2)
                (i64.const 32))
              (i64.const -8589934592))))
        (local.set 11
          (local.get 5))
        (local.set 9
          (i32.const 1))
        (loop ;; label = @3
          (call $__multi3
            (i32.add
              (local.get 1)
              (i32.const 32))
            (local.get 10)
            (i64.const 0)
            (local.get 11)
            (i64.const 0))
          (call $__multi3
            (i32.add
              (local.get 1)
              (i32.const 16))
            (local.tee 11
              (i64.load offset=40
                (local.get 1)))
            (i64.const 0)
            (local.get 2)
            (i64.const 0))
          (call $__multi3
            (local.get 1)
            (local.tee 10
              (i64.sub
                (i64.const -4611686018427387904)
                (i64.load offset=24
                  (local.get 1))))
            (i64.const 0)
            (local.get 2)
            (i64.const 0))
          (local.set 2
            (i64.shl
              (i64.load offset=8
                (local.get 1))
              (i64.const 1)))
          (local.set 6
            (i32.and
              (local.get 9)
              (i32.const 1)))
          (local.set 9
            (i32.const 0))
          (br_if 0 (;@3;)
            (local.get 6)))
        (local.set 4
          (f64.add
            (f64.reinterpret_i64
              (local.tee 2
                (i64.or
                  (i64.and
                    (i64.add
                      (i64.shr_u
                        (local.tee 11
                          (i64.add
                            (i64.sub
                              (i64.mul
                                (local.tee 2
                                  (i64.shr_u
                                    (local.get 11)
                                    (i64.const 9)))
                                (local.get 2))
                              (i64.shl
                                (local.get 5)
                                (i64.const 42)))
                            (local.get 2)))
                        (i64.const 63))
                      (local.get 2))
                    (i64.const 4503599627370495))
                  (i64.shl
                    (i64.extend_i32_u
                      (i32.shr_u
                        (i32.add
                          (local.get 3)
                          (i32.const 1023))
                        (i32.const 1)))
                    (i64.const 52)))))
            (f64.reinterpret_i64
              (i64.or
                (select
                  (i64.const 0)
                  (i64.const 4503599627370496)
                  (i64.eqz
                    (local.tee 2
                      (i64.add
                        (i64.add
                          (local.get 2)
                          (local.get 11))
                        (i64.const 1)))))
                (i64.and
                  (i64.xor
                    (local.get 2)
                    (local.get 11))
                  (i64.const -9223372036854775808))))))
        (br 1 (;@1;)))
      (local.set 4
        (local.get 0)))
    (global.set $__stack_pointer
      (i32.add
        (local.get 1)
        (i32.const 48)))
    (local.get 4)
  )
  (func $libm_acosf (;2;) (type 1) (param f32) (result f32)
    (local i32 i32 f32 f32)
    (block ;; label = @1
      (block ;; label = @2
        (block ;; label = @3
          (br_if 0 (;@3;)
            (i32.gt_u
              (local.tee 2
                (i32.and
                  (local.tee 1
                    (i32.reinterpret_f32
                      (local.get 0)))
                  (i32.const 2147483647)))
              (i32.const 1065353215)))
          (block ;; label = @4
            (br_if 0 (;@4;)
              (i32.lt_u
                (local.get 2)
                (i32.const 1056964608)))
            (block ;; label = @5
              (br_if 0 (;@5;)
                (i32.le_s
                  (local.get 1)
                  (i32.const -1)))
              (return
                (f32.add
                  (local.tee 0
                    (f32.add
                      (f32.add
                        (f32.mul
                          (f32.div
                            (f32.mul
                              (local.tee 0
                                (f32.mul
                                  (f32.sub
                                    (f32.const 0x1p+0 (;=1;))
                                    (local.get 0))
                                  (f32.const 0x1p-1 (;=0.5;))))
                              (f32.add
                                (f32.mul
                                  (local.get 0)
                                  (f32.add
                                    (f32.mul
                                      (local.get 0)
                                      (f32.const -0x1.1ba6d6p-7 (;=-0.008656363;)))
                                    (f32.const -0x1.5e2774p-5 (;=-0.042743422;))))
                                (f32.const 0x1.5554eap-3 (;=0.16666587;))))
                            (f32.add
                              (f32.mul
                                (local.get 0)
                                (f32.const -0x1.69cb5cp-1 (;=-0.70662963;)))
                              (f32.const 0x1p+0 (;=1;))))
                          (local.tee 3
                            (call $_ZN4libm4math4sqrt5sqrtf17h8c1b66187740c44bE
                              (local.get 0))))
                        (f32.div
                          (f32.sub
                            (local.get 0)
                            (f32.mul
                              (local.tee 4
                                (f32.reinterpret_i32
                                  (i32.and
                                    (i32.reinterpret_f32
                                      (local.get 3))
                                    (i32.const -4096))))
                              (local.get 4)))
                          (f32.add
                            (local.get 3)
                            (local.get 4))))
                      (local.get 4)))
                  (local.get 0))))
            (return
              (f32.add
                (local.tee 0
                  (f32.sub
                    (f32.const 0x1.921fb4p+0 (;=1.5707963;))
                    (f32.add
                      (local.tee 4
                        (call $_ZN4libm4math4sqrt5sqrtf17h8c1b66187740c44bE
                          (local.tee 0
                            (f32.mul
                              (f32.add
                                (local.get 0)
                                (f32.const 0x1p+0 (;=1;)))
                              (f32.const 0x1p-1 (;=0.5;))))))
                      (f32.add
                        (f32.mul
                          (local.get 4)
                          (f32.div
                            (f32.mul
                              (local.get 0)
                              (f32.add
                                (f32.mul
                                  (local.get 0)
                                  (f32.add
                                    (f32.mul
                                      (local.get 0)
                                      (f32.const -0x1.1ba6d6p-7 (;=-0.008656363;)))
                                    (f32.const -0x1.5e2774p-5 (;=-0.042743422;))))
                                (f32.const 0x1.5554eap-3 (;=0.16666587;))))
                            (f32.add
                              (f32.mul
                                (local.get 0)
                                (f32.const -0x1.69cb5cp-1 (;=-0.70662963;)))
                              (f32.const 0x1p+0 (;=1;)))))
                        (f32.const -0x1.4442dp-24 (;=-0.000000075497894;))))))
                (local.get 0))))
          (local.set 4
            (f32.const 0x1.921fb4p+0 (;=1.5707963;)))
          (br_if 1 (;@2;)
            (i32.lt_u
              (local.get 2)
              (i32.const 847249409)))
          (return
            (f32.add
              (f32.sub
                (f32.sub
                  (f32.const 0x1.4442dp-24 (;=0.000000075497894;))
                  (f32.mul
                    (local.get 0)
                    (f32.div
                      (f32.mul
                        (local.tee 4
                          (f32.mul
                            (local.get 0)
                            (local.get 0)))
                        (f32.add
                          (f32.mul
                            (local.get 4)
                            (f32.add
                              (f32.mul
                                (local.get 4)
                                (f32.const -0x1.1ba6d6p-7 (;=-0.008656363;)))
                              (f32.const -0x1.5e2774p-5 (;=-0.042743422;))))
                          (f32.const 0x1.5554eap-3 (;=0.16666587;))))
                      (f32.add
                        (f32.mul
                          (local.get 4)
                          (f32.const -0x1.69cb5cp-1 (;=-0.70662963;)))
                        (f32.const 0x1p+0 (;=1;))))))
                (local.get 0))
              (f32.const 0x1.921fb4p+0 (;=1.5707963;)))))
        (br_if 1 (;@1;)
          (i32.eq
            (local.get 2)
            (i32.const 1065353216)))
        (local.set 4
          (f32.div
            (f32.const 0x0p+0 (;=0;))
            (f32.sub
              (local.get 0)
              (local.get 0)))))
      (return
        (local.get 4)))
    (select
      (f32.const 0x0p+0 (;=0;))
      (f32.const 0x1.921fb4p+1 (;=3.1415925;))
      (i32.gt_s
        (local.get 1)
        (i32.const -1)))
  )
  (func $_ZN4libm4math4sqrt5sqrtf17h8c1b66187740c44bE (;3;) (type 1) (param f32) (result f32)
    (local i32 f32 i32 i32 i32 i32 i32 i64)
    (block ;; label = @1
      (block ;; label = @2
        (block ;; label = @3
          (br_if 0 (;@3;)
            (i32.gt_u
              (i32.add
                (local.tee 1
                  (i32.reinterpret_f32
                    (local.get 0)))
                (i32.const -2139095040))
              (i32.const -2130706433)))
          (br_if 2 (;@1;)
            (i32.eq
              (local.get 1)
              (i32.const -2147483648)))
          (br_if 2 (;@1;)
            (i32.eqz
              (local.get 1)))
          (br_if 2 (;@1;)
            (i32.eq
              (local.get 1)
              (i32.const 2139095040)))
          (local.set 2
            (f32.const nan (;=NaN;)))
          (br_if 1 (;@2;)
            (i32.gt_u
              (local.get 1)
              (i32.const 2139095040)))
          (local.set 1
            (i32.add
              (i32.reinterpret_f32
                (f32.mul
                  (local.get 0)
                  (f32.const 0x1p+23 (;=8388608;))))
              (i32.const -192937984))))
        (local.set 3
          (i32.const -1))
        (local.set 5
          (local.tee 4
            (i32.shl
              (i32.load16_u offset=1053232
                (i32.and
                  (i32.shr_u
                    (local.get 1)
                    (i32.const 16))
                  (i32.const 254)))
              (i32.const 16))))
        (local.set 7
          (local.tee 6
            (select
              (i32.and
                (i32.shl
                  (local.get 1)
                  (i32.const 7))
                (i32.const 2147483520))
              (i32.or
                (i32.shl
                  (local.get 1)
                  (i32.const 8))
                (i32.const -2147483648))
              (i32.and
                (local.get 1)
                (i32.const 8388608)))))
        (loop ;; label = @3
          (local.set 4
            (i32.and
              (i32.wrap_i64
                (i64.shr_u
                  (i64.mul
                    (i64.extend_i32_u
                      (local.tee 5
                        (i32.sub
                          (i32.const -1073741824)
                          (i32.wrap_i64
                            (i64.shr_u
                              (i64.mul
                                (local.tee 8
                                  (i64.extend_i32_u
                                    (local.get 4)))
                                (i64.extend_i32_u
                                  (local.tee 7
                                    (i32.shl
                                      (i32.wrap_i64
                                        (i64.shr_u
                                          (i64.mul
                                            (i64.extend_i32_u
                                              (local.get 5))
                                            (i64.extend_i32_u
                                              (local.get 7)))
                                          (i64.const 32)))
                                      (i32.eqz
                                        (local.get 3))))))
                              (i64.const 32))))))
                    (local.get 8))
                  (i64.const 31)))
              (i32.const -2)))
          (br_if 0 (;@3;)
            (i32.ne
              (local.tee 3
                (i32.add
                  (local.get 3)
                  (i32.const 1)))
              (i32.const 2))))
        (local.set 2
          (f32.add
            (f32.reinterpret_i32
              (local.tee 3
                (i32.or
                  (i32.and
                    (i32.add
                      (i32.shr_u
                        (local.tee 7
                          (i32.add
                            (i32.sub
                              (i32.mul
                                (local.tee 3
                                  (i32.shr_u
                                    (local.get 7)
                                    (i32.const 6)))
                                (local.get 3))
                              (i32.shl
                                (local.get 6)
                                (i32.const 16)))
                            (local.get 3)))
                        (i32.const 31))
                      (local.get 3))
                    (i32.const 8388607))
                  (i32.and
                    (i32.add
                      (i32.shr_u
                        (local.get 1)
                        (i32.const 1))
                      (i32.const 532676608))
                    (i32.const 2139095040)))))
            (f32.reinterpret_i32
              (i32.or
                (select
                  (i32.const 8388608)
                  (i32.const 0)
                  (local.tee 3
                    (i32.add
                      (i32.add
                        (local.get 3)
                        (local.get 7))
                      (i32.const 1))))
                (i32.and
                  (i32.xor
                    (local.get 3)
                    (local.get 7))
                  (i32.const -2147483648)))))))
      (return
        (local.get 2)))
    (local.get 0)
  )
  (func $libm_acosh (;4;) (type 0) (param f64) (result f64)
    (local i32)
    (block ;; label = @1
      (br_if 0 (;@1;)
        (i32.lt_u
          (local.tee 1
            (i32.and
              (i32.wrap_i64
                (i64.shr_u
                  (i64.reinterpret_f64
                    (local.get 0))
                  (i64.const 52)))
              (i32.const 2047)))
          (i32.const 1024)))
      (block ;; label = @2
        (br_if 0 (;@2;)
          (i32.lt_u
            (local.get 1)
            (i32.const 1049)))
        (return
          (f64.add
            (call $_ZN4libm4math3log3log17h781c40c93ff4fcfcE
              (local.get 0))
            (f64.const 0x1.62e42fefa39efp-1 (;=0.6931471805599453;)))))
      (return
        (call $_ZN4libm4math3log3log17h781c40c93ff4fcfcE
          (f64.add
            (f64.add
              (local.get 0)
              (local.get 0))
            (f64.div
              (f64.const -0x1p+0 (;=-1;))
              (f64.add
                (local.get 0)
                (call $_ZN4libm4math4sqrt4sqrt17h3b6b03f022c75fd4E
                  (f64.add
                    (f64.mul
                      (local.get 0)
                      (local.get 0))
                    (f64.const -0x1p+0 (;=-1;))))))))))
    (local.set 0
      (f64.add
        (local.get 0)
        (f64.const -0x1p+0 (;=-1;))))
    (call $_ZN4libm4math5log1p5log1p17h5d4b372f78bb46e9E
      (f64.add
        (local.get 0)
        (call $_ZN4libm4math4sqrt4sqrt17h3b6b03f022c75fd4E
          (f64.add
            (f64.mul
              (local.get 0)
              (local.get 0))
            (f64.add
              (local.get 0)
              (local.get 0))))))
  )
  (func $_ZN4libm4math3log3log17h781c40c93ff4fcfcE (;5;) (type 0) (param f64) (result f64)
    (local i64 i32 i64 i32 f64 f64)
    (block ;; label = @1
      (block ;; label = @2
        (block ;; label = @3
          (block ;; label = @4
            (br_if 0 (;@4;)
              (i64.lt_s
                (local.tee 1
                  (i64.reinterpret_f64
                    (local.get 0)))
                (i64.const 4503599627370496)))
            (br_if 3 (;@1;)
              (i64.gt_u
                (local.get 1)
                (i64.const 9218868437227405311)))
            (local.set 2
              (i32.const -1023))
            (block ;; label = @5
              (br_if 0 (;@5;)
                (i64.eq
                  (local.tee 3
                    (i64.shr_u
                      (local.get 1)
                      (i64.const 32)))
                  (i64.const 1072693248)))
              (local.set 4
                (i32.wrap_i64
                  (local.get 3)))
              (br 2 (;@3;)))
            (local.set 4
              (i32.const 1072693248))
            (br_if 1 (;@3;)
              (i32.wrap_i64
                (local.get 1)))
            (return
              (f64.const 0x0p+0 (;=0;))))
          (block ;; label = @4
            (br_if 0 (;@4;)
              (f64.ne
                (local.get 0)
                (f64.const 0x0p+0 (;=0;))))
            (return
              (f64.div
                (f64.const -0x1p+0 (;=-1;))
                (f64.mul
                  (local.get 0)
                  (local.get 0)))))
          (br_if 1 (;@2;)
            (i64.lt_s
              (local.get 1)
              (i64.const 0)))
          (local.set 4
            (i32.wrap_i64
              (i64.shr_u
                (local.tee 1
                  (i64.reinterpret_f64
                    (f64.mul
                      (local.get 0)
                      (f64.const 0x1p+54 (;=18014398509481984;)))))
                (i64.const 32))))
          (local.set 2
            (i32.const -1077)))
        (return
          (f64.add
            (f64.mul
              (local.tee 5
                (f64.convert_i32_s
                  (i32.add
                    (local.get 2)
                    (i32.shr_u
                      (local.tee 4
                        (i32.add
                          (local.get 4)
                          (i32.const 614242)))
                      (i32.const 20)))))
              (f64.const 0x1.62e42feep-1 (;=0.6931471803691238;)))
            (f64.add
              (local.tee 0
                (f64.add
                  (f64.reinterpret_i64
                    (i64.or
                      (i64.shl
                        (i64.extend_i32_u
                          (i32.add
                            (i32.and
                              (local.get 4)
                              (i32.const 1048575))
                            (i32.const 1072079006)))
                        (i64.const 32))
                      (i64.and
                        (local.get 1)
                        (i64.const 4294967295))))
                  (f64.const -0x1p+0 (;=-1;))))
              (f64.sub
                (f64.add
                  (f64.mul
                    (local.get 5)
                    (f64.const 0x1.a39ef35793c76p-33 (;=0.00000000019082149292705877;)))
                  (f64.mul
                    (local.tee 5
                      (f64.div
                        (local.get 0)
                        (f64.add
                          (local.get 0)
                          (f64.const 0x1p+1 (;=2;)))))
                    (f64.add
                      (local.tee 6
                        (f64.mul
                          (local.get 0)
                          (f64.mul
                            (local.get 0)
                            (f64.const 0x1p-1 (;=0.5;)))))
                      (f64.add
                        (f64.mul
                          (local.tee 0
                            (f64.mul
                              (local.tee 5
                                (f64.mul
                                  (local.get 5)
                                  (local.get 5)))
                              (local.get 5)))
                          (f64.add
                            (f64.mul
                              (local.get 0)
                              (f64.add
                                (f64.mul
                                  (local.get 0)
                                  (f64.const 0x1.39a09d078c69fp-3 (;=0.15313837699209373;)))
                                (f64.const 0x1.c71c51d8e78afp-3 (;=0.22222198432149784;))))
                            (f64.const 0x1.999999997fa04p-2 (;=0.3999999999940942;))))
                        (f64.mul
                          (local.get 5)
                          (f64.add
                            (f64.mul
                              (local.get 0)
                              (f64.add
                                (f64.mul
                                  (local.get 0)
                                  (f64.add
                                    (f64.mul
                                      (local.get 0)
                                      (f64.const 0x1.2f112df3e5244p-3 (;=0.14798198605116586;)))
                                    (f64.const 0x1.7466496cb03dep-3 (;=0.1818357216161805;))))
                                (f64.const 0x1.2492494229359p-2 (;=0.2857142874366239;))))
                            (f64.const 0x1.5555555555593p-1 (;=0.6666666666666735;))))))))
                (local.get 6))))))
      (local.set 0
        (f64.div
          (f64.sub
            (local.get 0)
            (local.get 0))
          (f64.const 0x0p+0 (;=0;)))))
    (local.get 0)
  )
  (func $_ZN4libm4math5log1p5log1p17h5d4b372f78bb46e9E (;6;) (type 0) (param f64) (result f64)
    (local i32 i64 i32 f64 f64 f64)
    (local.set 1
      (i32.sub
        (global.get $__stack_pointer)
        (i32.const 16)))
    (block ;; label = @1
      (block ;; label = @2
        (block ;; label = @3
          (block ;; label = @4
            (block ;; label = @5
              (block ;; label = @6
                (br_if 0 (;@6;)
                  (i64.gt_s
                    (local.tee 2
                      (i64.reinterpret_f64
                        (local.get 0)))
                    (i64.const 4601133429810003967)))
                (br_if 4 (;@2;)
                  (i64.gt_u
                    (local.get 2)
                    (i64.const -4616189618054758401)))
                (br_if 2 (;@4;)
                  (i32.ge_u
                    (i32.shl
                      (local.tee 3
                        (i32.wrap_i64
                          (i64.shr_u
                            (local.get 2)
                            (i64.const 32))))
                      (i32.const 1))
                    (i32.const 2034237440)))
                (br_if 1 (;@5;)
                  (i32.and
                    (local.get 3)
                    (i32.const 2146435072)))
                (f32.store offset=12
                  (local.get 1)
                  (f32.demote_f64
                    (local.get 0)))
                (drop
                  (f32.load offset=12
                    (local.get 1)))
                (br 1 (;@5;)))
              (br_if 2 (;@3;)
                (i64.le_u
                  (local.get 2)
                  (i64.const 9218868437227405311))))
            (return
              (local.get 0)))
          (local.set 4
            (f64.const 0x0p+0 (;=0;)))
          (br_if 0 (;@3;)
            (i64.gt_u
              (local.get 2)
              (i64.const -4624424114038243329)))
          (local.set 5
            (f64.const 0x0p+0 (;=0;)))
          (br 2 (;@1;)))
        (local.set 3
          (i32.add
            (i32.shr_u
              (local.tee 1
                (i32.add
                  (i32.wrap_i64
                    (i64.shr_u
                      (local.tee 2
                        (i64.reinterpret_f64
                          (local.tee 5
                            (f64.add
                              (local.get 0)
                              (f64.const 0x1p+0 (;=1;))))))
                      (i64.const 32)))
                  (i32.const 614242)))
              (i32.const 20))
            (i32.const -1023)))
        (local.set 4
          (f64.const 0x0p+0 (;=0;)))
        (block ;; label = @3
          (br_if 0 (;@3;)
            (i32.ge_u
              (local.get 1)
              (i32.const 1129316352)))
          (local.set 4
            (f64.div
              (select
                (f64.add
                  (f64.sub
                    (local.get 0)
                    (local.get 5))
                  (f64.const 0x1p+0 (;=1;)))
                (f64.sub
                  (local.get 0)
                  (f64.add
                    (local.get 5)
                    (f64.const -0x1p+0 (;=-1;))))
                (i32.gt_u
                  (local.get 1)
                  (i32.const 1074790399)))
              (local.get 5))))
        (local.set 0
          (f64.add
            (f64.reinterpret_i64
              (i64.or
                (i64.shl
                  (i64.extend_i32_u
                    (i32.add
                      (i32.and
                        (local.get 1)
                        (i32.const 1048575))
                      (i32.const 1072079006)))
                  (i64.const 32))
                (i64.and
                  (local.get 2)
                  (i64.const 4294967295))))
            (f64.const -0x1p+0 (;=-1;))))
        (local.set 5
          (f64.convert_i32_s
            (local.get 3)))
        (br 1 (;@1;)))
      (local.set 5
        (f64.const -inf (;=-inf;)))
      (block ;; label = @2
        (br_if 0 (;@2;)
          (f64.eq
            (local.get 0)
            (f64.const -0x1p+0 (;=-1;))))
        (local.set 5
          (f64.div
            (f64.sub
              (local.get 0)
              (local.get 0))
            (f64.const 0x0p+0 (;=0;)))))
      (return
        (local.get 5)))
    (f64.add
      (f64.mul
        (local.get 5)
        (f64.const 0x1.62e42feep-1 (;=0.6931471803691238;)))
      (f64.add
        (local.get 0)
        (f64.sub
          (f64.add
            (f64.add
              (local.get 4)
              (f64.mul
                (local.get 5)
                (f64.const 0x1.a39ef35793c76p-33 (;=0.00000000019082149292705877;))))
            (f64.mul
              (local.tee 5
                (f64.div
                  (local.get 0)
                  (f64.add
                    (local.get 0)
                    (f64.const 0x1p+1 (;=2;)))))
              (f64.add
                (local.tee 6
                  (f64.mul
                    (local.get 0)
                    (f64.mul
                      (local.get 0)
                      (f64.const 0x1p-1 (;=0.5;)))))
                (f64.add
                  (f64.mul
                    (local.tee 5
                      (f64.mul
                        (local.tee 4
                          (f64.mul
                            (local.get 5)
                            (local.get 5)))
                        (local.get 4)))
                    (f64.add
                      (f64.mul
                        (local.get 5)
                        (f64.add
                          (f64.mul
                            (local.get 5)
                            (f64.const 0x1.39a09d078c69fp-3 (;=0.15313837699209373;)))
                          (f64.const 0x1.c71c51d8e78afp-3 (;=0.22222198432149784;))))
                      (f64.const 0x1.999999997fa04p-2 (;=0.3999999999940942;))))
                  (f64.mul
                    (local.get 4)
                    (f64.add
                      (f64.mul
                        (local.get 5)
                        (f64.add
                          (f64.mul
                            (local.get 5)
                            (f64.add
                              (f64.mul
                                (local.get 5)
                                (f64.const 0x1.2f112df3e5244p-3 (;=0.14798198605116586;)))
                              (f64.const 0x1.7466496cb03dep-3 (;=0.1818357216161805;))))
                          (f64.const 0x1.2492494229359p-2 (;=0.2857142874366239;))))
                      (f64.const 0x1.5555555555593p-1 (;=0.6666666666666735;))))))))
          (local.get 6))))
  )
  (func $libm_acoshf (;7;) (type 1) (param f32) (result f32)
    (local i32)
    (block ;; label = @1
      (br_if 0 (;@1;)
        (i32.lt_u
          (local.tee 1
            (i32.and
              (i32.reinterpret_f32
                (local.get 0))
              (i32.const 2147483647)))
          (i32.const 1073741824)))
      (block ;; label = @2
        (br_if 0 (;@2;)
          (i32.lt_u
            (local.get 1)
            (i32.const 1166016512)))
        (return
          (f32.add
            (call $_ZN4libm4math4logf4logf17h7b88872ed73a994aE
              (local.get 0))
            (f32.const 0x1.62e43p-1 (;=0.6931472;)))))
      (return
        (call $_ZN4libm4math4logf4logf17h7b88872ed73a994aE
          (f32.add
            (f32.add
              (local.get 0)
              (local.get 0))
            (f32.div
              (f32.const -0x1p+0 (;=-1;))
              (f32.add
                (local.get 0)
                (call $_ZN4libm4math4sqrt5sqrtf17h8c1b66187740c44bE
                  (f32.add
                    (f32.mul
                      (local.get 0)
                      (local.get 0))
                    (f32.const -0x1p+0 (;=-1;))))))))))
    (local.set 0
      (f32.add
        (local.get 0)
        (f32.const -0x1p+0 (;=-1;))))
    (call $_ZN4libm4math6log1pf6log1pf17h4c967d18f426e9e4E
      (f32.add
        (local.get 0)
        (call $_ZN4libm4math4sqrt5sqrtf17h8c1b66187740c44bE
          (f32.add
            (f32.mul
              (local.get 0)
              (local.get 0))
            (f32.add
              (local.get 0)
              (local.get 0))))))
  )
  (func $_ZN4libm4math4logf4logf17h7b88872ed73a994aE (;8;) (type 1) (param f32) (result f32)
    (local i32 i32 f32 f32)
    (block ;; label = @1
      (block ;; label = @2
        (block ;; label = @3
          (br_if 0 (;@3;)
            (i32.lt_s
              (local.tee 1
                (i32.reinterpret_f32
                  (local.get 0)))
              (i32.const 8388608)))
          (br_if 1 (;@2;)
            (i32.gt_u
              (local.get 1)
              (i32.const 2139095039)))
          (local.set 2
            (i32.const -127))
          (local.set 0
            (f32.const 0x0p+0 (;=0;)))
          (br_if 1 (;@2;)
            (i32.eq
              (local.get 1)
              (i32.const 1065353216)))
          (br 2 (;@1;)))
        (block ;; label = @3
          (br_if 0 (;@3;)
            (f32.ne
              (local.get 0)
              (f32.const 0x0p+0 (;=0;))))
          (return
            (f32.div
              (f32.const -0x1p+0 (;=-1;))
              (f32.mul
                (local.get 0)
                (local.get 0)))))
        (block ;; label = @3
          (br_if 0 (;@3;)
            (i32.lt_s
              (local.get 1)
              (i32.const 0)))
          (local.set 1
            (i32.reinterpret_f32
              (f32.mul
                (local.get 0)
                (f32.const 0x1p+25 (;=33554432;)))))
          (local.set 2
            (i32.const -152))
          (br 2 (;@1;)))
        (local.set 0
          (f32.div
            (f32.sub
              (local.get 0)
              (local.get 0))
            (f32.const 0x0p+0 (;=0;)))))
      (return
        (local.get 0)))
    (f32.add
      (f32.mul
        (local.tee 3
          (f32.convert_i32_s
            (i32.add
              (local.get 2)
              (i32.shr_u
                (local.tee 1
                  (i32.add
                    (local.get 1)
                    (i32.const 4913933)))
                (i32.const 23)))))
        (f32.const 0x1.62e3p-1 (;=0.6931381;)))
      (f32.add
        (local.tee 0
          (f32.add
            (f32.reinterpret_i32
              (i32.add
                (i32.and
                  (local.get 1)
                  (i32.const 8388607))
                (i32.const 1060439283)))
            (f32.const -0x1p+0 (;=-1;))))
        (f32.sub
          (f32.add
            (f32.mul
              (local.get 3)
              (f32.const 0x1.2fefa2p-17 (;=0.000009058001;)))
            (f32.mul
              (local.tee 3
                (f32.div
                  (local.get 0)
                  (f32.add
                    (local.get 0)
                    (f32.const 0x1p+1 (;=2;)))))
              (f32.add
                (local.tee 4
                  (f32.mul
                    (local.get 0)
                    (f32.mul
                      (local.get 0)
                      (f32.const 0x1p-1 (;=0.5;)))))
                (f32.add
                  (f32.mul
                    (local.tee 0
                      (f32.mul
                        (local.get 3)
                        (local.get 3)))
                    (f32.add
                      (f32.mul
                        (local.tee 0
                          (f32.mul
                            (local.get 0)
                            (local.get 0)))
                        (f32.const 0x1.23d3dcp-2 (;=0.28498787;)))
                      (f32.const 0x1.555554p-1 (;=0.6666666;))))
                  (f32.mul
                    (local.get 0)
                    (f32.add
                      (f32.mul
                        (local.get 0)
                        (f32.const 0x1.f13c4cp-3 (;=0.24279079;)))
                      (f32.const 0x1.999c26p-2 (;=0.40000972;))))))))
          (local.get 4))))
  )
  (func $_ZN4libm4math6log1pf6log1pf17h4c967d18f426e9e4E (;9;) (type 1) (param f32) (result f32)
    (local i32 i32 f32 f32)
    (local.set 1
      (i32.sub
        (global.get $__stack_pointer)
        (i32.const 16)))
    (block ;; label = @1
      (block ;; label = @2
        (block ;; label = @3
          (block ;; label = @4
            (block ;; label = @5
              (block ;; label = @6
                (block ;; label = @7
                  (block ;; label = @8
                    (br_if 0 (;@8;)
                      (i32.gt_s
                        (local.tee 2
                          (i32.reinterpret_f32
                            (local.get 0)))
                        (i32.const 1054086095)))
                    (br_if 2 (;@6;)
                      (i32.gt_u
                        (local.get 2)
                        (i32.const -1082130433)))
                    (br_if 1 (;@7;)
                      (i32.ge_u
                        (i32.shl
                          (local.get 2)
                          (i32.const 1))
                        (i32.const 1728053248)))
                    (br_if 3 (;@5;)
                      (i32.eqz
                        (i32.and
                          (local.get 2)
                          (i32.const 2139095040))))
                    (br 7 (;@1;)))
                  (br_if 6 (;@1;)
                    (i32.gt_u
                      (local.get 2)
                      (i32.const 2139095039)))
                  (br 4 (;@3;)))
                (local.set 3
                  (f32.const 0x0p+0 (;=0;)))
                (br_if 3 (;@3;)
                  (i32.gt_u
                    (local.get 2)
                    (i32.const -1097468391)))
                (local.set 4
                  (f32.const 0x0p+0 (;=0;)))
                (br 4 (;@2;)))
              (br_if 1 (;@4;)
                (f32.ne
                  (local.get 0)
                  (f32.const -0x1p+0 (;=-1;))))
              (return
                (f32.const -inf (;=-inf;))))
            (f32.store offset=12
              (local.get 1)
              (f32.mul
                (local.get 0)
                (local.get 0)))
            (drop
              (f32.load offset=12
                (local.get 1)))
            (br 3 (;@1;)))
          (return
            (f32.div
              (f32.sub
                (local.get 0)
                (local.get 0))
              (f32.const 0x0p+0 (;=0;)))))
        (local.set 1
          (i32.add
            (i32.shr_u
              (local.tee 2
                (i32.add
                  (i32.reinterpret_f32
                    (local.tee 4
                      (f32.add
                        (local.get 0)
                        (f32.const 0x1p+0 (;=1;)))))
                  (i32.const 4913933)))
              (i32.const 23))
            (i32.const -127)))
        (local.set 3
          (f32.const 0x0p+0 (;=0;)))
        (block ;; label = @3
          (br_if 0 (;@3;)
            (i32.ge_u
              (local.get 2)
              (i32.const 1275068416)))
          (local.set 3
            (f32.div
              (select
                (f32.add
                  (f32.sub
                    (local.get 0)
                    (local.get 4))
                  (f32.const 0x1p+0 (;=1;)))
                (f32.sub
                  (local.get 0)
                  (f32.add
                    (local.get 4)
                    (f32.const -0x1p+0 (;=-1;))))
                (i32.gt_u
                  (local.get 2)
                  (i32.const 1082130431)))
              (local.get 4))))
        (local.set 0
          (f32.add
            (f32.reinterpret_i32
              (i32.add
                (i32.and
                  (local.get 2)
                  (i32.const 8388607))
                (i32.const 1060439283)))
            (f32.const -0x1p+0 (;=-1;))))
        (local.set 4
          (f32.convert_i32_s
            (local.get 1))))
      (return
        (f32.add
          (f32.mul
            (local.get 4)
            (f32.const 0x1.62e3p-1 (;=0.6931381;)))
          (f32.add
            (local.get 0)
            (f32.sub
              (f32.add
                (f32.add
                  (local.get 3)
                  (f32.mul
                    (local.get 4)
                    (f32.const 0x1.2fefa2p-17 (;=0.000009058001;))))
                (f32.mul
                  (local.tee 4
                    (f32.div
                      (local.get 0)
                      (f32.add
                        (local.get 0)
                        (f32.const 0x1p+1 (;=2;)))))
                  (f32.add
                    (local.tee 3
                      (f32.mul
                        (local.get 0)
                        (f32.mul
                          (local.get 0)
                          (f32.const 0x1p-1 (;=0.5;)))))
                    (f32.add
                      (f32.mul
                        (local.tee 4
                          (f32.mul
                            (local.get 4)
                            (local.get 4)))
                        (f32.add
                          (f32.mul
                            (local.tee 4
                              (f32.mul
                                (local.get 4)
                                (local.get 4)))
                            (f32.const 0x1.23d3dcp-2 (;=0.28498787;)))
                          (f32.const 0x1.555554p-1 (;=0.6666666;))))
                      (f32.mul
                        (local.get 4)
                        (f32.add
                          (f32.mul
                            (local.get 4)
                            (f32.const 0x1.f13c4cp-3 (;=0.24279079;)))
                          (f32.const 0x1.999c26p-2 (;=0.40000972;))))))))
              (local.get 3))))))
    (local.get 0)
  )
  (func $libm_asin (;10;) (type 0) (param f64) (result f64)
    (local i64 i32 f64 f64 f64)
    (block ;; label = @1
      (block ;; label = @2
        (block ;; label = @3
          (br_if 0 (;@3;)
            (i32.gt_u
              (local.tee 2
                (i32.and
                  (i32.wrap_i64
                    (i64.shr_u
                      (local.tee 1
                        (i64.reinterpret_f64
                          (local.get 0)))
                      (i64.const 32)))
                  (i32.const 2147483647)))
              (i32.const 1072693247)))
          (block ;; label = @4
            (block ;; label = @5
              (block ;; label = @6
                (br_if 0 (;@6;)
                  (i32.lt_u
                    (local.get 2)
                    (i32.const 1071644672)))
                (local.set 3
                  (f64.div
                    (f64.mul
                      (local.tee 0
                        (f64.mul
                          (f64.sub
                            (f64.const 0x1p+0 (;=1;))
                            (f64.abs
                              (local.get 0)))
                          (f64.const 0x1p-1 (;=0.5;))))
                      (f64.add
                        (f64.mul
                          (local.get 0)
                          (f64.add
                            (f64.mul
                              (local.get 0)
                              (f64.add
                                (f64.mul
                                  (local.get 0)
                                  (f64.add
                                    (f64.mul
                                      (local.get 0)
                                      (f64.add
                                        (f64.mul
                                          (local.get 0)
                                          (f64.const 0x1.23de10dfdf709p-15 (;=0.00003479331075960212;)))
                                        (f64.const 0x1.9efe07501b288p-11 (;=0.0007915349942898145;))))
                                    (f64.const -0x1.48228b5688f3bp-5 (;=-0.04005553450067941;))))
                                (f64.const 0x1.9c1550e884455p-3 (;=0.20121253213486293;))))
                            (f64.const -0x1.4d61203eb6f7dp-2 (;=-0.3255658186224009;))))
                        (f64.const 0x1.5555555555555p-3 (;=0.16666666666666666;))))
                    (f64.add
                      (f64.mul
                        (local.get 0)
                        (f64.add
                          (f64.mul
                            (local.get 0)
                            (f64.add
                              (f64.mul
                                (local.get 0)
                                (f64.add
                                  (f64.mul
                                    (local.get 0)
                                    (f64.const 0x1.3b8c5b12e9282p-4 (;=0.07703815055590194;)))
                                  (f64.const -0x1.6066c1b8d0159p-1 (;=-0.6882839716054533;))))
                              (f64.const 0x1.02ae59c598ac8p+1 (;=2.0209457602335057;))))
                          (f64.const -0x1.33a271c8a2d4bp+1 (;=-2.403394911734414;))))
                      (f64.const 0x1p+0 (;=1;)))))
                (local.set 4
                  (call $_ZN4libm4math4sqrt4sqrt17h3b6b03f022c75fd4E
                    (local.get 0)))
                (br_if 1 (;@5;)
                  (i32.gt_u
                    (local.get 2)
                    (i32.const 1072640818)))
                (local.set 0
                  (f64.add
                    (f64.add
                      (f64.sub
                        (f64.const 0x1.921fb54442d18p-1 (;=0.7853981633974483;))
                        (f64.add
                          (local.tee 5
                            (f64.reinterpret_i64
                              (i64.and
                                (i64.reinterpret_f64
                                  (local.get 4))
                                (i64.const -4294967296))))
                          (local.get 5)))
                      (f64.sub
                        (f64.sub
                          (f64.const 0x1.1a62633145c07p-54 (;=0.00000000000000006123233995736766;))
                          (f64.add
                            (local.tee 0
                              (f64.div
                                (f64.sub
                                  (local.get 0)
                                  (f64.mul
                                    (local.get 5)
                                    (local.get 5)))
                                (f64.add
                                  (local.get 4)
                                  (local.get 5))))
                            (local.get 0)))
                        (f64.mul
                          (f64.add
                            (local.get 4)
                            (local.get 4))
                          (local.get 3))))
                    (f64.const 0x1.921fb54442d18p-1 (;=0.7853981633974483;))))
                (br 2 (;@4;)))
              (br_if 3 (;@2;)
                (i32.lt_u
                  (i32.add
                    (local.get 2)
                    (i32.const -1048576))
                  (i32.const 1044381696)))
              (return
                (f64.add
                  (local.get 0)
                  (f64.mul
                    (local.get 0)
                    (f64.div
                      (f64.mul
                        (local.tee 4
                          (f64.mul
                            (local.get 0)
                            (local.get 0)))
                        (f64.add
                          (f64.mul
                            (local.get 4)
                            (f64.add
                              (f64.mul
                                (local.get 4)
                                (f64.add
                                  (f64.mul
                                    (local.get 4)
                                    (f64.add
                                      (f64.mul
                                        (local.get 4)
                                        (f64.add
                                          (f64.mul
                                            (local.get 4)
                                            (f64.const 0x1.23de10dfdf709p-15 (;=0.00003479331075960212;)))
                                          (f64.const 0x1.9efe07501b288p-11 (;=0.0007915349942898145;))))
                                      (f64.const -0x1.48228b5688f3bp-5 (;=-0.04005553450067941;))))
                                  (f64.const 0x1.9c1550e884455p-3 (;=0.20121253213486293;))))
                              (f64.const -0x1.4d61203eb6f7dp-2 (;=-0.3255658186224009;))))
                          (f64.const 0x1.5555555555555p-3 (;=0.16666666666666666;))))
                      (f64.add
                        (f64.mul
                          (local.get 4)
                          (f64.add
                            (f64.mul
                              (local.get 4)
                              (f64.add
                                (f64.mul
                                  (local.get 4)
                                  (f64.add
                                    (f64.mul
                                      (local.get 4)
                                      (f64.const 0x1.3b8c5b12e9282p-4 (;=0.07703815055590194;)))
                                    (f64.const -0x1.6066c1b8d0159p-1 (;=-0.6882839716054533;))))
                                (f64.const 0x1.02ae59c598ac8p+1 (;=2.0209457602335057;))))
                            (f64.const -0x1.33a271c8a2d4bp+1 (;=-2.403394911734414;))))
                        (f64.const 0x1p+0 (;=1;))))))))
            (local.set 0
              (f64.sub
                (f64.const 0x1.921fb54442d18p+0 (;=1.5707963267948966;))
                (f64.add
                  (f64.add
                    (local.tee 0
                      (f64.add
                        (local.get 4)
                        (f64.mul
                          (local.get 4)
                          (local.get 3))))
                    (local.get 0))
                  (f64.const -0x1.1a62633145c07p-54 (;=-0.00000000000000006123233995736766;))))))
          (return
            (select
              (f64.neg
                (local.get 0))
              (local.get 0)
              (i64.lt_s
                (local.get 1)
                (i64.const 0)))))
        (br_if 1 (;@1;)
          (i32.eqz
            (i32.or
              (i32.add
                (local.get 2)
                (i32.const -1072693248))
              (i32.wrap_i64
                (local.get 1)))))
        (local.set 0
          (f64.div
            (f64.const 0x0p+0 (;=0;))
            (f64.sub
              (local.get 0)
              (local.get 0)))))
      (return
        (local.get 0)))
    (f64.add
      (f64.mul
        (local.get 0)
        (f64.const 0x1.921fb54442d18p+0 (;=1.5707963267948966;)))
      (f64.const 0x1p-120 (;=0.000000000000000000000000000000000000752316384526264;)))
  )
  (func $libm_asinf (;11;) (type 1) (param f32) (result f32)
    (local f32 i32 f64)
    (block ;; label = @1
      (block ;; label = @2
        (block ;; label = @3
          (br_if 0 (;@3;)
            (i32.gt_u
              (local.tee 2
                (i32.reinterpret_f32
                  (local.tee 1
                    (f32.abs
                      (local.get 0)))))
              (i32.const 1065353215)))
          (block ;; label = @4
            (br_if 0 (;@4;)
              (i32.lt_u
                (local.get 2)
                (i32.const 1056964608)))
            (return
              (select
                (f32.neg
                  (local.tee 1
                    (f32.demote_f64
                      (f64.sub
                        (f64.const 0x1.921fb54442d18p+0 (;=1.5707963267948966;))
                        (f64.add
                          (local.tee 3
                            (f64.add
                              (local.tee 3
                                (call $_ZN4libm4math4sqrt4sqrt17h3b6b03f022c75fd4E
                                  (f64.promote_f32
                                    (local.tee 1
                                      (f32.mul
                                        (f32.sub
                                          (f32.const 0x1p+0 (;=1;))
                                          (local.get 1))
                                        (f32.const 0x1p-1 (;=0.5;)))))))
                              (f64.mul
                                (local.get 3)
                                (f64.promote_f32
                                  (f32.div
                                    (f32.mul
                                      (local.get 1)
                                      (f32.add
                                        (f32.mul
                                          (local.get 1)
                                          (f32.add
                                            (f32.mul
                                              (local.get 1)
                                              (f32.const -0x1.1ba6d6p-7 (;=-0.008656363;)))
                                            (f32.const -0x1.5e2774p-5 (;=-0.042743422;))))
                                        (f32.const 0x1.5554eap-3 (;=0.16666587;))))
                                    (f32.add
                                      (f32.mul
                                        (local.get 1)
                                        (f32.const -0x1.69cb5cp-1 (;=-0.70662963;)))
                                      (f32.const 0x1p+0 (;=1;))))))))
                          (local.get 3))))))
                (local.get 1)
                (i32.lt_s
                  (i32.reinterpret_f32
                    (local.get 0))
                  (i32.const 0)))))
          (br_if 1 (;@2;)
            (i32.lt_u
              (i32.add
                (local.get 2)
                (i32.const -8388608))
              (i32.const 956301312)))
          (return
            (f32.add
              (local.get 0)
              (f32.mul
                (local.get 0)
                (f32.div
                  (f32.mul
                    (local.tee 1
                      (f32.mul
                        (local.get 0)
                        (local.get 0)))
                    (f32.add
                      (f32.mul
                        (local.get 1)
                        (f32.add
                          (f32.mul
                            (local.get 1)
                            (f32.const -0x1.1ba6d6p-7 (;=-0.008656363;)))
                          (f32.const -0x1.5e2774p-5 (;=-0.042743422;))))
                      (f32.const 0x1.5554eap-3 (;=0.16666587;))))
                  (f32.add
                    (f32.mul
                      (local.get 1)
                      (f32.const -0x1.69cb5cp-1 (;=-0.70662963;)))
                    (f32.const 0x1p+0 (;=1;))))))))
        (br_if 1 (;@1;)
          (i32.eq
            (local.get 2)
            (i32.const 1065353216)))
        (local.set 0
          (f32.div
            (f32.const 0x0p+0 (;=0;))
            (f32.sub
              (local.get 0)
              (local.get 0)))))
      (return
        (local.get 0)))
    (f32.demote_f64
      (f64.add
        (f64.mul
          (f64.promote_f32
            (local.get 0))
          (f64.const 0x1.921fb54442d18p+0 (;=1.5707963267948966;)))
        (f64.const 0x1p-120 (;=0.000000000000000000000000000000000000752316384526264;))))
  )
  (func $libm_asinh (;12;) (type 0) (param f64) (result f64)
    (local i32 f64 i64 i32)
    (global.set $__stack_pointer
      (local.tee 1
        (i32.sub
          (global.get $__stack_pointer)
          (i32.const 16))))
    (local.set 2
      (f64.abs
        (local.get 0)))
    (block ;; label = @1
      (block ;; label = @2
        (block ;; label = @3
          (br_if 0 (;@3;)
            (i32.gt_u
              (local.tee 4
                (i32.and
                  (i32.wrap_i64
                    (i64.shr_u
                      (local.tee 3
                        (i64.reinterpret_f64
                          (local.get 0)))
                      (i64.const 52)))
                  (i32.const 2047)))
              (i32.const 1048)))
          (br_if 1 (;@2;)
            (i32.gt_u
              (local.get 4)
              (i32.const 1023)))
          (block ;; label = @4
            (br_if 0 (;@4;)
              (i32.gt_u
                (local.get 4)
                (i32.const 996)))
            (f64.store offset=8
              (local.get 1)
              (f64.add
                (local.get 2)
                (f64.const 0x1p+120 (;=1329227995784916000000000000000000000;))))
            (drop
              (f64.load offset=8
                (local.get 1)))
            (br 3 (;@1;)))
          (local.set 0
            (f64.mul
              (local.get 0)
              (local.get 0)))
          (local.set 2
            (call $_ZN4libm4math5log1p5log1p17h5d4b372f78bb46e9E
              (f64.add
                (local.get 2)
                (f64.div
                  (local.get 0)
                  (f64.add
                    (call $_ZN4libm4math4sqrt4sqrt17h3b6b03f022c75fd4E
                      (f64.add
                        (local.get 0)
                        (f64.const 0x1p+0 (;=1;))))
                    (f64.const 0x1p+0 (;=1;)))))))
          (br 2 (;@1;)))
        (local.set 2
          (f64.add
            (call $_ZN4libm4math3log3log17h781c40c93ff4fcfcE
              (local.get 2))
            (f64.const 0x1.62e42fefa39efp-1 (;=0.6931471805599453;))))
        (br 1 (;@1;)))
      (local.set 2
        (call $_ZN4libm4math3log3log17h781c40c93ff4fcfcE
          (f64.add
            (f64.add
              (local.get 2)
              (local.get 2))
            (f64.div
              (f64.const 0x1p+0 (;=1;))
              (f64.add
                (local.get 2)
                (call $_ZN4libm4math4sqrt4sqrt17h3b6b03f022c75fd4E
                  (f64.add
                    (f64.mul
                      (local.get 0)
                      (local.get 0))
                    (f64.const 0x1p+0 (;=1;))))))))))
    (global.set $__stack_pointer
      (i32.add
        (local.get 1)
        (i32.const 16)))
    (select
      (f64.neg
        (local.get 2))
      (local.get 2)
      (i64.lt_s
        (local.get 3)
        (i64.const 0)))
  )
  (func $libm_asinhf (;13;) (type 1) (param f32) (result f32)
    (local i32 f32 i32 f32)
    (global.set $__stack_pointer
      (local.tee 1
        (i32.sub
          (global.get $__stack_pointer)
          (i32.const 16))))
    (block ;; label = @1
      (block ;; label = @2
        (block ;; label = @3
          (br_if 0 (;@3;)
            (i32.gt_u
              (local.tee 3
                (i32.reinterpret_f32
                  (local.tee 2
                    (f32.abs
                      (local.get 0)))))
              (i32.const 1166016511)))
          (br_if 1 (;@2;)
            (i32.gt_u
              (local.get 3)
              (i32.const 1073741823)))
          (block ;; label = @4
            (br_if 0 (;@4;)
              (i32.gt_u
                (local.get 3)
                (i32.const 964689919)))
            (f32.store offset=12
              (local.get 1)
              (f32.add
                (local.get 2)
                (f32.const 0x1p+120 (;=1329228000000000000000000000000000000;))))
            (drop
              (f32.load offset=12
                (local.get 1)))
            (br 3 (;@1;)))
          (local.set 4
            (f32.mul
              (local.get 0)
              (local.get 0)))
          (local.set 2
            (call $_ZN4libm4math6log1pf6log1pf17h4c967d18f426e9e4E
              (f32.add
                (local.get 2)
                (f32.div
                  (local.get 4)
                  (f32.add
                    (call $_ZN4libm4math4sqrt5sqrtf17h8c1b66187740c44bE
                      (f32.add
                        (local.get 4)
                        (f32.const 0x1p+0 (;=1;))))
                    (f32.const 0x1p+0 (;=1;)))))))
          (br 2 (;@1;)))
        (local.set 2
          (f32.add
            (call $_ZN4libm4math4logf4logf17h7b88872ed73a994aE
              (local.get 2))
            (f32.const 0x1.62e43p-1 (;=0.6931472;))))
        (br 1 (;@1;)))
      (local.set 2
        (call $_ZN4libm4math4logf4logf17h7b88872ed73a994aE
          (f32.add
            (f32.add
              (local.get 2)
              (local.get 2))
            (f32.div
              (f32.const 0x1p+0 (;=1;))
              (f32.add
                (local.get 2)
                (call $_ZN4libm4math4sqrt5sqrtf17h8c1b66187740c44bE
                  (f32.add
                    (f32.mul
                      (local.get 0)
                      (local.get 0))
                    (f32.const 0x1p+0 (;=1;))))))))))
    (global.set $__stack_pointer
      (i32.add
        (local.get 1)
        (i32.const 16)))
    (select
      (f32.neg
        (local.get 2))
      (local.get 2)
      (i32.lt_s
        (i32.reinterpret_f32
          (local.get 0))
        (i32.const 0)))
  )
  (func $libm_atan (;14;) (type 0) (param f64) (result f64)
    (call $_ZN4libm4math4atan4atan17hb57e8f80d0bd9eddE
      (local.get 0))
  )
  (func $_ZN4libm4math4atan4atan17hb57e8f80d0bd9eddE (;15;) (type 0) (param f64) (result f64)
    (local i32 i64 i32 i32 f64 f64 f64)
    (local.set 1
      (i32.sub
        (global.get $__stack_pointer)
        (i32.const 16)))
    (block ;; label = @1
      (block ;; label = @2
        (block ;; label = @3
          (block ;; label = @4
            (block ;; label = @5
              (block ;; label = @6
                (br_if 0 (;@6;)
                  (i32.gt_u
                    (local.tee 3
                      (i32.and
                        (i32.wrap_i64
                          (i64.shr_u
                            (local.tee 2
                              (i64.reinterpret_f64
                                (local.get 0)))
                            (i64.const 32)))
                        (i32.const 2147483647)))
                    (i32.const 1141899263)))
                (br_if 1 (;@5;)
                  (i32.le_u
                    (local.get 3)
                    (i32.const 1071382527)))
                (local.set 0
                  (f64.abs
                    (local.get 0)))
                (br_if 3 (;@3;)
                  (i32.lt_u
                    (local.get 3)
                    (i32.const 1072889856)))
                (br_if 2 (;@4;)
                  (i32.lt_u
                    (local.get 3)
                    (i32.const 1073971200)))
                (local.set 0
                  (f64.div
                    (f64.const -0x1p+0 (;=-1;))
                    (local.get 0)))
                (local.set 4
                  (i32.const 3))
                (br 4 (;@2;)))
              (br_if 4 (;@1;)
                (f64.ne
                  (local.get 0)
                  (local.get 0)))
              (return
                (f64.copysign
                  (f64.const 0x1.921fb54442d18p+0 (;=1.5707963267948966;))
                  (local.get 0))))
            (local.set 4
              (i32.const -1))
            (br_if 2 (;@2;)
              (i32.ge_u
                (local.get 3)
                (i32.const 1044381696)))
            (br_if 3 (;@1;)
              (i32.ge_u
                (local.get 3)
                (i32.const 1048576)))
            (f32.store offset=12
              (local.get 1)
              (f32.demote_f64
                (local.get 0)))
            (drop
              (f32.load offset=12
                (local.get 1)))
            (return
              (local.get 0)))
          (local.set 0
            (f64.div
              (f64.add
                (local.get 0)
                (f64.const -0x1.8p+0 (;=-1.5;)))
              (f64.add
                (f64.mul
                  (local.get 0)
                  (f64.const 0x1.8p+0 (;=1.5;)))
                (f64.const 0x1p+0 (;=1;)))))
          (local.set 4
            (i32.const 2))
          (br 1 (;@2;)))
        (block ;; label = @3
          (br_if 0 (;@3;)
            (i32.lt_u
              (local.get 3)
              (i32.const 1072037888)))
          (local.set 0
            (f64.div
              (f64.add
                (local.get 0)
                (f64.const -0x1p+0 (;=-1;)))
              (f64.add
                (local.get 0)
                (f64.const 0x1p+0 (;=1;)))))
          (local.set 4
            (i32.const 1))
          (br 1 (;@2;)))
        (local.set 0
          (f64.div
            (f64.add
              (f64.add
                (local.get 0)
                (local.get 0))
              (f64.const -0x1p+0 (;=-1;)))
            (f64.add
              (local.get 0)
              (f64.const 0x1p+1 (;=2;)))))
        (local.set 4
          (i32.const 0)))
      (local.set 7
        (f64.mul
          (local.tee 6
            (f64.mul
              (local.tee 5
                (f64.mul
                  (local.get 0)
                  (local.get 0)))
              (local.get 5)))
          (f64.add
            (f64.mul
              (local.get 6)
              (f64.add
                (f64.mul
                  (local.get 6)
                  (f64.add
                    (f64.mul
                      (local.get 6)
                      (f64.add
                        (f64.mul
                          (local.get 6)
                          (f64.const -0x1.2b4442c6a6c2fp-5 (;=-0.036531572744216916;)))
                        (f64.const -0x1.dde2d52defd9ap-5 (;=-0.058335701337905735;))))
                    (f64.const -0x1.3b0f2af749a6dp-4 (;=-0.0769187620504483;))))
                (f64.const -0x1.c71c6fe231671p-4 (;=-0.11111110405462356;))))
            (f64.const -0x1.999999998ebc4p-3 (;=-0.19999999999876483;)))))
      (local.set 6
        (f64.mul
          (local.get 5)
          (f64.add
            (f64.mul
              (local.get 6)
              (f64.add
                (f64.mul
                  (local.get 6)
                  (f64.add
                    (f64.mul
                      (local.get 6)
                      (f64.add
                        (f64.mul
                          (local.get 6)
                          (f64.add
                            (f64.mul
                              (local.get 6)
                              (f64.const 0x1.0ad3ae322da11p-6 (;=0.016285820115365782;)))
                            (f64.const 0x1.97b4b24760debp-5 (;=0.049768779946159324;))))
                        (f64.const 0x1.10d66a0d03d51p-4 (;=0.06661073137387531;))))
                    (f64.const 0x1.745cdc54c206ep-4 (;=0.09090887133436507;))))
                (f64.const 0x1.24924920083ffp-3 (;=0.14285714272503466;))))
            (f64.const 0x1.555555555550dp-2 (;=0.3333333333333293;)))))
      (block ;; label = @2
        (br_if 0 (;@2;)
          (i32.le_u
            (local.get 3)
            (i32.const 1071382527)))
        (return
          (select
            (f64.neg
              (local.tee 0
                (f64.sub
                  (f64.load offset=1053536
                    (local.tee 3
                      (i32.shl
                        (local.get 4)
                        (i32.const 3))))
                  (f64.sub
                    (f64.sub
                      (f64.mul
                        (local.get 0)
                        (f64.add
                          (local.get 7)
                          (local.get 6)))
                      (f64.load offset=1053568
                        (local.get 3)))
                    (local.get 0)))))
            (local.get 0)
            (i64.lt_s
              (local.get 2)
              (i64.const 0)))))
      (local.set 0
        (f64.sub
          (local.get 0)
          (f64.mul
            (local.get 0)
            (f64.add
              (local.get 7)
              (local.get 6))))))
    (local.get 0)
  )
  (func $libm_atan2 (;16;) (type 2) (param f64 f64) (result f64)
    (local i64 i32 i32 i32 i32 i32 f64)
    (block ;; label = @1
      (br_if 0 (;@1;)
        (i32.and
          (f64.eq
            (local.get 1)
            (local.get 1))
          (f64.eq
            (local.get 0)
            (local.get 0))))
      (return
        (f64.add
          (local.get 0)
          (local.get 1))))
    (block ;; label = @1
      (br_if 0 (;@1;)
        (i32.or
          (i32.add
            (local.tee 3
              (i32.wrap_i64
                (i64.shr_u
                  (local.tee 2
                    (i64.reinterpret_f64
                      (local.get 1)))
                  (i64.const 32))))
            (i32.const -1072693248))
          (local.tee 4
            (i32.wrap_i64
              (local.get 2)))))
      (return
        (call $_ZN4libm4math4atan4atan17hb57e8f80d0bd9eddE
          (local.get 0))))
    (local.set 6
      (i32.or
        (local.tee 5
          (i32.and
            (i32.shr_u
              (local.get 3)
              (i32.const 30))
            (i32.const 2)))
        (i32.wrap_i64
          (i64.shr_u
            (local.tee 2
              (i64.reinterpret_f64
                (local.get 0)))
            (i64.const 63)))))
    (block ;; label = @1
      (block ;; label = @2
        (block ;; label = @3
          (block ;; label = @4
            (br_if 0 (;@4;)
              (i32.or
                (local.tee 7
                  (i32.and
                    (i32.wrap_i64
                      (i64.shr_u
                        (local.get 2)
                        (i64.const 32)))
                    (i32.const 2147483647)))
                (i32.wrap_i64
                  (local.get 2))))
            (local.set 8
              (f64.const -0x1.921fb54442d18p+1 (;=-3.141592653589793;)))
            (block ;; label = @5
              (block ;; label = @6
                (br_table 0 (;@6;) 0 (;@6;) 1 (;@5;) 3 (;@3;) 0 (;@6;)
                  (local.get 6)))
              (return
                (local.get 0)))
            (return
              (f64.const 0x1.921fb54442d18p+1 (;=3.141592653589793;))))
          (br_if 2 (;@1;)
            (i32.eqz
              (i32.or
                (local.tee 3
                  (i32.and
                    (local.get 3)
                    (i32.const 2147483647)))
                (local.get 4))))
          (block ;; label = @4
            (block ;; label = @5
              (br_if 0 (;@5;)
                (i32.ne
                  (local.get 3)
                  (i32.const 2146435072)))
              (br_if 1 (;@4;)
                (i32.ne
                  (local.get 7)
                  (i32.const 2146435072)))
              (return
                (f64.load offset=1053616
                  (i32.shl
                    (local.get 6)
                    (i32.const 3)))))
            (br_if 2 (;@2;)
              (i32.eq
                (local.get 7)
                (i32.const 2146435072)))
            (br_if 2 (;@2;)
              (i32.lt_u
                (i32.add
                  (local.get 3)
                  (i32.const 67108864))
                (local.get 7)))
            (block ;; label = @5
              (block ;; label = @6
                (br_if 0 (;@6;)
                  (i32.eqz
                    (local.get 5)))
                (local.set 8
                  (f64.const 0x0p+0 (;=0;)))
                (br_if 1 (;@5;)
                  (i32.lt_u
                    (i32.add
                      (local.get 7)
                      (i32.const 67108864))
                    (local.get 3))))
              (local.set 8
                (call $_ZN4libm4math4atan4atan17hb57e8f80d0bd9eddE
                  (f64.abs
                    (f64.div
                      (local.get 0)
                      (local.get 1))))))
            (block ;; label = @5
              (block ;; label = @6
                (block ;; label = @7
                  (br_table 4 (;@3;) 1 (;@6;) 2 (;@5;) 0 (;@7;) 4 (;@3;)
                    (local.get 6)))
                (return
                  (f64.add
                    (f64.add
                      (local.get 8)
                      (f64.const -0x1.1a62633145c07p-53 (;=-0.00000000000000012246467991473532;)))
                    (f64.const -0x1.921fb54442d18p+1 (;=-3.141592653589793;)))))
              (return
                (f64.neg
                  (local.get 8))))
            (return
              (f64.sub
                (f64.const 0x1.921fb54442d18p+1 (;=3.141592653589793;))
                (f64.add
                  (local.get 8)
                  (f64.const -0x1.1a62633145c07p-53 (;=-0.00000000000000012246467991473532;))))))
          (local.set 8
            (f64.load offset=1053648
              (i32.shl
                (local.get 6)
                (i32.const 3)))))
        (return
          (local.get 8)))
      (return
        (f64.copysign
          (f64.const 0x1.921fb54442d18p+0 (;=1.5707963267948966;))
          (local.get 0))))
    (f64.copysign
      (f64.const 0x1.921fb54442d18p+0 (;=1.5707963267948966;))
      (local.get 0))
  )
  (func $libm_atan2f (;17;) (type 3) (param f32 f32) (result f32)
    (local i32 i32 i32 i32 f32)
    (block ;; label = @1
      (br_if 0 (;@1;)
        (i32.and
          (f32.eq
            (local.get 1)
            (local.get 1))
          (f32.eq
            (local.get 0)
            (local.get 0))))
      (return
        (f32.add
          (local.get 0)
          (local.get 1))))
    (block ;; label = @1
      (br_if 0 (;@1;)
        (i32.ne
          (local.tee 2
            (i32.reinterpret_f32
              (local.get 1)))
          (i32.const 1065353216)))
      (return
        (call $_ZN4libm4math5atanf5atanf17h0afe9904700c13b3E
          (local.get 0))))
    (local.set 5
      (i32.or
        (local.tee 3
          (i32.and
            (i32.shr_u
              (local.get 2)
              (i32.const 30))
            (i32.const 2)))
        (i32.shr_u
          (local.tee 4
            (i32.reinterpret_f32
              (local.get 0)))
          (i32.const 31))))
    (block ;; label = @1
      (block ;; label = @2
        (block ;; label = @3
          (block ;; label = @4
            (block ;; label = @5
              (block ;; label = @6
                (block ;; label = @7
                  (block ;; label = @8
                    (br_if 0 (;@8;)
                      (local.tee 4
                        (i32.and
                          (local.get 4)
                          (i32.const 2147483647))))
                    (local.set 6
                      (f32.const -0x1.921fb6p+1 (;=-3.1415927;)))
                    (br_table 1 (;@7;) 1 (;@7;) 2 (;@6;) 6 (;@2;) 1 (;@7;)
                      (local.get 5)))
                  (br_if 2 (;@5;)
                    (i32.eqz
                      (local.tee 2
                        (i32.and
                          (local.get 2)
                          (i32.const 2147483647)))))
                  (br_if 3 (;@4;)
                    (i32.ne
                      (local.get 2)
                      (i32.const 2139095040)))
                  (br_if 4 (;@3;)
                    (i32.ne
                      (local.get 4)
                      (i32.const 2139095040)))
                  (return
                    (f32.load offset=1053680
                      (i32.shl
                        (local.get 5)
                        (i32.const 2)))))
                (return
                  (local.get 0)))
              (return
                (f32.const 0x1.921fb6p+1 (;=3.1415927;))))
            (return
              (f32.copysign
                (f32.const 0x1.921fb6p+0 (;=1.5707964;))
                (local.get 0))))
          (br_if 2 (;@1;)
            (i32.eq
              (local.get 4)
              (i32.const 2139095040)))
          (br_if 2 (;@1;)
            (i32.lt_u
              (i32.add
                (local.get 2)
                (i32.const 218103808))
              (local.get 4)))
          (block ;; label = @4
            (block ;; label = @5
              (br_if 0 (;@5;)
                (i32.eqz
                  (local.get 3)))
              (local.set 6
                (f32.const 0x0p+0 (;=0;)))
              (br_if 1 (;@4;)
                (i32.lt_u
                  (i32.add
                    (local.get 4)
                    (i32.const 218103808))
                  (local.get 2))))
            (local.set 6
              (call $_ZN4libm4math5atanf5atanf17h0afe9904700c13b3E
                (f32.abs
                  (f32.div
                    (local.get 0)
                    (local.get 1))))))
          (block ;; label = @4
            (block ;; label = @5
              (block ;; label = @6
                (br_table 4 (;@2;) 1 (;@5;) 2 (;@4;) 0 (;@6;) 4 (;@2;)
                  (local.get 5)))
              (return
                (f32.add
                  (f32.add
                    (local.get 6)
                    (f32.const 0x1.777a5cp-24 (;=0.00000008742278;)))
                  (f32.const -0x1.921fb6p+1 (;=-3.1415927;)))))
            (return
              (f32.neg
                (local.get 6))))
          (return
            (f32.sub
              (f32.const 0x1.921fb6p+1 (;=3.1415927;))
              (f32.add
                (local.get 6)
                (f32.const 0x1.777a5cp-24 (;=0.00000008742278;))))))
        (local.set 6
          (f32.load offset=1053696
            (i32.shl
              (local.get 5)
              (i32.const 2)))))
      (return
        (local.get 6)))
    (f32.copysign
      (f32.const 0x1.921fb6p+0 (;=1.5707964;))
      (local.get 0))
  )
  (func $_ZN4libm4math5atanf5atanf17h0afe9904700c13b3E (;18;) (type 1) (param f32) (result f32)
    (local i32 i32 f32 i32 i32 f32 f32)
    (local.set 1
      (i32.sub
        (global.get $__stack_pointer)
        (i32.const 16)))
    (local.set 2
      (i32.reinterpret_f32
        (local.get 0)))
    (block ;; label = @1
      (block ;; label = @2
        (br_if 0 (;@2;)
          (i32.gt_u
            (local.tee 4
              (i32.reinterpret_f32
                (local.tee 3
                  (f32.abs
                    (local.get 0)))))
            (i32.const 1283457023)))
        (block ;; label = @3
          (block ;; label = @4
            (block ;; label = @5
              (block ;; label = @6
                (br_if 0 (;@6;)
                  (i32.le_u
                    (local.get 4)
                    (i32.const 1054867455)))
                (br_if 2 (;@4;)
                  (i32.lt_u
                    (local.get 4)
                    (i32.const 1066926080)))
                (br_if 1 (;@5;)
                  (i32.lt_u
                    (local.get 4)
                    (i32.const 1075576832)))
                (local.set 0
                  (f32.div
                    (f32.const -0x1p+0 (;=-1;))
                    (local.get 3)))
                (local.set 5
                  (i32.const 3))
                (br 3 (;@3;)))
              (local.set 5
                (i32.const -1))
              (br_if 2 (;@3;)
                (i32.ge_u
                  (local.get 4)
                  (i32.const 964689920)))
              (br_if 4 (;@1;)
                (i32.ge_u
                  (local.get 4)
                  (i32.const 8388608)))
              (f32.store offset=12
                (local.get 1)
                (f32.mul
                  (local.get 0)
                  (local.get 0)))
              (drop
                (f32.load offset=12
                  (local.get 1)))
              (return
                (local.get 0)))
            (local.set 0
              (f32.div
                (f32.add
                  (local.get 3)
                  (f32.const -0x1.8p+0 (;=-1.5;)))
                (f32.add
                  (f32.mul
                    (local.get 3)
                    (f32.const 0x1.8p+0 (;=1.5;)))
                  (f32.const 0x1p+0 (;=1;)))))
            (local.set 5
              (i32.const 2))
            (br 1 (;@3;)))
          (block ;; label = @4
            (br_if 0 (;@4;)
              (i32.lt_u
                (local.get 4)
                (i32.const 1060110336)))
            (local.set 0
              (f32.div
                (f32.add
                  (local.get 3)
                  (f32.const -0x1p+0 (;=-1;)))
                (f32.add
                  (local.get 3)
                  (f32.const 0x1p+0 (;=1;)))))
            (local.set 5
              (i32.const 1))
            (br 1 (;@3;)))
          (local.set 0
            (f32.div
              (f32.add
                (f32.add
                  (local.get 3)
                  (local.get 3))
                (f32.const -0x1p+0 (;=-1;)))
              (f32.add
                (local.get 3)
                (f32.const 0x1p+1 (;=2;)))))
          (local.set 5
            (i32.const 0)))
        (local.set 7
          (f32.mul
            (local.tee 3
              (f32.mul
                (local.tee 6
                  (f32.mul
                    (local.get 0)
                    (local.get 0)))
                (local.get 6)))
            (f32.add
              (f32.mul
                (local.get 3)
                (f32.const -0x1.b4248ep-4 (;=-0.106480174;)))
              (f32.const -0x1.99953p-3 (;=-0.19999158;)))))
        (local.set 3
          (f32.mul
            (local.get 6)
            (f32.add
              (f32.mul
                (local.get 3)
                (f32.add
                  (f32.mul
                    (local.get 3)
                    (f32.const 0x1.f9584ap-5 (;=0.061687607;)))
                  (f32.const 0x1.23ea1ap-3 (;=0.14253636;))))
              (f32.const 0x1.555552p-2 (;=0.33333328;)))))
        (block ;; label = @3
          (br_if 0 (;@3;)
            (i32.le_u
              (local.get 4)
              (i32.const 1054867455)))
          (return
            (select
              (local.tee 0
                (f32.sub
                  (f32.load offset=1048600
                    (local.tee 4
                      (i32.shl
                        (local.get 5)
                        (i32.const 2))))
                  (f32.sub
                    (f32.sub
                      (f32.mul
                        (local.get 0)
                        (f32.add
                          (local.get 7)
                          (local.get 3)))
                      (f32.load offset=1048616
                        (local.get 4)))
                    (local.get 0))))
              (f32.neg
                (local.get 0))
              (i32.gt_s
                (local.get 2)
                (i32.const -1)))))
        (local.set 0
          (f32.sub
            (local.get 0)
            (f32.mul
              (local.get 0)
              (f32.add
                (local.get 7)
                (local.get 3)))))
        (br 1 (;@1;)))
      (br_if 0 (;@1;)
        (f32.ne
          (local.get 0)
          (local.get 0)))
      (return
        (select
          (f32.const 0x1.921fb4p+0 (;=1.5707963;))
          (f32.const -0x1.921fb4p+0 (;=-1.5707963;))
          (i32.gt_s
            (local.get 2)
            (i32.const -1)))))
    (local.get 0)
  )
  (func $libm_atanf (;19;) (type 1) (param f32) (result f32)
    (call $_ZN4libm4math5atanf5atanf17h0afe9904700c13b3E
      (local.get 0))
  )
  (func $libm_atanh (;20;) (type 0) (param f64) (result f64)
    (local i32 f64 i64 i32)
    (global.set $__stack_pointer
      (local.tee 1
        (i32.sub
          (global.get $__stack_pointer)
          (i32.const 16))))
    (local.set 2
      (f64.abs
        (local.get 0)))
    (block ;; label = @1
      (block ;; label = @2
        (br_if 0 (;@2;)
          (i32.lt_u
            (local.tee 4
              (i32.and
                (i32.wrap_i64
                  (i64.shr_u
                    (local.tee 3
                      (i64.reinterpret_f64
                        (local.get 0)))
                    (i64.const 52)))
                (i32.const 2047)))
            (i32.const 1022)))
        (local.set 2
          (f64.mul
            (call $_ZN4libm4math5log1p5log1p17h5d4b372f78bb46e9E
              (f64.add
                (local.tee 2
                  (f64.div
                    (local.get 2)
                    (f64.sub
                      (f64.const 0x1p+0 (;=1;))
                      (local.get 2))))
                (local.get 2)))
            (f64.const 0x1p-1 (;=0.5;))))
        (br 1 (;@1;)))
      (block ;; label = @2
        (br_if 0 (;@2;)
          (i32.lt_u
            (local.get 4)
            (i32.const 991)))
        (local.set 2
          (f64.mul
            (call $_ZN4libm4math5log1p5log1p17h5d4b372f78bb46e9E
              (f64.add
                (local.tee 0
                  (f64.add
                    (local.get 2)
                    (local.get 2)))
                (f64.div
                  (f64.mul
                    (local.get 2)
                    (local.get 0))
                  (f64.sub
                    (f64.const 0x1p+0 (;=1;))
                    (local.get 2)))))
            (f64.const 0x1p-1 (;=0.5;))))
        (br 1 (;@1;)))
      (br_if 0 (;@1;)
        (local.get 4))
      (f32.store offset=12
        (local.get 1)
        (f32.demote_f64
          (local.get 2)))
      (drop
        (f32.load offset=12
          (local.get 1))))
    (global.set $__stack_pointer
      (i32.add
        (local.get 1)
        (i32.const 16)))
    (select
      (f64.neg
        (local.get 2))
      (local.get 2)
      (i64.lt_s
        (local.get 3)
        (i64.const 0)))
  )
  (func $libm_atanhf (;21;) (type 1) (param f32) (result f32)
    (local i32 f32 i32 f32)
    (global.set $__stack_pointer
      (local.tee 1
        (i32.sub
          (global.get $__stack_pointer)
          (i32.const 16))))
    (block ;; label = @1
      (block ;; label = @2
        (br_if 0 (;@2;)
          (i32.lt_u
            (local.tee 3
              (i32.reinterpret_f32
                (local.tee 2
                  (f32.abs
                    (local.get 0)))))
            (i32.const 1056964608)))
        (local.set 2
          (f32.mul
            (call $_ZN4libm4math6log1pf6log1pf17h4c967d18f426e9e4E
              (f32.add
                (local.tee 2
                  (f32.div
                    (local.get 2)
                    (f32.sub
                      (f32.const 0x1p+0 (;=1;))
                      (local.get 2))))
                (local.get 2)))
            (f32.const 0x1p-1 (;=0.5;))))
        (br 1 (;@1;)))
      (block ;; label = @2
        (br_if 0 (;@2;)
          (i32.lt_u
            (local.get 3)
            (i32.const 796917760)))
        (local.set 2
          (f32.mul
            (call $_ZN4libm4math6log1pf6log1pf17h4c967d18f426e9e4E
              (f32.add
                (local.tee 4
                  (f32.add
                    (local.get 2)
                    (local.get 2)))
                (f32.div
                  (f32.mul
                    (local.get 2)
                    (local.get 4))
                  (f32.sub
                    (f32.const 0x1p+0 (;=1;))
                    (local.get 2)))))
            (f32.const 0x1p-1 (;=0.5;))))
        (br 1 (;@1;)))
      (br_if 0 (;@1;)
        (i32.gt_u
          (local.get 3)
          (i32.const 8388607)))
      (f32.store offset=12
        (local.get 1)
        (f32.mul
          (local.get 0)
          (local.get 0)))
      (drop
        (f32.load offset=12
          (local.get 1))))
    (global.set $__stack_pointer
      (i32.add
        (local.get 1)
        (i32.const 16)))
    (select
      (f32.neg
        (local.get 2))
      (local.get 2)
      (i32.lt_s
        (i32.reinterpret_f32
          (local.get 0))
        (i32.const 0)))
  )
  (func $libm_cbrt (;22;) (type 0) (param f64) (result f64)
    (local i32 i64 i32 i32 i64 i64 f64 f64 f64 i32 f64 f64 f64 f64)
    (global.set $__stack_pointer
      (local.tee 1
        (i32.sub
          (global.get $__stack_pointer)
          (i32.const 48))))
    (i64.store offset=40
      (local.get 1)
      (i64.const -4625196817309499392))
    (i64.store offset=32
      (local.get 1)
      (i64.const 4598175219545276416))
    (i64.store offset=24
      (local.get 1)
      (i64.const -4620693217682128896))
    (i64.store offset=16
      (local.get 1)
      (i64.const 4602678819172646912))
    (i64.store offset=8
      (local.get 1)
      (i64.const -4616189618054758400))
    (i64.store
      (local.get 1)
      (i64.const 4607182418800017408))
    (local.set 4
      (i32.and
        (local.tee 3
          (i32.wrap_i64
            (i64.shr_u
              (local.tee 2
                (i64.reinterpret_f64
                  (local.get 0)))
              (i64.const 52))))
        (i32.const 2047)))
    (block ;; label = @1
      (block ;; label = @2
        (block ;; label = @3
          (br_if 0 (;@3;)
            (i32.eqz
              (i32.and
                (i32.add
                  (local.get 3)
                  (i32.const 1))
                (i32.const 2046))))
          (local.set 5
            (local.get 2))
          (br 1 (;@2;)))
        (block ;; label = @3
          (block ;; label = @4
            (br_if 0 (;@4;)
              (i64.eqz
                (local.tee 5
                  (i64.and
                    (i64.reinterpret_f64
                      (local.get 0))
                    (i64.const 9223372036854775807)))))
            (br_if 1 (;@3;)
              (i32.ne
                (local.get 4)
                (i32.const 2047))))
          (local.set 2
            (i64.reinterpret_f64
              (f64.add
                (local.get 0)
                (local.get 0))))
          (br 2 (;@1;)))
        (local.set 5
          (i64.shl
            (local.get 2)
            (i64.add
              (local.tee 6
                (i64.clz
                  (local.get 5)))
              (i64.const 53))))
        (local.set 4
          (i32.add
            (i32.sub
              (local.get 4)
              (i32.wrap_i64
                (local.get 6)))
            (i32.const 12))))
      (local.set 11
        (call $_ZN4libm4math3fma3fma17hfec38a2f137b951cE
          (local.tee 7
            (f64.mul
              (f64.sub
                (local.tee 8
                  (f64.add
                    (f64.add
                      (f64.mul
                        (local.tee 7
                          (f64.reinterpret_i64
                            (local.tee 5
                              (i64.or
                                (i64.and
                                  (local.get 5)
                                  (i64.const 4503599627370495))
                                (i64.const 4607182418800017408)))))
                        (f64.const 0x1.2c9a3e94d1da5p-1 (;=0.5871142918266982;)))
                      (f64.const 0x1.1b0babccfef9cp-1 (;=0.5528234184016472;)))
                    (f64.mul
                      (f64.mul
                        (local.get 7)
                        (local.get 7))
                      (f64.add
                        (f64.mul
                          (local.get 7)
                          (f64.const 0x1.7a8d3e4ec9b07p-6 (;=0.02310496411078147;)))
                        (f64.const -0x1.4dc30b1a1ddbap-3 (;=-0.16296967194987905;))))))
                (f64.mul
                  (f64.mul
                    (local.get 8)
                    (local.tee 7
                      (f64.add
                        (f64.mul
                          (f64.mul
                            (local.get 8)
                            (local.get 8))
                          (f64.mul
                            (local.get 8)
                            (local.tee 9
                              (f64.div
                                (f64.const 0x1p+0 (;=1;))
                                (local.get 7)))))
                        (f64.const -0x1p+0 (;=-1;)))))
                  (f64.add
                    (f64.mul
                      (local.get 7)
                      (f64.const -0x1.c71c71c71c71cp-3 (;=-0.2222222222222222;)))
                    (f64.const 0x1.5555555555555p-2 (;=0.3333333333333333;)))))
              (f64.reinterpret_i64
                (i64.or
                  (i64.load offset=1052864
                    (i32.shl
                      (local.tee 10
                        (i32.and
                          (local.tee 3
                            (i32.sub
                              (local.tee 4
                                (i32.add
                                  (local.get 4)
                                  (i32.const 3072)))
                              (i32.mul
                                (local.tee 4
                                  (i32.div_u
                                    (i32.and
                                      (local.get 4)
                                      (i32.const 65535))
                                    (i32.const 3)))
                                (i32.const 3))))
                          (i32.const 65535)))
                      (i32.const 3)))
                  (i64.and
                    (local.get 2)
                    (i64.const -9223372036854775808))))))
          (local.get 7)
          (f64.neg
            (local.tee 8
              (f64.mul
                (local.get 7)
                (local.get 7))))))
      (block ;; label = @2
        (block ;; label = @3
          (br_if 0 (;@3;)
            (f64.lt
              (f64.abs
                (f64.add
                  (local.tee 9
                    (f64.abs
                      (local.tee 7
                        (f64.sub
                          (f64.sub
                            (local.get 7)
                            (local.tee 8
                              (f64.sub
                                (local.get 7)
                                (local.tee 9
                                  (f64.mul
                                    (f64.mul
                                      (local.get 7)
                                      (f64.const 0x1.5555555555555p-2 (;=0.3333333333333333;)))
                                    (f64.mul
                                      (local.tee 12
                                        (f64.mul
                                          (f64.load
                                            (i32.add
                                              (i32.add
                                                (local.get 1)
                                                (i32.shl
                                                  (local.get 10)
                                                  (i32.const 4)))
                                              (i32.shl
                                                (i32.wrap_i64
                                                  (i64.shr_u
                                                    (local.get 2)
                                                    (i64.const 63)))
                                                (i32.const 3))))
                                          (local.get 9)))
                                      (f64.add
                                        (f64.add
                                          (call $_ZN4libm4math3fma3fma17hfec38a2f137b951cE
                                            (local.get 7)
                                            (local.get 8)
                                            (f64.neg
                                              (local.tee 9
                                                (f64.mul
                                                  (local.get 7)
                                                  (local.get 8)))))
                                          (f64.mul
                                            (local.get 11)
                                            (local.get 7)))
                                        (f64.sub
                                          (local.get 9)
                                          (local.tee 13
                                            (f64.copysign
                                              (local.tee 11
                                                (f64.reinterpret_i64
                                                  (i64.add
                                                    (i64.shl
                                                      (i64.extend_i32_u
                                                        (local.get 3))
                                                      (i64.const 52))
                                                    (local.get 5))))
                                              (local.get 0)))))))))))
                          (local.get 9)))))
                  (f64.const -0x1p-53 (;=-0.00000000000000011102230246251565;))))
              (f64.const 0x1p-75 (;=0.000000000000000000000026469779601696886;))))
          (br_if 1 (;@2;)
            (i32.eqz
              (f64.lt
                (f64.abs
                  (f64.add
                    (local.get 9)
                    (f64.const -0x1.8p-52 (;=-0.00000000000000033306690738754696;))))
                (f64.const 0x1p-75 (;=0.000000000000000000000026469779601696886;))))))
        (local.set 9
          (call $_ZN4libm4math3fma3fma17hfec38a2f137b951cE
            (local.get 8)
            (local.get 8)
            (f64.neg
              (local.tee 7
                (f64.mul
                  (local.get 8)
                  (local.get 8))))))
        (block ;; label = @3
          (br_if 0 (;@3;)
            (f64.lt
              (f64.abs
                (f64.add
                  (local.tee 8
                    (f64.abs
                      (local.tee 7
                        (f64.sub
                          (f64.sub
                            (local.get 8)
                            (local.tee 9
                              (f64.sub
                                (local.get 8)
                                (local.tee 7
                                  (f64.mul
                                    (f64.mul
                                      (local.get 8)
                                      (f64.const 0x1.5555555555555p-2 (;=0.3333333333333333;)))
                                    (f64.mul
                                      (local.get 12)
                                      (f64.add
                                        (f64.sub
                                          (local.tee 14
                                            (f64.mul
                                              (local.get 8)
                                              (local.get 7)))
                                          (local.get 13))
                                        (f64.add
                                          (call $_ZN4libm4math3fma3fma17hfec38a2f137b951cE
                                            (local.get 8)
                                            (local.get 7)
                                            (f64.neg
                                              (local.get 14)))
                                          (f64.mul
                                            (local.get 8)
                                            (local.get 9))))))))))
                          (local.get 7)))))
                  (f64.const -0x1p-53 (;=-0.00000000000000011102230246251565;))))
              (f64.const 0x1p-98 (;=0.0000000000000000000000000000031554436208840472;))))
          (br_if 0 (;@3;)
            (f64.lt
              (f64.abs
                (f64.add
                  (local.get 8)
                  (f64.const -0x1.8p-52 (;=-0.00000000000000033306690738754696;))))
              (f64.const 0x1p-98 (;=0.0000000000000000000000000000031554436208840472;))))
          (local.set 8
            (local.get 9))
          (br 1 (;@2;)))
        (local.set 8
          (select
            (f64.copysign
              (f64.const 0x1.de87aa837820fp+0 (;=1.86925759992312;))
              (local.get 0))
            (select
              (f64.copysign
                (f64.const 0x1.79d15d0e8d59cp+0 (;=1.4758508835342132;))
                (local.get 0))
              (local.get 9)
              (f64.eq
                (local.get 11)
                (f64.const 0x1.9b78223aa307cp+1 (;=3.2146036897957497;))))
            (f64.eq
              (local.get 11)
              (f64.const 0x1.a202bfc89ddffp+2 (;=6.531417795099968;))))))
      (local.set 2
        (i64.add
          (i64.shl
            (i64.extend_i32_u
              (i32.add
                (local.get 4)
                (i32.const 2731)))
            (i64.const 52))
          (local.tee 5
            (i64.reinterpret_f64
              (local.get 8)))))
      (br_if 0 (;@1;)
        (i64.ge_u
          (i64.or
            (i64.shr_u
              (local.tee 6
                (i64.shl
                  (local.get 5)
                  (i64.const 30)))
              (i64.const 63))
            (local.get 6))
          (i64.const 1073741825)))
      (block ;; label = @2
        (br_if 0 (;@2;)
          (f64.lt
            (f64.abs
              (f64.sub
                (f64.sub
                  (f64.reinterpret_i64
                    (i64.add
                      (i64.and
                        (local.get 5)
                        (i64.const -65536))
                      (i64.const 5373952)))
                  (local.get 8))
                (local.get 7)))
            (f64.const 0x1p-60 (;=0.0000000000000000008673617379884035;))))
        (br_if 1 (;@1;)
          (f64.ne
            (local.get 11)
            (f64.const 0x1p+0 (;=1;)))))
      (local.set 2
        (i64.and
          (i64.add
            (local.get 2)
            (i64.const 32768))
          (i64.const -65536))))
    (global.set $__stack_pointer
      (i32.add
        (local.get 1)
        (i32.const 48)))
    (f64.reinterpret_i64
      (local.get 2))
  )
  (func $_ZN4libm4math3fma3fma17hfec38a2f137b951cE (;23;) (type 4) (param f64 f64 f64) (result f64)
    (local i32 i64 i64 i32 i64 i64 i32 i32 i64 i64 i32 i64 i64 i64)
    (global.set $__stack_pointer
      (local.tee 3
        (i32.sub
          (global.get $__stack_pointer)
          (i32.const 16))))
    (local.set 5
      (local.tee 4
        (i64.reinterpret_f64
          (local.get 0))))
    (block ;; label = @1
      (br_if 0 (;@1;)
        (local.tee 6
          (i32.and
            (i32.wrap_i64
              (i64.shr_u
                (local.get 4)
                (i64.const 52)))
            (i32.const 2047))))
      (local.set 6
        (select
          (i32.add
            (local.tee 6
              (i32.and
                (i32.wrap_i64
                  (i64.shr_u
                    (local.tee 5
                      (i64.reinterpret_f64
                        (f64.mul
                          (local.get 0)
                          (f64.const 0x1p+63 (;=9223372036854776000;)))))
                    (i64.const 52)))
                (i32.const 2047)))
            (i32.const -63))
          (i32.const 2048)
          (local.get 6))))
    (local.set 8
      (local.tee 7
        (i64.reinterpret_f64
          (local.get 1))))
    (block ;; label = @1
      (br_if 0 (;@1;)
        (local.tee 9
          (i32.and
            (i32.wrap_i64
              (i64.shr_u
                (local.get 7)
                (i64.const 52)))
            (i32.const 2047))))
      (local.set 9
        (select
          (i32.add
            (local.tee 10
              (i32.and
                (i32.wrap_i64
                  (i64.shr_u
                    (local.tee 8
                      (i64.reinterpret_f64
                        (f64.mul
                          (local.get 1)
                          (f64.const 0x1p+63 (;=9223372036854776000;)))))
                    (i64.const 52)))
                (i32.const 2047)))
            (i32.const -63))
          (i32.const 2048)
          (local.get 10))))
    (local.set 12
      (local.tee 11
        (i64.reinterpret_f64
          (local.get 2))))
    (block ;; label = @1
      (br_if 0 (;@1;)
        (local.tee 10
          (i32.and
            (i32.wrap_i64
              (i64.shr_u
                (local.get 11)
                (i64.const 52)))
            (i32.const 2047))))
      (local.set 10
        (select
          (i32.add
            (local.tee 10
              (i32.and
                (i32.wrap_i64
                  (i64.shr_u
                    (local.tee 12
                      (i64.reinterpret_f64
                        (f64.mul
                          (local.get 2)
                          (f64.const 0x1p+63 (;=9223372036854776000;)))))
                    (i64.const 52)))
                (i32.const 2047)))
            (i32.const -63))
          (i32.const 2048)
          (local.get 10))))
    (block ;; label = @1
      (block ;; label = @2
        (block ;; label = @3
          (br_if 0 (;@3;)
            (i32.gt_s
              (local.get 6)
              (i32.const 2046)))
          (br_if 1 (;@2;)
            (i32.lt_s
              (local.get 9)
              (i32.const 2047))))
        (local.set 0
          (f64.add
            (f64.mul
              (local.get 0)
              (local.get 1))
            (local.get 2)))
        (br 1 (;@1;)))
      (local.set 13
        (i32.add
          (local.get 10)
          (i32.const -1076)))
      (block ;; label = @2
        (block ;; label = @3
          (block ;; label = @4
            (br_if 0 (;@4;)
              (i32.gt_s
                (local.get 10)
                (i32.const 2046)))
            (local.set 14
              (i64.or
                (i64.and
                  (i64.shl
                    (local.get 12)
                    (i64.const 1))
                  (i64.const 9007199254740990))
                (i64.const 9007199254740992)))
            (local.set 12
              (i64.const 0))
            (call $__multi3
              (local.get 3)
              (i64.or
                (i64.and
                  (i64.shl
                    (local.get 8)
                    (i64.const 1))
                  (i64.const 9007199254740990))
                (i64.const 9007199254740992))
              (i64.const 0)
              (i64.or
                (i64.and
                  (i64.shl
                    (local.get 5)
                    (i64.const 1))
                  (i64.const 9007199254740990))
                (i64.const 9007199254740992))
              (i64.const 0))
            (local.set 15
              (i64.load offset=8
                (local.get 3)))
            (local.set 8
              (i64.load
                (local.get 3)))
            (block ;; label = @5
              (br_if 0 (;@5;)
                (i32.gt_s
                  (local.tee 6
                    (i32.sub
                      (local.get 13)
                      (local.tee 9
                        (i32.add
                          (i32.add
                            (local.get 9)
                            (local.get 6))
                          (i32.const -2152)))))
                  (i32.const 0)))
              (block ;; label = @6
                (br_if 0 (;@6;)
                  (i32.ne
                    (local.get 13)
                    (local.get 9)))
                (local.set 5
                  (local.get 14))
                (local.set 9
                  (local.get 13))
                (br 4 (;@2;)))
              (block ;; label = @6
                (br_if 0 (;@6;)
                  (i32.ge_s
                    (local.get 6)
                    (i32.const -63)))
                (local.set 5
                  (i64.const 1))
                (br 4 (;@2;)))
              (local.set 12
                (i64.const 0))
              (local.set 5
                (i64.or
                  (i64.shr_u
                    (local.get 14)
                    (i64.extend_i32_u
                      (i32.and
                        (i32.sub
                          (i32.const 0)
                          (local.get 6))
                        (i32.const 63))))
                  (i64.extend_i32_u
                    (i64.ne
                      (i64.shl
                        (local.get 14)
                        (i64.extend_i32_u
                          (i32.and
                            (local.get 6)
                            (i32.const 63))))
                      (i64.const 0)))))
              (br 3 (;@2;)))
            (block ;; label = @5
              (block ;; label = @6
                (br_if 0 (;@6;)
                  (i32.lt_u
                    (local.get 6)
                    (i32.const 64)))
                (local.set 9
                  (i32.add
                    (local.get 10)
                    (i32.const -1140)))
                (br_if 1 (;@5;)
                  (local.tee 10
                    (i32.add
                      (local.get 6)
                      (i32.const -64))))
                (br 3 (;@3;)))
              (local.set 5
                (i64.shl
                  (local.get 14)
                  (i64.extend_i32_u
                    (local.get 6))))
              (local.set 12
                (i64.shr_u
                  (local.get 14)
                  (i64.extend_i32_u
                    (i32.sub
                      (i32.const 64)
                      (local.get 6)))))
              (br 3 (;@2;)))
            (block ;; label = @5
              (br_if 0 (;@5;)
                (i32.le_u
                  (local.get 6)
                  (i32.const 127)))
              (local.set 8
                (i64.const 1))
              (local.set 15
                (i64.const 0))
              (br 2 (;@3;)))
            (local.set 5
              (i64.const 0))
            (local.set 8
              (i64.or
                (local.tee 8
                  (i64.or
                    (i64.shl
                      (local.get 15)
                      (local.tee 12
                        (i64.extend_i32_u
                          (i32.sub
                            (i32.const 128)
                            (local.get 6)))))
                    (i64.shr_u
                      (local.get 8)
                      (local.tee 16
                        (i64.extend_i32_u
                          (local.get 10))))))
                (i64.extend_i32_u
                  (i64.ne
                    (i64.shl
                      (local.get 8)
                      (local.get 12))
                    (i64.const 0)))))
            (local.set 15
              (i64.shr_u
                (local.get 15)
                (local.get 16)))
            (local.set 12
              (local.get 14))
            (br 2 (;@2;)))
          (local.set 0
            (select
              (local.get 2)
              (f64.mul
                (local.get 0)
                (local.get 1))
              (i32.eq
                (local.get 13)
                (i32.const 971))))
          (br 2 (;@1;)))
        (local.set 5
          (i64.const 0))
        (local.set 12
          (local.get 14)))
      (block ;; label = @2
        (block ;; label = @3
          (block ;; label = @4
            (block ;; label = @5
              (block ;; label = @6
                (block ;; label = @7
                  (br_if 0 (;@7;)
                    (i32.xor
                      (i64.lt_s
                        (local.get 11)
                        (i64.const 0))
                      (local.tee 6
                        (i64.gt_s
                          (local.tee 7
                            (i64.xor
                              (local.get 4)
                              (local.get 7)))
                          (i64.const -1)))))
                  (local.set 4
                    (select
                      (local.tee 4
                        (i64.sub
                          (local.get 8)
                          (local.get 5)))
                      (i64.sub
                        (i64.const 0)
                        (local.get 4))
                      (local.tee 13
                        (i64.gt_s
                          (local.tee 11
                            (i64.sub
                              (local.get 15)
                              (i64.add
                                (local.get 12)
                                (i64.extend_i32_u
                                  (i64.lt_u
                                    (local.get 8)
                                    (local.get 5))))))
                          (i64.const -1)))))
                  (local.set 10
                    (select
                      (i64.lt_s
                        (local.get 7)
                        (i64.const 0))
                      (local.get 6)
                      (local.get 13)))
                  (br_if 1 (;@6;)
                    (i32.eqz
                      (i64.eqz
                        (local.tee 7
                          (select
                            (local.get 11)
                            (i64.sub
                              (select
                                (i64.const -1)
                                (i64.const 0)
                                (i64.ne
                                  (local.get 8)
                                  (local.get 5)))
                              (local.get 11))
                            (local.get 13))))))
                  (br_if 2 (;@5;)
                    (i32.eqz
                      (i64.eqz
                        (local.get 4))))
                  (local.set 0
                    (f64.add
                      (f64.mul
                        (local.get 0)
                        (local.get 1))
                      (local.get 2)))
                  (br 6 (;@1;)))
                (local.set 10
                  (i32.wrap_i64
                    (i64.shr_u
                      (local.get 7)
                      (i64.const 63))))
                (local.set 7
                  (i64.add
                    (i64.add
                      (local.get 12)
                      (local.get 15))
                    (i64.extend_i32_u
                      (i64.lt_u
                        (local.tee 4
                          (i64.add
                            (local.get 5)
                            (local.get 8)))
                        (local.get 5))))))
              (local.set 4
                (i64.or
                  (i64.or
                    (i64.shr_u
                      (local.get 4)
                      (i64.sub
                        (i64.const 1)
                        (local.tee 11
                          (i64.clz
                            (local.get 7)))))
                    (i64.shl
                      (local.get 7)
                      (local.tee 5
                        (i64.add
                          (local.get 11)
                          (i64.const -1)))))
                  (i64.extend_i32_u
                    (i64.ne
                      (i64.shl
                        (local.get 4)
                        (local.get 5))
                      (i64.const 0)))))
              (local.set 6
                (i32.add
                  (i32.sub
                    (local.get 9)
                    (i32.wrap_i64
                      (local.get 11)))
                  (i32.const 65)))
              (br_if 1 (;@4;)
                (i32.eqz
                  (local.get 10)))
              (br 2 (;@3;)))
            (local.set 6
              (i32.sub
                (local.get 9)
                (local.tee 13
                  (i32.add
                    (i32.wrap_i64
                      (local.tee 7
                        (i64.clz
                          (local.get 4))))
                    (i32.const -1)))))
            (block ;; label = @5
              (br_if 0 (;@5;)
                (i64.ne
                  (local.get 7)
                  (i64.const 0)))
              (local.set 4
                (i64.or
                  (i64.and
                    (local.get 4)
                    (i64.const 1))
                  (i64.shr_u
                    (local.get 4)
                    (i64.const 1))))
              (br_if 2 (;@3;)
                (local.get 10))
              (br 1 (;@4;)))
            (local.set 4
              (i64.shl
                (local.get 4)
                (i64.extend_i32_u
                  (local.get 13))))
            (br_if 1 (;@3;)
              (local.get 10)))
          (local.set 10
            (i32.const 0))
          (local.set 7
            (local.get 4))
          (br 1 (;@2;)))
        (local.set 7
          (i64.sub
            (i64.const 0)
            (local.get 4)))
        (local.set 10
          (i32.const 1)))
      (local.set 0
        (f64.convert_i64_s
          (local.get 7)))
      (block ;; label = @2
        (block ;; label = @3
          (block ;; label = @4
            (block ;; label = @5
              (br_if 0 (;@5;)
                (i32.ge_s
                  (local.get 6)
                  (i32.const -1084)))
              (br_if 2 (;@3;)
                (i32.eq
                  (local.get 6)
                  (i32.const -1085)))
              (local.set 0
                (f64.mul
                  (f64.convert_i64_s
                    (select
                      (i64.sub
                        (i64.const 0)
                        (local.tee 4
                          (i64.or
                            (select
                              (i64.const 0)
                              (i64.const 1024)
                              (i64.eqz
                                (i64.and
                                  (local.get 4)
                                  (i64.const 1023))))
                            (i64.and
                              (local.get 4)
                              (i64.const -1024)))))
                      (local.get 4)
                      (local.get 10)))
                  (f64.const 0x1p-969 (;=0.0000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000002004168360008973;))))
              (br_if 1 (;@4;)
                (i32.le_u
                  (local.get 6)
                  (i32.const -1992)))
              (local.set 6
                (i32.add
                  (local.get 6)
                  (i32.const 969)))
              (br 3 (;@2;)))
            (block ;; label = @5
              (br_if 0 (;@5;)
                (i32.gt_s
                  (local.get 6)
                  (i32.const 1023)))
              (br_if 3 (;@2;)
                (i32.gt_s
                  (local.get 6)
                  (i32.const -1023)))
              (local.set 6
                (i32.add
                  (local.get 6)
                  (i32.const 969)))
              (local.set 0
                (f64.mul
                  (local.get 0)
                  (f64.const 0x1p-969 (;=0.0000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000002004168360008973;))))
              (br 3 (;@2;)))
            (local.set 6
              (i32.add
                (local.get 6)
                (i32.const -1023)))
            (local.set 0
              (f64.mul
                (local.get 0)
                (f64.const 0x1p+1023 (;=89884656743115800000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000;))))
            (br 2 (;@2;)))
          (local.set 0
            (f64.mul
              (local.get 0)
              (f64.const 0x1p-969 (;=0.0000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000002004168360008973;))))
          (local.set 6
            (i32.add
              (select
                (local.get 6)
                (i32.const -2960)
                (i32.gt_u
                  (local.get 6)
                  (i32.const -2960)))
              (i32.const 1938)))
          (br 1 (;@2;)))
        (block ;; label = @3
          (block ;; label = @4
            (block ;; label = @5
              (br_if 0 (;@5;)
                (f64.eq
                  (local.tee 2
                    (select
                      (f64.const -0x1p+63 (;=-9223372036854776000;))
                      (f64.const 0x1p+63 (;=9223372036854776000;))
                      (local.get 10)))
                  (local.get 0)))
              (br_if 1 (;@4;)
                (i32.eqz
                  (i64.eqz
                    (i64.and
                      (local.get 4)
                      (i64.const 2047)))))
              (br 2 (;@3;)))
            (local.set 0
              (f64.copysign
                (f64.const 0x1p-1022 (;=0.000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000022250738585072014;))
                (local.get 0)))
            (br 3 (;@1;)))
          (local.set 0
            (f64.sub
              (f64.add
                (local.tee 0
                  (f64.convert_i64_s
                    (select
                      (i64.sub
                        (i64.const 0)
                        (local.tee 4
                          (i64.or
                            (i64.or
                              (i64.and
                                (local.get 4)
                                (i64.const 1))
                              (i64.shr_u
                                (local.get 4)
                                (i64.const 1)))
                            (i64.const 4611686018427387904))))
                      (local.get 4)
                      (local.get 10))))
                (local.get 0))
              (local.get 2))))
        (local.set 0
          (f64.mul
            (local.get 0)
            (f64.const 0x1p-969 (;=0.0000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000002004168360008973;))))
        (local.set 6
          (i32.const -116)))
      (local.set 0
        (f64.mul
          (local.get 0)
          (f64.reinterpret_i64
            (i64.shl
              (i64.extend_i32_u
                (i32.and
                  (i32.add
                    (local.get 6)
                    (i32.const 1023))
                  (i32.const 2047)))
              (i64.const 52))))))
    (global.set $__stack_pointer
      (i32.add
        (local.get 3)
        (i32.const 16)))
    (local.get 0)
  )
  (func $libm_cbrtf (;24;) (type 1) (param f32) (result f32)
    (local i32 i32 f32 f64 f64 f64 f64)
    (block ;; label = @1
      (block ;; label = @2
        (br_if 0 (;@2;)
          (i32.gt_u
            (local.tee 1
              (i32.and
                (i32.reinterpret_f32
                  (local.get 0))
                (i32.const 2147483647)))
            (i32.const 2139095039)))
        (local.set 2
          (i32.const 709958130))
        (block ;; label = @3
          (block ;; label = @4
            (br_if 0 (;@4;)
              (i32.lt_u
                (local.get 1)
                (i32.const 8388608)))
            (local.set 3
              (local.get 0))
            (br 1 (;@3;)))
          (br_if 2 (;@1;)
            (i32.eqz
              (local.get 1)))
          (local.set 1
            (i32.and
              (i32.reinterpret_f32
                (local.tee 3
                  (f32.mul
                    (local.get 0)
                    (f32.const 0x1p+24 (;=16777216;)))))
              (i32.const 2147483647)))
          (local.set 2
            (i32.const 642849266)))
        (return
          (f32.demote_f64
            (f64.div
              (f64.mul
                (local.tee 6
                  (f64.div
                    (f64.mul
                      (f64.add
                        (local.tee 5
                          (f64.add
                            (local.tee 4
                              (f64.promote_f32
                                (local.get 0)))
                            (local.get 4)))
                        (local.tee 7
                          (f64.mul
                            (f64.mul
                              (local.tee 6
                                (f64.promote_f32
                                  (f32.copysign
                                    (f32.reinterpret_i32
                                      (i32.add
                                        (i32.div_u
                                          (local.get 1)
                                          (i32.const 3))
                                        (local.get 2)))
                                    (local.get 3))))
                              (local.get 6))
                            (local.get 6))))
                      (local.get 6))
                    (f64.add
                      (local.get 7)
                      (f64.add
                        (local.get 7)
                        (local.get 4)))))
                (f64.add
                  (local.get 5)
                  (local.tee 6
                    (f64.mul
                      (local.get 6)
                      (f64.mul
                        (local.get 6)
                        (local.get 6))))))
              (f64.add
                (local.get 6)
                (f64.add
                  (local.get 6)
                  (local.get 4)))))))
      (local.set 0
        (f32.add
          (local.get 0)
          (local.get 0))))
    (local.get 0)
  )
  (func $libm_cos (;25;) (type 0) (param f64) (result f64)
    (local i32 i32 f64 f64 f64)
    (global.set $__stack_pointer
      (local.tee 1
        (i32.sub
          (global.get $__stack_pointer)
          (i32.const 32))))
    (block ;; label = @1
      (block ;; label = @2
        (block ;; label = @3
          (block ;; label = @4
            (block ;; label = @5
              (block ;; label = @6
                (block ;; label = @7
                  (br_if 0 (;@7;)
                    (i32.lt_u
                      (local.tee 2
                        (i32.and
                          (i32.wrap_i64
                            (i64.shr_u
                              (i64.reinterpret_f64
                                (local.get 0))
                              (i64.const 32)))
                          (i32.const 2147483647)))
                      (i32.const 1072243196)))
                  (br_if 1 (;@6;)
                    (i32.gt_u
                      (local.get 2)
                      (i32.const 2146435071)))
                  (call $_ZN4libm4math8rem_pio28rem_pio217hcfd3034c1d4391f0E
                    (i32.add
                      (local.get 1)
                      (i32.const 8))
                    (local.get 0))
                  (local.set 3
                    (f64.load offset=24
                      (local.get 1)))
                  (local.set 0
                    (f64.load offset=8
                      (local.get 1)))
                  (br_table 3 (;@4;) 4 (;@3;) 5 (;@2;) 2 (;@5;) 3 (;@4;)
                    (i32.and
                      (i32.load offset=16
                        (local.get 1))
                      (i32.const 3))))
                (block ;; label = @7
                  (br_if 0 (;@7;)
                    (i32.trunc_sat_f64_s
                      (local.get 0)))
                  (local.set 3
                    (f64.const 0x1p+0 (;=1;)))
                  (br_if 6 (;@1;)
                    (i32.lt_u
                      (local.get 2)
                      (i32.const 1044816030))))
                (local.set 3
                  (call $_ZN4libm4math5k_cos5k_cos17h5eb36bbcae756a29E
                    (local.get 0)
                    (f64.const 0x0p+0 (;=0;))))
                (br 5 (;@1;)))
              (local.set 3
                (f64.sub
                  (local.get 0)
                  (local.get 0)))
              (br 4 (;@1;)))
            (local.set 3
              (f64.sub
                (local.get 0)
                (f64.add
                  (f64.mul
                    (local.tee 5
                      (f64.mul
                        (local.get 0)
                        (local.tee 4
                          (f64.mul
                            (local.get 0)
                            (local.get 0)))))
                    (f64.const 0x1.5555555555549p-3 (;=0.16666666666666632;)))
                  (f64.sub
                    (f64.mul
                      (local.get 4)
                      (f64.sub
                        (f64.mul
                          (local.get 3)
                          (f64.const 0x1p-1 (;=0.5;)))
                        (f64.mul
                          (local.get 5)
                          (f64.add
                            (f64.mul
                              (f64.mul
                                (local.get 4)
                                (f64.mul
                                  (local.get 4)
                                  (local.get 4)))
                              (f64.add
                                (f64.mul
                                  (local.get 4)
                                  (f64.const 0x1.5d93a5acfd57cp-33 (;=0.000000000158969099521155;)))
                                (f64.const -0x1.ae5e68a2b9cebp-26 (;=-0.000000025050760253406863;))))
                            (f64.add
                              (f64.mul
                                (local.get 4)
                                (f64.add
                                  (f64.mul
                                    (local.get 4)
                                    (f64.const 0x1.71de357b1fe7dp-19 (;=0.0000027557313707070068;)))
                                  (f64.const -0x1.a01a019c161d5p-13 (;=-0.0001984126982985795;))))
                              (f64.const 0x1.111111110f8a6p-7 (;=0.00833333333332249;)))))))
                    (local.get 3)))))
            (br 3 (;@1;)))
          (local.set 3
            (call $_ZN4libm4math5k_cos5k_cos17h5eb36bbcae756a29E
              (local.get 0)
              (local.get 3)))
          (br 2 (;@1;)))
        (local.set 3
          (f64.neg
            (f64.sub
              (local.get 0)
              (f64.add
                (f64.mul
                  (local.tee 5
                    (f64.mul
                      (local.get 0)
                      (local.tee 4
                        (f64.mul
                          (local.get 0)
                          (local.get 0)))))
                  (f64.const 0x1.5555555555549p-3 (;=0.16666666666666632;)))
                (f64.sub
                  (f64.mul
                    (local.get 4)
                    (f64.sub
                      (f64.mul
                        (local.get 3)
                        (f64.const 0x1p-1 (;=0.5;)))
                      (f64.mul
                        (local.get 5)
                        (f64.add
                          (f64.mul
                            (f64.mul
                              (local.get 4)
                              (f64.mul
                                (local.get 4)
                                (local.get 4)))
                            (f64.add
                              (f64.mul
                                (local.get 4)
                                (f64.const 0x1.5d93a5acfd57cp-33 (;=0.000000000158969099521155;)))
                              (f64.const -0x1.ae5e68a2b9cebp-26 (;=-0.000000025050760253406863;))))
                          (f64.add
                            (f64.mul
                              (local.get 4)
                              (f64.add
                                (f64.mul
                                  (local.get 4)
                                  (f64.const 0x1.71de357b1fe7dp-19 (;=0.0000027557313707070068;)))
                                (f64.const -0x1.a01a019c161d5p-13 (;=-0.0001984126982985795;))))
                            (f64.const 0x1.111111110f8a6p-7 (;=0.00833333333332249;)))))))
                  (local.get 3))))))
        (br 1 (;@1;)))
      (local.set 3
        (f64.neg
          (call $_ZN4libm4math5k_cos5k_cos17h5eb36bbcae756a29E
            (local.get 0)
            (local.get 3)))))
    (global.set $__stack_pointer
      (i32.add
        (local.get 1)
        (i32.const 32)))
    (local.get 3)
  )
  (func $_ZN4libm4math8rem_pio28rem_pio217hcfd3034c1d4391f0E (;26;) (type 5) (param i32 f64)
    (local i32 i64 i32 i32 i32 i32 i32 f64 i32 i32)
    (global.set $__stack_pointer
      (local.tee 2
        (i32.sub
          (global.get $__stack_pointer)
          (i32.const 48))))
    (block ;; label = @1
      (block ;; label = @2
        (block ;; label = @3
          (block ;; label = @4
            (block ;; label = @5
              (block ;; label = @6
                (br_if 0 (;@6;)
                  (i32.lt_u
                    (local.tee 5
                      (i32.and
                        (local.tee 4
                          (i32.wrap_i64
                            (i64.shr_u
                              (local.tee 3
                                (i64.reinterpret_f64
                                  (local.get 1)))
                              (i64.const 32))))
                        (i32.const 2147483647)))
                    (i32.const 1074752123)))
                (block ;; label = @7
                  (br_if 0 (;@7;)
                    (i32.lt_u
                      (local.get 5)
                      (i32.const 1075594812)))
                  (br_if 4 (;@3;)
                    (i32.lt_u
                      (local.get 5)
                      (i32.const 1094263291)))
                  (br_if 2 (;@5;)
                    (i32.gt_u
                      (local.get 5)
                      (i32.const 2146435071)))
                  (local.set 6
                    (i32.add
                      (local.get 2)
                      (i32.const 16)))
                  (local.set 7
                    (i32.add
                      (local.get 2)
                      (i32.const 8)))
                  (i64.store
                    (i32.add
                      (local.get 2)
                      (i32.const 8))
                    (i64.const 0))
                  (i64.store
                    (local.get 2)
                    (i64.const 0))
                  (local.set 1
                    (f64.reinterpret_i64
                      (i64.or
                        (i64.and
                          (local.get 3)
                          (i64.const 4503599627370495))
                        (i64.const 4710765210229538816))))
                  (local.set 4
                    (local.get 2))
                  (local.set 8
                    (i32.const 1))
                  (loop ;; label = @8
                    (f64.store
                      (local.get 4)
                      (local.tee 9
                        (f64.convert_i32_s
                          (i32.trunc_sat_f64_s
                            (local.get 1)))))
                    (local.set 1
                      (f64.mul
                        (f64.sub
                          (local.get 1)
                          (local.get 9))
                        (f64.const 0x1p+24 (;=16777216;))))
                    (local.set 10
                      (i32.and
                        (local.get 8)
                        (i32.const 1)))
                    (local.set 8
                      (i32.const 0))
                    (local.set 4
                      (local.get 7))
                    (br_if 0 (;@8;)
                      (local.get 10)))
                  (f64.store offset=16
                    (local.get 2)
                    (local.get 1))
                  (local.set 4
                    (i32.const 3))
                  (local.set 8
                    (i32.const 0))
                  (block ;; label = @8
                    (loop ;; label = @9
                      (local.set 11
                        (local.get 4))
                      (br_if 1 (;@8;)
                        (f64.ne
                          (local.tee 1
                            (f64.load
                              (local.get 6)))
                          (f64.const 0x0p+0 (;=0;))))
                      (local.set 4
                        (i32.const 2))
                      (local.set 10
                        (i32.and
                          (local.get 8)
                          (i32.const 1)))
                      (local.set 8
                        (i32.const 1))
                      (local.set 6
                        (local.get 7))
                      (br_if 0 (;@9;)
                        (i32.eqz
                          (local.get 10)))))
                  (i64.store
                    (i32.add
                      (local.get 2)
                      (i32.const 40))
                    (i64.const 0))
                  (i64.store
                    (i32.add
                      (local.get 2)
                      (i32.const 32))
                    (i64.const 0))
                  (i64.store offset=24
                    (local.get 2)
                    (i64.const 0))
                  (local.set 4
                    (call $_ZN4libm4math14rem_pio2_large14rem_pio2_large17hd7141cf13a36ffbcE
                      (local.get 2)
                      (select
                        (local.get 11)
                        (i32.const 1)
                        (f64.ne
                          (local.get 1)
                          (f64.const 0x0p+0 (;=0;))))
                      (i32.add
                        (local.get 2)
                        (i32.const 24))
                      (i32.add
                        (i32.shr_u
                          (local.get 5)
                          (i32.const 20))
                        (i32.const -1046))
                      (i32.const 1)))
                  (block ;; label = @8
                    (br_if 0 (;@8;)
                      (i64.lt_s
                        (local.get 3)
                        (i64.const 0)))
                    (i32.store offset=8
                      (local.get 0)
                      (local.get 4))
                    (f64.store offset=16
                      (local.get 0)
                      (f64.load offset=32
                        (local.get 2)))
                    (f64.store
                      (local.get 0)
                      (f64.load offset=24
                        (local.get 2)))
                    (br 7 (;@1;)))
                  (i32.store offset=8
                    (local.get 0)
                    (i32.sub
                      (i32.const 0)
                      (local.get 4)))
                  (f64.store offset=16
                    (local.get 0)
                    (f64.neg
                      (f64.load offset=32
                        (local.get 2))))
                  (f64.store
                    (local.get 0)
                    (f64.neg
                      (f64.load offset=24
                        (local.get 2))))
                  (br 6 (;@1;)))
                (block ;; label = @7
                  (br_if 0 (;@7;)
                    (i32.lt_u
                      (local.get 5)
                      (i32.const 1075183037)))
                  (block ;; label = @8
                    (br_if 0 (;@8;)
                      (i32.ne
                        (local.get 5)
                        (i32.const 1075388923)))
                    (call $_ZN4libm4math8rem_pio28rem_pio26medium17h661801483d1aa963E
                      (local.get 0)
                      (local.get 1)
                      (i32.const 1075388923))
                    (br 7 (;@1;)))
                  (block ;; label = @8
                    (br_if 0 (;@8;)
                      (i64.lt_s
                        (local.get 3)
                        (i64.const 0)))
                    (i32.store offset=8
                      (local.get 0)
                      (i32.const 4))
                    (f64.store
                      (local.get 0)
                      (local.tee 9
                        (f64.add
                          (local.tee 1
                            (f64.add
                              (local.get 1)
                              (f64.const -0x1.921fb544p+2 (;=-6.2831853069365025;))))
                          (f64.const -0x1.0b4611a626331p-32 (;=-0.0000000002430840202602477;)))))
                    (f64.store offset=16
                      (local.get 0)
                      (f64.add
                        (f64.sub
                          (local.get 1)
                          (local.get 9))
                        (f64.const -0x1.0b4611a626331p-32 (;=-0.0000000002430840202602477;))))
                    (br 7 (;@1;)))
                  (i32.store offset=8
                    (local.get 0)
                    (i32.const -4))
                  (f64.store
                    (local.get 0)
                    (local.tee 9
                      (f64.add
                        (local.tee 1
                          (f64.add
                            (local.get 1)
                            (f64.const 0x1.921fb544p+2 (;=6.2831853069365025;))))
                        (f64.const 0x1.0b4611a626331p-32 (;=0.0000000002430840202602477;)))))
                  (f64.store offset=16
                    (local.get 0)
                    (f64.add
                      (f64.sub
                        (local.get 1)
                        (local.get 9))
                      (f64.const 0x1.0b4611a626331p-32 (;=0.0000000002430840202602477;))))
                  (br 6 (;@1;)))
                (br_if 4 (;@2;)
                  (i32.eq
                    (local.get 5)
                    (i32.const 1074977148)))
                (block ;; label = @7
                  (br_if 0 (;@7;)
                    (i64.lt_s
                      (local.get 3)
                      (i64.const 0)))
                  (i32.store offset=8
                    (local.get 0)
                    (i32.const 3))
                  (f64.store
                    (local.get 0)
                    (local.tee 9
                      (f64.add
                        (local.tee 1
                          (f64.add
                            (local.get 1)
                            (f64.const -0x1.2d97c7f3p+2 (;=-4.712388980202377;))))
                        (f64.const -0x1.90e91a79394cap-33 (;=-0.00000000018231301519518578;)))))
                  (f64.store offset=16
                    (local.get 0)
                    (f64.add
                      (f64.sub
                        (local.get 1)
                        (local.get 9))
                      (f64.const -0x1.90e91a79394cap-33 (;=-0.00000000018231301519518578;))))
                  (br 6 (;@1;)))
                (i32.store offset=8
                  (local.get 0)
                  (i32.const -3))
                (f64.store
                  (local.get 0)
                  (local.tee 9
                    (f64.add
                      (local.tee 1
                        (f64.add
                          (local.get 1)
                          (f64.const 0x1.2d97c7f3p+2 (;=4.712388980202377;))))
                      (f64.const 0x1.90e91a79394cap-33 (;=0.00000000018231301519518578;)))))
                (f64.store offset=16
                  (local.get 0)
                  (f64.add
                    (f64.sub
                      (local.get 1)
                      (local.get 9))
                    (f64.const 0x1.90e91a79394cap-33 (;=0.00000000018231301519518578;))))
                (br 5 (;@1;)))
              (br_if 2 (;@3;)
                (i32.eq
                  (i32.and
                    (local.get 4)
                    (i32.const 1048575))
                  (i32.const 598523)))
              (block ;; label = @6
                (br_if 0 (;@6;)
                  (i32.lt_u
                    (local.get 5)
                    (i32.const 1073928573)))
                (block ;; label = @7
                  (br_if 0 (;@7;)
                    (i64.le_s
                      (local.get 3)
                      (i64.const -1)))
                  (i32.store offset=8
                    (local.get 0)
                    (i32.const 2))
                  (f64.store
                    (local.get 0)
                    (local.tee 9
                      (f64.add
                        (local.tee 1
                          (f64.add
                            (local.get 1)
                            (f64.const -0x1.921fb544p+1 (;=-3.1415926534682512;))))
                        (f64.const -0x1.0b4611a626331p-33 (;=-0.00000000012154201013012384;)))))
                  (f64.store offset=16
                    (local.get 0)
                    (f64.add
                      (f64.sub
                        (local.get 1)
                        (local.get 9))
                      (f64.const -0x1.0b4611a626331p-33 (;=-0.00000000012154201013012384;))))
                  (br 6 (;@1;)))
                (i32.store offset=8
                  (local.get 0)
                  (i32.const -2))
                (f64.store
                  (local.get 0)
                  (local.tee 9
                    (f64.add
                      (local.tee 1
                        (f64.add
                          (local.get 1)
                          (f64.const 0x1.921fb544p+1 (;=3.1415926534682512;))))
                      (f64.const 0x1.0b4611a626331p-33 (;=0.00000000012154201013012384;)))))
                (f64.store offset=16
                  (local.get 0)
                  (f64.add
                    (f64.sub
                      (local.get 1)
                      (local.get 9))
                    (f64.const 0x1.0b4611a626331p-33 (;=0.00000000012154201013012384;))))
                (br 5 (;@1;)))
              (br_if 1 (;@4;)
                (i64.gt_s
                  (local.get 3)
                  (i64.const -1)))
              (i32.store offset=8
                (local.get 0)
                (i32.const -1))
              (f64.store
                (local.get 0)
                (local.tee 9
                  (f64.add
                    (local.tee 1
                      (f64.add
                        (local.get 1)
                        (f64.const 0x1.921fb544p+0 (;=1.5707963267341256;))))
                    (f64.const 0x1.0b4611a626331p-34 (;=0.00000000006077100506506192;)))))
              (f64.store offset=16
                (local.get 0)
                (f64.add
                  (f64.sub
                    (local.get 1)
                    (local.get 9))
                  (f64.const 0x1.0b4611a626331p-34 (;=0.00000000006077100506506192;))))
              (br 4 (;@1;)))
            (i32.store offset=8
              (local.get 0)
              (i32.const 0))
            (f64.store offset=16
              (local.get 0)
              (local.tee 1
                (f64.sub
                  (local.get 1)
                  (local.get 1))))
            (f64.store
              (local.get 0)
              (local.get 1))
            (br 3 (;@1;)))
          (i32.store offset=8
            (local.get 0)
            (i32.const 1))
          (f64.store
            (local.get 0)
            (local.tee 9
              (f64.add
                (local.tee 1
                  (f64.add
                    (local.get 1)
                    (f64.const -0x1.921fb544p+0 (;=-1.5707963267341256;))))
                (f64.const -0x1.0b4611a626331p-34 (;=-0.00000000006077100506506192;)))))
          (f64.store offset=16
            (local.get 0)
            (f64.add
              (f64.sub
                (local.get 1)
                (local.get 9))
              (f64.const -0x1.0b4611a626331p-34 (;=-0.00000000006077100506506192;))))
          (br 2 (;@1;)))
        (call $_ZN4libm4math8rem_pio28rem_pio26medium17h661801483d1aa963E
          (local.get 0)
          (local.get 1)
          (local.get 5))
        (br 1 (;@1;)))
      (call $_ZN4libm4math8rem_pio28rem_pio26medium17h661801483d1aa963E
        (local.get 0)
        (local.get 1)
        (i32.const 1074977148)))
    (global.set $__stack_pointer
      (i32.add
        (local.get 2)
        (i32.const 48)))
  )
  (func $_ZN4libm4math5k_cos5k_cos17h5eb36bbcae756a29E (;27;) (type 2) (param f64 f64) (result f64)
    (local f64 f64 f64)
    (f64.add
      (local.tee 4
        (f64.sub
          (f64.const 0x1p+0 (;=1;))
          (local.tee 3
            (f64.mul
              (local.tee 2
                (f64.mul
                  (local.get 0)
                  (local.get 0)))
              (f64.const 0x1p-1 (;=0.5;))))))
      (f64.add
        (f64.sub
          (f64.sub
            (f64.const 0x1p+0 (;=1;))
            (local.get 4))
          (local.get 3))
        (f64.sub
          (f64.mul
            (local.get 2)
            (f64.add
              (f64.mul
                (local.get 2)
                (f64.add
                  (f64.mul
                    (local.get 2)
                    (f64.add
                      (f64.mul
                        (local.get 2)
                        (f64.const 0x1.a01a019cb159p-16 (;=0.00002480158728947673;)))
                      (f64.const -0x1.6c16c16c15177p-10 (;=-0.001388888888887411;))))
                  (f64.const 0x1.555555555554cp-5 (;=0.0416666666666666;))))
              (f64.mul
                (f64.mul
                  (local.tee 3
                    (f64.mul
                      (local.get 2)
                      (local.get 2)))
                  (local.get 3))
                (f64.add
                  (f64.mul
                    (local.get 2)
                    (f64.add
                      (f64.mul
                        (local.get 2)
                        (f64.const -0x1.8fae9be8838d4p-37 (;=-0.000000000011359647557788195;)))
                      (f64.const 0x1.1ee9ebdb4b1c4p-29 (;=0.000000002087572321298175;))))
                  (f64.const -0x1.27e4f809c52adp-22 (;=-0.00000027557314351390663;))))))
          (f64.mul
            (local.get 0)
            (local.get 1)))))
  )
  (func $libm_cosf (;28;) (type 1) (param f32) (result f32)
    (local i32 f64 i32 i32 f64 f64)
    (global.set $__stack_pointer
      (local.tee 1
        (i32.sub
          (global.get $__stack_pointer)
          (i32.const 16))))
    (local.set 2
      (f64.promote_f32
        (local.get 0)))
    (block ;; label = @1
      (block ;; label = @2
        (block ;; label = @3
          (block ;; label = @4
            (br_if 0 (;@4;)
              (i32.lt_u
                (local.tee 4
                  (i32.and
                    (local.tee 3
                      (i32.reinterpret_f32
                        (local.get 0)))
                    (i32.const 2147483647)))
                (i32.const 1061752795)))
            (block ;; label = @5
              (br_if 0 (;@5;)
                (i32.lt_u
                  (local.get 4)
                  (i32.const 1081824210)))
              (block ;; label = @6
                (br_if 0 (;@6;)
                  (i32.lt_u
                    (local.get 4)
                    (i32.const 1088565718)))
                (block ;; label = @7
                  (block ;; label = @8
                    (block ;; label = @9
                      (block ;; label = @10
                        (block ;; label = @11
                          (br_if 0 (;@11;)
                            (i32.gt_u
                              (local.get 4)
                              (i32.const 2139095039)))
                          (call $_ZN4libm4math9rem_pio2f9rem_pio2f17h3b6adc3e5afb8880E
                            (local.get 1)
                            (local.get 0))
                          (local.set 2
                            (f64.load offset=8
                              (local.get 1)))
                          (br_table 2 (;@9;) 3 (;@8;) 4 (;@7;) 1 (;@10;) 2 (;@9;)
                            (i32.and
                              (i32.load
                                (local.get 1))
                              (i32.const 3))))
                        (local.set 0
                          (f32.sub
                            (local.get 0)
                            (local.get 0)))
                        (br 9 (;@1;)))
                      (local.set 0
                        (f32.demote_f64
                          (f64.add
                            (f64.mul
                              (f64.mul
                                (local.tee 6
                                  (f64.mul
                                    (local.get 2)
                                    (local.tee 5
                                      (f64.mul
                                        (local.get 2)
                                        (local.get 2)))))
                                (f64.mul
                                  (local.get 5)
                                  (local.get 5)))
                              (f64.add
                                (f64.mul
                                  (local.get 5)
                                  (f64.const 0x1.6cd878c3b46a7p-19 (;=0.000002718311493989822;)))
                                (f64.const -0x1.a00f9e2cae774p-13 (;=-0.00019839334836096632;))))
                            (f64.add
                              (local.get 2)
                              (f64.mul
                                (local.get 6)
                                (f64.add
                                  (f64.mul
                                    (local.get 5)
                                    (f64.const 0x1.11110896efbb2p-7 (;=0.008333329385889463;)))
                                  (f64.const -0x1.5555554cbac77p-3 (;=-0.16666666641626524;))))))))
                      (br 8 (;@1;)))
                    (local.set 0
                      (f32.demote_f64
                        (f64.add
                          (f64.add
                            (f64.add
                              (f64.mul
                                (local.tee 2
                                  (f64.mul
                                    (local.get 2)
                                    (local.get 2)))
                                (f64.const -0x1.ffffffd0c5e81p-2 (;=-0.499999997251031;)))
                              (f64.const 0x1p+0 (;=1;)))
                            (f64.mul
                              (local.tee 5
                                (f64.mul
                                  (local.get 2)
                                  (local.get 2)))
                              (f64.const 0x1.55553e1053a42p-5 (;=0.04166662332373906;))))
                          (f64.mul
                            (f64.mul
                              (local.get 2)
                              (local.get 5))
                            (f64.add
                              (f64.mul
                                (local.get 2)
                                (f64.const 0x1.99342e0ee5069p-16 (;=0.00002439044879627741;)))
                              (f64.const -0x1.6c087e80f1e27p-10 (;=-0.001388676377460993;)))))))
                    (br 7 (;@1;)))
                  (local.set 0
                    (f32.demote_f64
                      (f64.add
                        (f64.mul
                          (f64.mul
                            (local.tee 6
                              (f64.mul
                                (local.tee 5
                                  (f64.mul
                                    (local.get 2)
                                    (local.get 2)))
                                (f64.neg
                                  (local.get 2))))
                            (f64.mul
                              (local.get 5)
                              (local.get 5)))
                          (f64.add
                            (f64.mul
                              (local.get 5)
                              (f64.const 0x1.6cd878c3b46a7p-19 (;=0.000002718311493989822;)))
                            (f64.const -0x1.a00f9e2cae774p-13 (;=-0.00019839334836096632;))))
                        (f64.sub
                          (f64.mul
                            (local.get 6)
                            (f64.add
                              (f64.mul
                                (local.get 5)
                                (f64.const 0x1.11110896efbb2p-7 (;=0.008333329385889463;)))
                              (f64.const -0x1.5555554cbac77p-3 (;=-0.16666666641626524;))))
                          (local.get 2)))))
                  (br 6 (;@1;)))
                (local.set 0
                  (f32.neg
                    (f32.demote_f64
                      (f64.add
                        (f64.add
                          (f64.add
                            (f64.mul
                              (local.tee 2
                                (f64.mul
                                  (local.get 2)
                                  (local.get 2)))
                              (f64.const -0x1.ffffffd0c5e81p-2 (;=-0.499999997251031;)))
                            (f64.const 0x1p+0 (;=1;)))
                          (f64.mul
                            (local.tee 5
                              (f64.mul
                                (local.get 2)
                                (local.get 2)))
                            (f64.const 0x1.55553e1053a42p-5 (;=0.04166662332373906;))))
                        (f64.mul
                          (f64.mul
                            (local.get 2)
                            (local.get 5))
                          (f64.add
                            (f64.mul
                              (local.get 2)
                              (f64.const 0x1.99342e0ee5069p-16 (;=0.00002439044879627741;)))
                            (f64.const -0x1.6c087e80f1e27p-10 (;=-0.001388676377460993;))))))))
                (br 5 (;@1;)))
              (br_if 2 (;@3;)
                (i32.gt_u
                  (local.get 4)
                  (i32.const 1085271519)))
              (block ;; label = @6
                (br_if 0 (;@6;)
                  (i32.le_s
                    (local.get 3)
                    (i32.const -1)))
                (local.set 0
                  (f32.demote_f64
                    (f64.add
                      (f64.mul
                        (f64.mul
                          (local.tee 6
                            (f64.mul
                              (local.tee 5
                                (f64.add
                                  (local.get 2)
                                  (f64.const -0x1.2d97c7f3321d2p+2 (;=-4.71238898038469;))))
                              (local.tee 2
                                (f64.mul
                                  (local.get 5)
                                  (local.get 5)))))
                          (f64.mul
                            (local.get 2)
                            (local.get 2)))
                        (f64.add
                          (f64.mul
                            (local.get 2)
                            (f64.const 0x1.6cd878c3b46a7p-19 (;=0.000002718311493989822;)))
                          (f64.const -0x1.a00f9e2cae774p-13 (;=-0.00019839334836096632;))))
                      (f64.add
                        (local.get 5)
                        (f64.mul
                          (local.get 6)
                          (f64.add
                            (f64.mul
                              (local.get 2)
                              (f64.const 0x1.11110896efbb2p-7 (;=0.008333329385889463;)))
                            (f64.const -0x1.5555554cbac77p-3 (;=-0.16666666641626524;))))))))
                (br 5 (;@1;)))
              (local.set 0
                (f32.demote_f64
                  (f64.add
                    (f64.mul
                      (f64.mul
                        (local.tee 6
                          (f64.mul
                            (local.tee 5
                              (f64.sub
                                (f64.const -0x1.2d97c7f3321d2p+2 (;=-4.71238898038469;))
                                (local.get 2)))
                            (local.tee 2
                              (f64.mul
                                (local.get 5)
                                (local.get 5)))))
                        (f64.mul
                          (local.get 2)
                          (local.get 2)))
                      (f64.add
                        (f64.mul
                          (local.get 2)
                          (f64.const 0x1.6cd878c3b46a7p-19 (;=0.000002718311493989822;)))
                        (f64.const -0x1.a00f9e2cae774p-13 (;=-0.00019839334836096632;))))
                    (f64.add
                      (local.get 5)
                      (f64.mul
                        (local.get 6)
                        (f64.add
                          (f64.mul
                            (local.get 2)
                            (f64.const 0x1.11110896efbb2p-7 (;=0.008333329385889463;)))
                          (f64.const -0x1.5555554cbac77p-3 (;=-0.16666666641626524;))))))))
              (br 4 (;@1;)))
            (br_if 2 (;@2;)
              (i32.gt_u
                (local.get 4)
                (i32.const 1075235811)))
            (block ;; label = @5
              (br_if 0 (;@5;)
                (i32.le_s
                  (local.get 3)
                  (i32.const -1)))
              (local.set 0
                (f32.demote_f64
                  (f64.add
                    (f64.mul
                      (f64.mul
                        (local.tee 6
                          (f64.mul
                            (local.tee 5
                              (f64.sub
                                (f64.const 0x1.921fb54442d18p+0 (;=1.5707963267948966;))
                                (local.get 2)))
                            (local.tee 2
                              (f64.mul
                                (local.get 5)
                                (local.get 5)))))
                        (f64.mul
                          (local.get 2)
                          (local.get 2)))
                      (f64.add
                        (f64.mul
                          (local.get 2)
                          (f64.const 0x1.6cd878c3b46a7p-19 (;=0.000002718311493989822;)))
                        (f64.const -0x1.a00f9e2cae774p-13 (;=-0.00019839334836096632;))))
                    (f64.add
                      (local.get 5)
                      (f64.mul
                        (local.get 6)
                        (f64.add
                          (f64.mul
                            (local.get 2)
                            (f64.const 0x1.11110896efbb2p-7 (;=0.008333329385889463;)))
                          (f64.const -0x1.5555554cbac77p-3 (;=-0.16666666641626524;))))))))
              (br 4 (;@1;)))
            (local.set 0
              (f32.demote_f64
                (f64.add
                  (f64.mul
                    (f64.mul
                      (local.tee 6
                        (f64.mul
                          (local.tee 5
                            (f64.add
                              (local.get 2)
                              (f64.const 0x1.921fb54442d18p+0 (;=1.5707963267948966;))))
                          (local.tee 2
                            (f64.mul
                              (local.get 5)
                              (local.get 5)))))
                      (f64.mul
                        (local.get 2)
                        (local.get 2)))
                    (f64.add
                      (f64.mul
                        (local.get 2)
                        (f64.const 0x1.6cd878c3b46a7p-19 (;=0.000002718311493989822;)))
                      (f64.const -0x1.a00f9e2cae774p-13 (;=-0.00019839334836096632;))))
                  (f64.add
                    (local.get 5)
                    (f64.mul
                      (local.get 6)
                      (f64.add
                        (f64.mul
                          (local.get 2)
                          (f64.const 0x1.11110896efbb2p-7 (;=0.008333329385889463;)))
                        (f64.const -0x1.5555554cbac77p-3 (;=-0.16666666641626524;))))))))
            (br 3 (;@1;)))
          (block ;; label = @4
            (br_if 0 (;@4;)
              (i32.lt_u
                (local.get 4)
                (i32.const 964689920)))
            (local.set 0
              (f32.demote_f64
                (f64.add
                  (f64.add
                    (f64.add
                      (f64.mul
                        (local.tee 2
                          (f64.mul
                            (local.get 2)
                            (local.get 2)))
                        (f64.const -0x1.ffffffd0c5e81p-2 (;=-0.499999997251031;)))
                      (f64.const 0x1p+0 (;=1;)))
                    (f64.mul
                      (local.tee 5
                        (f64.mul
                          (local.get 2)
                          (local.get 2)))
                      (f64.const 0x1.55553e1053a42p-5 (;=0.04166662332373906;))))
                  (f64.mul
                    (f64.mul
                      (local.get 2)
                      (local.get 5))
                    (f64.add
                      (f64.mul
                        (local.get 2)
                        (f64.const 0x1.99342e0ee5069p-16 (;=0.00002439044879627741;)))
                      (f64.const -0x1.6c087e80f1e27p-10 (;=-0.001388676377460993;)))))))
            (br 3 (;@1;)))
          (f32.store
            (local.get 1)
            (f32.add
              (local.get 0)
              (f32.const 0x1p+120 (;=1329228000000000000000000000000000000;))))
          (drop
            (f32.load
              (local.get 1)))
          (local.set 0
            (f32.const 0x1p+0 (;=1;)))
          (br 2 (;@1;)))
        (local.set 0
          (f32.demote_f64
            (f64.add
              (f64.add
                (f64.add
                  (f64.mul
                    (local.tee 2
                      (f64.mul
                        (local.tee 2
                          (f64.add
                            (select
                              (f64.const -0x1.921fb54442d18p+2 (;=-6.283185307179586;))
                              (f64.const 0x1.921fb54442d18p+2 (;=6.283185307179586;))
                              (i32.gt_s
                                (local.get 3)
                                (i32.const -1)))
                            (local.get 2)))
                        (local.get 2)))
                    (f64.const -0x1.ffffffd0c5e81p-2 (;=-0.499999997251031;)))
                  (f64.const 0x1p+0 (;=1;)))
                (f64.mul
                  (local.tee 5
                    (f64.mul
                      (local.get 2)
                      (local.get 2)))
                  (f64.const 0x1.55553e1053a42p-5 (;=0.04166662332373906;))))
              (f64.mul
                (f64.mul
                  (local.get 2)
                  (local.get 5))
                (f64.add
                  (f64.mul
                    (local.get 2)
                    (f64.const 0x1.99342e0ee5069p-16 (;=0.00002439044879627741;)))
                  (f64.const -0x1.6c087e80f1e27p-10 (;=-0.001388676377460993;)))))))
        (br 1 (;@1;)))
      (local.set 0
        (f32.neg
          (f32.demote_f64
            (f64.add
              (f64.add
                (f64.add
                  (f64.mul
                    (local.tee 2
                      (f64.mul
                        (local.tee 2
                          (f64.add
                            (select
                              (f64.const -0x1.921fb54442d18p+1 (;=-3.141592653589793;))
                              (f64.const 0x1.921fb54442d18p+1 (;=3.141592653589793;))
                              (i32.gt_s
                                (local.get 3)
                                (i32.const -1)))
                            (local.get 2)))
                        (local.get 2)))
                    (f64.const -0x1.ffffffd0c5e81p-2 (;=-0.499999997251031;)))
                  (f64.const 0x1p+0 (;=1;)))
                (f64.mul
                  (local.tee 5
                    (f64.mul
                      (local.get 2)
                      (local.get 2)))
                  (f64.const 0x1.55553e1053a42p-5 (;=0.04166662332373906;))))
              (f64.mul
                (f64.mul
                  (local.get 2)
                  (local.get 5))
                (f64.add
                  (f64.mul
                    (local.get 2)
                    (f64.const 0x1.99342e0ee5069p-16 (;=0.00002439044879627741;)))
                  (f64.const -0x1.6c087e80f1e27p-10 (;=-0.001388676377460993;)))))))))
    (global.set $__stack_pointer
      (i32.add
        (local.get 1)
        (i32.const 16)))
    (local.get 0)
  )
  (func $_ZN4libm4math9rem_pio2f9rem_pio2f17h3b6adc3e5afb8880E (;29;) (type 6) (param i32 f32)
    (local i32 f64 i32 i32 i32 f64)
    (global.set $__stack_pointer
      (local.tee 2
        (i32.sub
          (global.get $__stack_pointer)
          (i32.const 16))))
    (i64.store offset=8
      (local.get 2)
      (i64.const 0))
    (local.set 3
      (f64.promote_f32
        (local.get 1)))
    (block ;; label = @1
      (block ;; label = @2
        (block ;; label = @3
          (br_if 0 (;@3;)
            (i32.lt_u
              (local.tee 5
                (i32.and
                  (local.tee 4
                    (i32.reinterpret_f32
                      (local.get 1)))
                  (i32.const 2147483647)))
              (i32.const 1305022427)))
          (br_if 1 (;@2;)
            (i32.gt_u
              (local.get 5)
              (i32.const 2139095039)))
          (f64.store
            (local.get 2)
            (f64.promote_f32
              (f32.reinterpret_i32
                (i32.sub
                  (local.get 5)
                  (i32.shl
                    (local.tee 6
                      (i32.add
                        (i32.shr_u
                          (local.get 5)
                          (i32.const 23))
                        (i32.const -150)))
                    (i32.const 23))))))
          (local.set 5
            (call $_ZN4libm4math14rem_pio2_large14rem_pio2_large17hd7141cf13a36ffbcE
              (local.get 2)
              (i32.const 1)
              (i32.add
                (local.get 2)
                (i32.const 8))
              (local.get 6)
              (i32.const 0)))
          (block ;; label = @4
            (br_if 0 (;@4;)
              (i32.le_s
                (local.get 4)
                (i32.const -1)))
            (local.set 3
              (f64.load offset=8
                (local.get 2)))
            (br 3 (;@1;)))
          (local.set 5
            (i32.sub
              (i32.const 0)
              (local.get 5)))
          (local.set 3
            (f64.neg
              (f64.load offset=8
                (local.get 2))))
          (br 2 (;@1;)))
        (local.set 3
          (f64.add
            (f64.add
              (local.get 3)
              (f64.mul
                (local.tee 7
                  (f64.add
                    (f64.add
                      (f64.mul
                        (local.get 3)
                        (f64.const 0x1.45f306dc9c883p-1 (;=0.6366197723675814;)))
                      (f64.const 0x1.8p+52 (;=6755399441055744;)))
                    (f64.const -0x1.8p+52 (;=-6755399441055744;))))
                (f64.const -0x1.921fb5p+0 (;=-1.5707963109016418;))))
            (f64.mul
              (local.get 7)
              (f64.const -0x1.110b4611a6263p-26 (;=-0.000000015893254773528196;)))))
        (local.set 5
          (i32.trunc_sat_f64_s
            (local.get 7)))
        (br 1 (;@1;)))
      (local.set 3
        (f64.sub
          (local.get 3)
          (local.get 3)))
      (local.set 5
        (i32.const 0)))
    (f64.store offset=8
      (local.get 0)
      (local.get 3))
    (i32.store
      (local.get 0)
      (local.get 5))
    (global.set $__stack_pointer
      (i32.add
        (local.get 2)
        (i32.const 16)))
  )
  (func $libm_cosh (;30;) (type 0) (param f64) (result f64)
    (local i32 i64)
    (global.set $__stack_pointer
      (local.tee 1
        (i32.sub
          (global.get $__stack_pointer)
          (i32.const 16))))
    (block ;; label = @1
      (block ;; label = @2
        (br_if 0 (;@2;)
          (i64.lt_u
            (local.tee 2
              (i64.reinterpret_f64
                (local.tee 0
                  (f64.abs
                    (local.get 0)))))
            (i64.const 4604418530035630080)))
        (block ;; label = @3
          (br_if 0 (;@3;)
            (i64.lt_u
              (local.get 2)
              (i64.const 4649454526309335040)))
          (local.set 0
            (f64.mul
              (f64.mul
                (call $_ZN4libm4math3exp3exp17h8eb8b2450c3bf8abE
                  (f64.add
                    (local.get 0)
                    (f64.const -0x1.62066151add8bp+10 (;=-1416.0996898839683;))))
                (f64.const 0x1p+1021 (;=22471164185778950000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000;)))
              (f64.const 0x1p+1021 (;=22471164185778950000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000;))))
          (br 2 (;@1;)))
        (local.set 0
          (f64.mul
            (f64.add
              (local.tee 0
                (call $_ZN4libm4math3exp3exp17h8eb8b2450c3bf8abE
                  (local.get 0)))
              (f64.div
                (f64.const 0x1p+0 (;=1;))
                (local.get 0)))
            (f64.const 0x1p-1 (;=0.5;))))
        (br 1 (;@1;)))
      (block ;; label = @2
        (br_if 0 (;@2;)
          (i64.lt_u
            (local.get 2)
            (i64.const 4490088828488384512)))
        (local.set 0
          (f64.add
            (f64.div
              (f64.mul
                (local.tee 0
                  (call $_ZN4libm4math5expm15expm117hf425b3a732f15702E
                    (local.get 0)))
                (local.get 0))
              (f64.add
                (local.tee 0
                  (f64.add
                    (local.get 0)
                    (f64.const 0x1p+0 (;=1;))))
                (local.get 0)))
            (f64.const 0x1p+0 (;=1;))))
        (br 1 (;@1;)))
      (f64.store offset=8
        (local.get 1)
        (f64.add
          (local.get 0)
          (f64.const 0x1p+120 (;=1329227995784916000000000000000000000;))))
      (drop
        (f64.load offset=8
          (local.get 1)))
      (local.set 0
        (f64.const 0x1p+0 (;=1;))))
    (global.set $__stack_pointer
      (i32.add
        (local.get 1)
        (i32.const 16)))
    (local.get 0)
  )
  (func $_ZN4libm4math3exp3exp17h8eb8b2450c3bf8abE (;31;) (type 0) (param f64) (result f64)
    (local i32 i64 i32 i32 f64 f64 f64)
    (global.set $__stack_pointer
      (local.tee 1
        (i32.sub
          (global.get $__stack_pointer)
          (i32.const 16))))
    (local.set 3
      (i32.wrap_i64
        (i64.shr_u
          (local.tee 2
            (i64.reinterpret_f64
              (local.get 0)))
          (i64.const 63))))
    (block ;; label = @1
      (block ;; label = @2
        (block ;; label = @3
          (block ;; label = @4
            (block ;; label = @5
              (block ;; label = @6
                (block ;; label = @7
                  (block ;; label = @8
                    (br_if 0 (;@8;)
                      (i32.lt_u
                        (local.tee 4
                          (i32.and
                            (i32.wrap_i64
                              (i64.shr_u
                                (local.get 2)
                                (i64.const 32)))
                            (i32.const 2147483647)))
                        (i32.const 1082532651)))
                    (block ;; label = @9
                      (br_if 0 (;@9;)
                        (f64.eq
                          (local.get 0)
                          (local.get 0)))
                      (local.set 5
                        (local.get 0))
                      (br 8 (;@1;)))
                    (br_if 2 (;@6;)
                      (f64.gt
                        (local.get 0)
                        (f64.const 0x1.62e42fefa39efp+9 (;=709.782712893384;))))
                    (br_if 1 (;@7;)
                      (i32.eqz
                        (f64.lt
                          (local.get 0)
                          (f64.const -0x1.6232bdd7abcd2p+9 (;=-708.3964185322641;)))))
                    (f32.store offset=4
                      (local.get 1)
                      (f32.demote_f64
                        (f64.div
                          (f64.const -0x1p-149 (;=-0.000000000000000000000000000000000000000000001401298464324817;))
                          (local.get 0))))
                    (drop
                      (f32.load offset=4
                        (local.get 1)))
                    (local.set 5
                      (f64.const 0x0p+0 (;=0;)))
                    (br_if 1 (;@7;)
                      (i32.eqz
                        (f64.lt
                          (local.get 0)
                          (f64.const -0x1.74910d52d3051p+9 (;=-745.1332191019411;)))))
                    (br 7 (;@1;)))
                  (block ;; label = @8
                    (br_if 0 (;@8;)
                      (i32.gt_u
                        (local.get 4)
                        (i32.const 1071001154)))
                    (br_if 3 (;@5;)
                      (i32.le_u
                        (local.get 4)
                        (i32.const 1043333120)))
                    (local.set 6
                      (f64.const 0x0p+0 (;=0;)))
                    (local.set 4
                      (i32.const 0))
                    (local.set 5
                      (local.get 0))
                    (br 6 (;@2;)))
                  (br_if 3 (;@4;)
                    (i32.le_u
                      (local.get 4)
                      (i32.const 1072734897))))
                (local.set 4
                  (i32.trunc_sat_f64_s
                    (f64.add
                      (f64.mul
                        (local.get 0)
                        (f64.const 0x1.71547652b82fep+0 (;=1.4426950408889634;)))
                      (f64.load offset=1053600
                        (i32.shl
                          (local.get 3)
                          (i32.const 3))))))
                (br 3 (;@3;)))
              (local.set 5
                (f64.mul
                  (local.get 0)
                  (f64.const 0x1p+1023 (;=89884656743115800000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000;))))
              (br 4 (;@1;)))
            (f64.store offset=8
              (local.get 1)
              (f64.add
                (local.get 0)
                (f64.const 0x1p+1023 (;=89884656743115800000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000;))))
            (local.set 5
              (f64.add
                (local.get 0)
                (f64.const 0x1p+0 (;=1;))))
            (drop
              (f64.load offset=8
                (local.get 1)))
            (br 3 (;@1;)))
          (local.set 4
            (i32.sub
              (i32.xor
                (local.get 3)
                (i32.const 1))
              (local.get 3))))
        (local.set 5
          (f64.sub
            (local.tee 0
              (f64.add
                (local.get 0)
                (f64.mul
                  (local.tee 5
                    (f64.convert_i32_s
                      (local.get 4)))
                  (f64.const -0x1.62e42feep-1 (;=-0.6931471803691238;)))))
            (local.tee 6
              (f64.mul
                (local.get 5)
                (f64.const 0x1.a39ef35793c76p-33 (;=0.00000000019082149292705877;)))))))
      (local.set 5
        (f64.add
          (f64.add
            (local.get 0)
            (f64.sub
              (f64.div
                (f64.mul
                  (local.get 5)
                  (local.tee 7
                    (f64.sub
                      (local.get 5)
                      (f64.mul
                        (local.tee 7
                          (f64.mul
                            (local.get 5)
                            (local.get 5)))
                        (f64.add
                          (f64.mul
                            (local.get 7)
                            (f64.add
                              (f64.mul
                                (local.get 7)
                                (f64.add
                                  (f64.mul
                                    (local.get 7)
                                    (f64.add
                                      (f64.mul
                                        (local.get 7)
                                        (f64.const 0x1.6376972bea4dp-25 (;=0.000000041381367970572385;)))
                                      (f64.const -0x1.bbd41c5d26bf1p-20 (;=-0.0000016533902205465252;))))
                                  (f64.const 0x1.1566aaf25de2cp-14 (;=0.00006613756321437934;))))
                              (f64.const -0x1.6c16c16bebd93p-9 (;=-0.0027777777777015593;))))
                          (f64.const 0x1.555555555553ep-3 (;=0.16666666666666602;)))))))
                (f64.sub
                  (f64.const 0x1p+1 (;=2;))
                  (local.get 7)))
              (local.get 6)))
          (f64.const 0x1p+0 (;=1;))))
      (br_if 0 (;@1;)
        (i32.eqz
          (local.get 4)))
      (local.set 5
        (call $_ZN4libm4math6scalbn6scalbn17hd5ee51b98c77623bE
          (local.get 5)
          (local.get 4))))
    (global.set $__stack_pointer
      (i32.add
        (local.get 1)
        (i32.const 16)))
    (local.get 5)
  )
  (func $_ZN4libm4math5expm15expm117hf425b3a732f15702E (;32;) (type 0) (param f64) (result f64)
    (local i32 i64 i32 f64 f64 f64 f64)
    (f64.store offset=8
      (local.tee 1
        (i32.sub
          (global.get $__stack_pointer)
          (i32.const 16)))
      (local.get 0))
    (block ;; label = @1
      (block ;; label = @2
        (block ;; label = @3
          (block ;; label = @4
            (block ;; label = @5
              (block ;; label = @6
                (block ;; label = @7
                  (block ;; label = @8
                    (br_if 0 (;@8;)
                      (i32.gt_u
                        (local.tee 3
                          (i32.and
                            (i32.wrap_i64
                              (i64.shr_u
                                (local.tee 2
                                  (i64.reinterpret_f64
                                    (local.get 0)))
                                (i64.const 32)))
                            (i32.const 2147483647)))
                        (i32.const 1078159481)))
                    (br_if 2 (;@6;)
                      (i32.gt_u
                        (local.get 3)
                        (i32.const 1071001154)))
                    (br_if 1 (;@7;)
                      (i32.lt_u
                        (local.get 3)
                        (i32.const 1016070144)))
                    (local.set 4
                      (f64.const 0x0p+0 (;=0;)))
                    (local.set 3
                      (i32.const 0))
                    (br 6 (;@2;)))
                  (br_if 6 (;@1;)
                    (f64.ne
                      (local.get 0)
                      (local.get 0)))
                  (block ;; label = @8
                    (br_if 0 (;@8;)
                      (i64.ge_s
                        (local.get 2)
                        (i64.const 0)))
                    (return
                      (f64.const -0x1p+0 (;=-1;))))
                  (br_if 2 (;@5;)
                    (i32.eqz
                      (f64.gt
                        (local.get 0)
                        (f64.const 0x1.62e42fefa39efp+9 (;=709.782712893384;)))))
                  (return
                    (f64.mul
                      (local.get 0)
                      (f64.const 0x1p+1023 (;=89884656743115800000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000;)))))
                (br_if 5 (;@1;)
                  (i32.ge_u
                    (local.get 3)
                    (i32.const 1048576)))
                (drop
                  (f64.load offset=8
                    (local.get 1)))
                (return
                  (local.get 0)))
              (br_if 1 (;@4;)
                (i32.lt_u
                  (local.get 3)
                  (i32.const 1072734898))))
            (local.set 5
              (f64.mul
                (local.tee 4
                  (f64.convert_i32_s
                    (local.tee 3
                      (i32.trunc_sat_f64_s
                        (f64.add
                          (f64.mul
                            (local.get 0)
                            (f64.const 0x1.71547652b82fep+0 (;=1.4426950408889634;)))
                          (f64.copysign
                            (f64.const 0x1p-1 (;=0.5;))
                            (local.get 0)))))))
                (f64.const 0x1.a39ef35793c76p-33 (;=0.00000000019082149292705877;))))
            (local.set 4
              (f64.add
                (local.get 0)
                (f64.mul
                  (local.get 4)
                  (f64.const -0x1.62e42feep-1 (;=-0.6931471803691238;)))))
            (br 1 (;@3;)))
          (block ;; label = @4
            (br_if 0 (;@4;)
              (i64.gt_s
                (local.get 2)
                (i64.const -1)))
            (local.set 4
              (f64.add
                (local.get 0)
                (f64.const 0x1.62e42feep-1 (;=0.6931471803691238;))))
            (local.set 5
              (f64.const -0x1.a39ef35793c76p-33 (;=-0.00000000019082149292705877;)))
            (local.set 3
              (i32.const -1))
            (br 1 (;@3;)))
          (local.set 4
            (f64.add
              (local.get 0)
              (f64.const -0x1.62e42feep-1 (;=-0.6931471803691238;))))
          (local.set 5
            (f64.const 0x1.a39ef35793c76p-33 (;=0.00000000019082149292705877;)))
          (local.set 3
            (i32.const 1)))
        (local.set 4
          (f64.sub
            (f64.sub
              (local.get 4)
              (local.tee 0
                (f64.sub
                  (local.get 4)
                  (local.get 5))))
            (local.get 5))))
      (local.set 6
        (f64.mul
          (local.tee 5
            (f64.mul
              (local.get 0)
              (local.tee 6
                (f64.mul
                  (local.get 0)
                  (f64.const 0x1p-1 (;=0.5;))))))
          (f64.div
            (f64.sub
              (local.tee 7
                (f64.add
                  (f64.mul
                    (local.get 5)
                    (f64.add
                      (f64.mul
                        (local.get 5)
                        (f64.add
                          (f64.mul
                            (local.get 5)
                            (f64.add
                              (f64.mul
                                (local.get 5)
                                (f64.add
                                  (f64.mul
                                    (local.get 5)
                                    (f64.const -0x1.afdb76e09c32dp-23 (;=-0.00000020109921818362437;)))
                                  (f64.const 0x1.0cfca86e65239p-18 (;=0.000004008217827329362;))))
                              (f64.const -0x1.4ce199eaadbb7p-14 (;=-0.0000793650757867488;))))
                          (f64.const 0x1.a01a019fe5585p-10 (;=0.0015873015872548146;))))
                      (f64.const -0x1.11111111110f4p-5 (;=-0.03333333333333313;))))
                  (f64.const 0x1p+0 (;=1;))))
              (local.tee 6
                (f64.sub
                  (f64.const 0x1.8p+1 (;=3;))
                  (f64.mul
                    (local.get 6)
                    (local.get 7)))))
            (f64.sub
              (f64.const 0x1.8p+2 (;=6;))
              (f64.mul
                (local.get 0)
                (local.get 6))))))
      (block ;; label = @2
        (br_if 0 (;@2;)
          (local.get 3))
        (return
          (f64.sub
            (local.get 0)
            (f64.sub
              (f64.mul
                (local.get 0)
                (local.get 6))
              (local.get 5)))))
      (local.set 5
        (f64.sub
          (f64.sub
            (f64.mul
              (local.get 0)
              (f64.sub
                (local.get 6)
                (local.get 4)))
            (local.get 4))
          (local.get 5)))
      (block ;; label = @2
        (block ;; label = @3
          (block ;; label = @4
            (br_table 0 (;@4;) 2 (;@2;) 1 (;@3;) 2 (;@2;)
              (i32.add
                (local.get 3)
                (i32.const 1))))
          (return
            (f64.add
              (f64.mul
                (f64.sub
                  (local.get 0)
                  (local.get 5))
                (f64.const 0x1p-1 (;=0.5;)))
              (f64.const -0x1p-1 (;=-0.5;)))))
        (block ;; label = @3
          (br_if 0 (;@3;)
            (f64.lt
              (local.get 0)
              (f64.const -0x1p-2 (;=-0.25;))))
          (return
            (f64.add
              (f64.add
                (local.tee 0
                  (f64.sub
                    (local.get 0)
                    (local.get 5)))
                (local.get 0))
              (f64.const 0x1p+0 (;=1;)))))
        (return
          (f64.mul
            (f64.sub
              (local.get 5)
              (f64.add
                (local.get 0)
                (f64.const 0x1p-1 (;=0.5;))))
            (f64.const -0x1p+1 (;=-2;)))))
      (local.set 4
        (f64.reinterpret_i64
          (i64.shl
            (i64.extend_i32_u
              (i32.add
                (local.get 3)
                (i32.const 1023)))
            (i64.const 52))))
      (block ;; label = @2
        (br_if 0 (;@2;)
          (i32.lt_u
            (local.get 3)
            (i32.const 57)))
        (return
          (f64.add
            (select
              (f64.mul
                (f64.add
                  (local.tee 0
                    (f64.add
                      (f64.sub
                        (local.get 0)
                        (local.get 5))
                      (f64.const 0x1p+0 (;=1;))))
                  (local.get 0))
                (f64.const 0x1p+1023 (;=89884656743115800000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000;)))
              (f64.mul
                (local.get 0)
                (local.get 4))
              (i32.eq
                (local.get 3)
                (i32.const 1024)))
            (f64.const -0x1p+0 (;=-1;)))))
      (local.set 6
        (f64.reinterpret_i64
          (i64.shl
            (i64.extend_i32_u
              (i32.sub
                (i32.const 1023)
                (local.get 3)))
            (i64.const 52))))
      (block ;; label = @2
        (block ;; label = @3
          (br_if 0 (;@3;)
            (i32.lt_u
              (local.get 3)
              (i32.const 20)))
          (local.set 0
            (f64.add
              (f64.sub
                (local.get 0)
                (f64.add
                  (local.get 5)
                  (local.get 6)))
              (f64.const 0x1p+0 (;=1;))))
          (br 1 (;@2;)))
        (local.set 0
          (f64.add
            (f64.sub
              (f64.const 0x1p+0 (;=1;))
              (local.get 6))
            (f64.sub
              (local.get 0)
              (local.get 5)))))
      (local.set 0
        (f64.mul
          (local.get 0)
          (local.get 4))))
    (local.get 0)
  )
  (func $libm_coshf (;33;) (type 1) (param f32) (result f32)
    (local i32 i32)
    (global.set $__stack_pointer
      (local.tee 1
        (i32.sub
          (global.get $__stack_pointer)
          (i32.const 16))))
    (block ;; label = @1
      (block ;; label = @2
        (br_if 0 (;@2;)
          (i32.lt_u
            (local.tee 2
              (i32.reinterpret_f32
                (local.tee 0
                  (f32.abs
                    (local.get 0)))))
            (i32.const 1060205079)))
        (block ;; label = @3
          (br_if 0 (;@3;)
            (i32.lt_u
              (local.get 2)
              (i32.const 1118925335)))
          (local.set 0
            (f32.mul
              (f32.mul
                (call $_ZN4libm4math4expf4expf17hedd2067d7b29c452E
                  (f32.add
                    (local.get 0)
                    (f32.const -0x1.45c778p+7 (;=-162.88959;))))
                (f32.const 0x1p+117 (;=166153500000000000000000000000000000;)))
              (f32.const 0x1p+117 (;=166153500000000000000000000000000000;))))
          (br 2 (;@1;)))
        (local.set 0
          (f32.mul
            (f32.add
              (local.tee 0
                (call $_ZN4libm4math4expf4expf17hedd2067d7b29c452E
                  (local.get 0)))
              (f32.div
                (f32.const 0x1p+0 (;=1;))
                (local.get 0)))
            (f32.const 0x1p-1 (;=0.5;))))
        (br 1 (;@1;)))
      (block ;; label = @2
        (br_if 0 (;@2;)
          (i32.lt_u
            (local.get 2)
            (i32.const 964689920)))
        (local.set 0
          (f32.add
            (f32.div
              (f32.mul
                (local.tee 0
                  (call $_ZN4libm4math6expm1f6expm1f17h8c93774ba8d4df49E
                    (local.get 0)))
                (local.get 0))
              (f32.add
                (local.tee 0
                  (f32.add
                    (local.get 0)
                    (f32.const 0x1p+0 (;=1;))))
                (local.get 0)))
            (f32.const 0x1p+0 (;=1;))))
        (br 1 (;@1;)))
      (f32.store offset=12
        (local.get 1)
        (f32.add
          (local.get 0)
          (f32.const 0x1p+120 (;=1329228000000000000000000000000000000;))))
      (drop
        (f32.load offset=12
          (local.get 1)))
      (local.set 0
        (f32.const 0x1p+0 (;=1;))))
    (global.set $__stack_pointer
      (i32.add
        (local.get 1)
        (i32.const 16)))
    (local.get 0)
  )
  (func $_ZN4libm4math4expf4expf17hedd2067d7b29c452E (;34;) (type 1) (param f32) (result f32)
    (local i32 i32 i32 i32 f32 f32 f32)
    (global.set $__stack_pointer
      (local.tee 1
        (i32.sub
          (global.get $__stack_pointer)
          (i32.const 16))))
    (local.set 3
      (i32.shr_u
        (local.tee 2
          (i32.reinterpret_f32
            (local.get 0)))
        (i32.const 31)))
    (block ;; label = @1
      (block ;; label = @2
        (block ;; label = @3
          (block ;; label = @4
            (block ;; label = @5
              (block ;; label = @6
                (block ;; label = @7
                  (br_if 0 (;@7;)
                    (i32.lt_u
                      (local.tee 4
                        (i32.and
                          (local.get 2)
                          (i32.const 2147483647)))
                      (i32.const 1118743632)))
                  (block ;; label = @8
                    (br_if 0 (;@8;)
                      (i32.le_u
                        (local.get 4)
                        (i32.const 2139095040)))
                    (local.set 5
                      (local.get 0))
                    (br 7 (;@1;)))
                  (block ;; label = @8
                    (br_if 0 (;@8;)
                      (i32.gt_u
                        (local.get 4)
                        (i32.const 1118925335)))
                    (br_if 2 (;@6;)
                      (i32.gt_s
                        (local.get 2)
                        (i32.const -1)))
                    (f32.store offset=8
                      (local.get 1)
                      (f32.div
                        (f32.const -0x1p-126 (;=-0.000000000000000000000000000000000000011754944;))
                        (local.get 0)))
                    (drop
                      (f32.load offset=8
                        (local.get 1)))
                    (br 2 (;@6;)))
                  (block ;; label = @8
                    (br_if 0 (;@8;)
                      (i32.gt_s
                        (local.get 2)
                        (i32.const -1)))
                    (f32.store offset=8
                      (local.get 1)
                      (f32.div
                        (f32.const -0x1p-126 (;=-0.000000000000000000000000000000000000011754944;))
                        (local.get 0)))
                    (drop
                      (f32.load offset=8
                        (local.get 1)))
                    (local.set 5
                      (f32.const 0x0p+0 (;=0;)))
                    (br_if 2 (;@6;)
                      (i32.le_u
                        (local.get 4)
                        (i32.const 1120924084)))
                    (br 7 (;@1;)))
                  (local.set 5
                    (f32.mul
                      (local.get 0)
                      (f32.const 0x1p+127 (;=170141180000000000000000000000000000000;))))
                  (br 6 (;@1;)))
                (block ;; label = @7
                  (br_if 0 (;@7;)
                    (i32.gt_u
                      (local.get 4)
                      (i32.const 1051816472)))
                  (br_if 2 (;@5;)
                    (i32.le_u
                      (local.get 4)
                      (i32.const 956301312)))
                  (local.set 4
                    (i32.const 0))
                  (local.set 6
                    (f32.const 0x0p+0 (;=0;)))
                  (local.set 5
                    (local.get 0))
                  (br 5 (;@2;)))
                (br_if 2 (;@4;)
                  (i32.le_u
                    (local.get 4)
                    (i32.const 1065686418))))
              (local.set 4
                (i32.trunc_sat_f32_s
                  (f32.add
                    (f32.mul
                      (local.get 0)
                      (f32.const 0x1.715476p+0 (;=1.442695;)))
                    (f32.load offset=1052728
                      (i32.shl
                        (local.get 3)
                        (i32.const 2))))))
              (br 2 (;@3;)))
            (f32.store offset=12
              (local.get 1)
              (f32.add
                (local.get 0)
                (f32.const 0x1p+127 (;=170141180000000000000000000000000000000;))))
            (local.set 5
              (f32.add
                (local.get 0)
                (f32.const 0x1p+0 (;=1;))))
            (drop
              (f32.load offset=12
                (local.get 1)))
            (br 3 (;@1;)))
          (local.set 4
            (i32.sub
              (i32.xor
                (local.get 3)
                (i32.const 1))
              (local.get 3))))
        (local.set 5
          (f32.sub
            (local.tee 0
              (f32.add
                (local.get 0)
                (f32.mul
                  (local.tee 5
                    (f32.convert_i32_s
                      (local.get 4)))
                  (f32.const -0x1.62e4p-1 (;=-0.69314575;)))))
            (local.tee 6
              (f32.mul
                (local.get 5)
                (f32.const 0x1.7f7d1cp-20 (;=0.0000014286068;)))))))
      (local.set 5
        (f32.add
          (f32.add
            (local.get 0)
            (f32.sub
              (f32.div
                (f32.mul
                  (local.get 5)
                  (local.tee 7
                    (f32.sub
                      (local.get 5)
                      (f32.mul
                        (local.tee 7
                          (f32.mul
                            (local.get 5)
                            (local.get 5)))
                        (f32.add
                          (f32.mul
                            (local.get 7)
                            (f32.const -0x1.6aa42ap-9 (;=-0.0027667333;)))
                          (f32.const 0x1.55551ep-3 (;=0.16666625;)))))))
                (f32.sub
                  (f32.const 0x1p+1 (;=2;))
                  (local.get 7)))
              (local.get 6)))
          (f32.const 0x1p+0 (;=1;))))
      (br_if 0 (;@1;)
        (i32.eqz
          (local.get 4)))
      (local.set 5
        (call $_ZN4libm4math6scalbn7scalbnf17ha430054e259e38a4E
          (local.get 5)
          (local.get 4))))
    (global.set $__stack_pointer
      (i32.add
        (local.get 1)
        (i32.const 16)))
    (local.get 5)
  )
  (func $_ZN4libm4math6expm1f6expm1f17h8c93774ba8d4df49E (;35;) (type 1) (param f32) (result f32)
    (local i32 i32 i32 f32 f32 f32 f32)
    (local.set 1
      (i32.sub
        (global.get $__stack_pointer)
        (i32.const 16)))
    (block ;; label = @1
      (block ;; label = @2
        (block ;; label = @3
          (block ;; label = @4
            (block ;; label = @5
              (block ;; label = @6
                (block ;; label = @7
                  (block ;; label = @8
                    (block ;; label = @9
                      (block ;; label = @10
                        (br_if 0 (;@10;)
                          (i32.gt_u
                            (local.tee 3
                              (i32.and
                                (local.tee 2
                                  (i32.reinterpret_f32
                                    (local.get 0)))
                                (i32.const 2147483647)))
                            (i32.const 1100331075)))
                        (br_if 1 (;@9;)
                          (i32.gt_u
                            (local.get 3)
                            (i32.const 1051816472)))
                        (br_if 6 (;@4;)
                          (i32.lt_u
                            (local.get 3)
                            (i32.const 855638016)))
                        (local.set 3
                          (i32.const 0))
                        (local.set 4
                          (f32.const 0x0p+0 (;=0;)))
                        (br 5 (;@5;)))
                      (local.set 5
                        (select
                          (local.get 0)
                          (f32.const -0x1p+0 (;=-1;))
                          (local.tee 1
                            (i32.gt_u
                              (local.get 3)
                              (i32.const 2139095040)))))
                      (br_if 7 (;@2;)
                        (i32.lt_s
                          (local.get 2)
                          (i32.const 0)))
                      (br_if 7 (;@2;)
                        (local.get 1))
                      (local.set 5
                        (f32.const 0x1p-1 (;=0.5;)))
                      (br_if 1 (;@8;)
                        (i32.lt_u
                          (local.get 3)
                          (i32.const 1118925336)))
                      (return
                        (f32.mul
                          (local.get 0)
                          (f32.const 0x1p+127 (;=170141180000000000000000000000000000000;)))))
                    (br_if 1 (;@7;)
                      (i32.lt_u
                        (local.get 3)
                        (i32.const 1065686418)))
                    (local.set 5
                      (select
                        (f32.const -0x1p-1 (;=-0.5;))
                        (f32.const 0x1p-1 (;=0.5;))
                        (i32.lt_s
                          (local.get 2)
                          (i32.const 0)))))
                  (local.set 5
                    (f32.mul
                      (local.tee 4
                        (f32.convert_i32_s
                          (local.tee 3
                            (i32.trunc_sat_f32_s
                              (f32.add
                                (f32.mul
                                  (local.get 0)
                                  (f32.const 0x1.715476p+0 (;=1.442695;)))
                                (local.get 5))))))
                      (f32.const 0x1.2fefa2p-17 (;=0.000009058001;))))
                  (local.set 4
                    (f32.add
                      (local.get 0)
                      (f32.mul
                        (local.get 4)
                        (f32.const -0x1.62e3p-1 (;=-0.6931381;)))))
                  (br 1 (;@6;)))
                (block ;; label = @7
                  (br_if 0 (;@7;)
                    (i32.lt_s
                      (local.get 2)
                      (i32.const 0)))
                  (local.set 4
                    (f32.add
                      (local.get 0)
                      (f32.const -0x1.62e3p-1 (;=-0.6931381;))))
                  (local.set 5
                    (f32.const 0x1.2fefa2p-17 (;=0.000009058001;)))
                  (local.set 3
                    (i32.const 1))
                  (br 1 (;@6;)))
                (local.set 4
                  (f32.add
                    (local.get 0)
                    (f32.const 0x1.62e3p-1 (;=0.6931381;))))
                (local.set 5
                  (f32.const -0x1.2fefa2p-17 (;=-0.000009058001;)))
                (local.set 3
                  (i32.const -1)))
              (local.set 4
                (f32.sub
                  (f32.sub
                    (local.get 4)
                    (local.tee 0
                      (f32.sub
                        (local.get 4)
                        (local.get 5))))
                  (local.get 5))))
            (local.set 6
              (f32.mul
                (local.tee 5
                  (f32.mul
                    (local.get 0)
                    (local.tee 6
                      (f32.mul
                        (local.get 0)
                        (f32.const 0x1p-1 (;=0.5;))))))
                (f32.div
                  (f32.sub
                    (local.tee 7
                      (f32.add
                        (f32.mul
                          (local.get 5)
                          (f32.add
                            (f32.mul
                              (local.get 5)
                              (f32.const 0x1.9e602p-10 (;=0.001580717;)))
                            (f32.const -0x1.1110dp-5 (;=-0.033333212;))))
                        (f32.const 0x1p+0 (;=1;))))
                    (local.tee 6
                      (f32.sub
                        (f32.const 0x1.8p+1 (;=3;))
                        (f32.mul
                          (local.get 6)
                          (local.get 7)))))
                  (f32.sub
                    (f32.const 0x1.8p+2 (;=6;))
                    (f32.mul
                      (local.get 0)
                      (local.get 6))))))
            (br_if 1 (;@3;)
              (local.get 3))
            (return
              (f32.sub
                (local.get 0)
                (f32.sub
                  (f32.mul
                    (local.get 0)
                    (local.get 6))
                  (local.get 5)))))
          (br_if 2 (;@1;)
            (i32.ge_u
              (local.get 3)
              (i32.const 8388608)))
          (f32.store offset=12
            (local.get 1)
            (f32.mul
              (local.get 0)
              (local.get 0)))
          (drop
            (f32.load offset=12
              (local.get 1)))
          (br 2 (;@1;)))
        (local.set 5
          (f32.sub
            (f32.sub
              (f32.mul
                (local.get 0)
                (f32.sub
                  (local.get 6)
                  (local.get 4)))
              (local.get 4))
            (local.get 5)))
        (block ;; label = @3
          (block ;; label = @4
            (block ;; label = @5
              (br_table 0 (;@5;) 2 (;@3;) 1 (;@4;) 2 (;@3;)
                (i32.add
                  (local.get 3)
                  (i32.const 1))))
            (return
              (f32.add
                (f32.mul
                  (f32.sub
                    (local.get 0)
                    (local.get 5))
                  (f32.const 0x1p-1 (;=0.5;)))
                (f32.const -0x1p-1 (;=-0.5;)))))
          (block ;; label = @4
            (br_if 0 (;@4;)
              (f32.lt
                (local.get 0)
                (f32.const -0x1p-2 (;=-0.25;))))
            (return
              (f32.add
                (f32.add
                  (local.tee 0
                    (f32.sub
                      (local.get 0)
                      (local.get 5)))
                  (local.get 0))
                (f32.const 0x1p+0 (;=1;)))))
          (return
            (f32.mul
              (f32.sub
                (local.get 5)
                (f32.add
                  (local.get 0)
                  (f32.const 0x1p-1 (;=0.5;))))
              (f32.const -0x1p+1 (;=-2;)))))
        (local.set 4
          (f32.reinterpret_i32
            (i32.add
              (local.tee 2
                (i32.shl
                  (local.get 3)
                  (i32.const 23)))
              (i32.const 1065353216))))
        (block ;; label = @3
          (br_if 0 (;@3;)
            (i32.lt_u
              (local.get 3)
              (i32.const 57)))
          (return
            (f32.add
              (select
                (f32.mul
                  (f32.add
                    (local.tee 0
                      (f32.add
                        (f32.sub
                          (local.get 0)
                          (local.get 5))
                        (f32.const 0x1p+0 (;=1;))))
                    (local.get 0))
                  (f32.const 0x1p+127 (;=170141180000000000000000000000000000000;)))
                (f32.mul
                  (local.get 0)
                  (local.get 4))
                (i32.eq
                  (local.get 3)
                  (i32.const 128)))
              (f32.const -0x1p+0 (;=-1;)))))
        (local.set 6
          (f32.reinterpret_i32
            (i32.sub
              (i32.const 1065353216)
              (local.get 2))))
        (block ;; label = @3
          (block ;; label = @4
            (br_if 0 (;@4;)
              (i32.lt_u
                (local.get 3)
                (i32.const 23)))
            (local.set 0
              (f32.add
                (f32.sub
                  (local.get 0)
                  (f32.add
                    (local.get 5)
                    (local.get 6)))
                (f32.const 0x1p+0 (;=1;))))
            (br 1 (;@3;)))
          (local.set 0
            (f32.add
              (f32.sub
                (f32.const 0x1p+0 (;=1;))
                (local.get 6))
              (f32.sub
                (local.get 0)
                (local.get 5)))))
        (local.set 5
          (f32.mul
            (local.get 0)
            (local.get 4))))
      (return
        (local.get 5)))
    (local.get 0)
  )
  (func $libm_exp (;36;) (type 0) (param f64) (result f64)
    (call $_ZN4libm4math3exp3exp17h8eb8b2450c3bf8abE
      (local.get 0))
  )
  (func $libm_exp2 (;37;) (type 0) (param f64) (result f64)
    (local i32 i64 i64 f64 i32 i32 f64)
    (global.set $__stack_pointer
      (local.tee 1
        (i32.sub
          (global.get $__stack_pointer)
          (i32.const 16))))
    (block ;; label = @1
      (block ;; label = @2
        (block ;; label = @3
          (br_if 0 (;@3;)
            (i64.gt_u
              (local.tee 3
                (i64.and
                  (i64.shr_u
                    (local.tee 2
                      (i64.reinterpret_f64
                        (local.get 0)))
                    (i64.const 32))
                  (i64.const 2147483647)))
              (i64.const 1083174911)))
          (br_if 1 (;@2;)
            (i64.ge_u
              (local.get 3)
              (i64.const 1016070144)))
          (local.set 0
            (f64.add
              (local.get 0)
              (f64.const 0x1p+0 (;=1;))))
          (br 2 (;@1;)))
        (block ;; label = @3
          (block ;; label = @4
            (br_if 0 (;@4;)
              (i64.lt_s
                (local.get 2)
                (i64.const 0)))
            (br_if 1 (;@3;)
              (i64.gt_u
                (local.get 3)
                (i64.const 1083179007))))
          (block ;; label = @4
            (block ;; label = @5
              (br_if 0 (;@5;)
                (i64.gt_u
                  (local.get 3)
                  (i64.const 2146435071)))
              (br_if 1 (;@4;)
                (i64.le_s
                  (local.get 2)
                  (i64.const -1)))
              (br 3 (;@2;)))
            (local.set 0
              (f64.div
                (f64.const -0x1p+0 (;=-1;))
                (local.get 0)))
            (br 3 (;@1;)))
          (block ;; label = @4
            (br_if 0 (;@4;)
              (i32.eqz
                (f64.le
                  (local.get 0)
                  (f64.const -0x1.0ccp+10 (;=-1075;)))))
            (f32.store offset=12
              (local.get 1)
              (f32.demote_f64
                (f64.div
                  (f64.const -0x1p-149 (;=-0.000000000000000000000000000000000000000000001401298464324817;))
                  (local.get 0))))
            (drop
              (f32.load offset=12
                (local.get 1)))
            (local.set 0
              (f64.const 0x0p+0 (;=0;)))
            (br 3 (;@1;)))
          (br_if 1 (;@2;)
            (f64.eq
              (f64.add
                (f64.add
                  (local.get 0)
                  (f64.const -0x1p+52 (;=-4503599627370496;)))
                (f64.const 0x1p+52 (;=4503599627370496;)))
              (local.get 0)))
          (f32.store offset=12
            (local.get 1)
            (f32.demote_f64
              (f64.div
                (f64.const -0x1p-149 (;=-0.000000000000000000000000000000000000000000001401298464324817;))
                (local.get 0))))
          (drop
            (f32.load offset=12
              (local.get 1)))
          (br 1 (;@2;)))
        (local.set 0
          (f64.mul
            (local.get 0)
            (f64.const 0x1p+1023 (;=89884656743115800000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000;))))
        (br 1 (;@1;)))
      (local.set 0
        (call $_ZN4libm4math6scalbn6scalbn17hd5ee51b98c77623bE
          (f64.add
            (local.tee 7
              (f64.load offset=1048632
                (local.tee 6
                  (i32.and
                    (i32.shl
                      (local.tee 5
                        (i32.add
                          (i32.wrap_i64
                            (i64.reinterpret_f64
                              (local.tee 4
                                (f64.add
                                  (local.get 0)
                                  (f64.const 0x1.8p+44 (;=26388279066624;))))))
                          (i32.const 128)))
                      (i32.const 4))
                    (i32.const 4080)))))
            (f64.mul
              (f64.mul
                (local.get 7)
                (local.tee 0
                  (f64.sub
                    (f64.sub
                      (local.get 0)
                      (f64.add
                        (local.get 4)
                        (f64.const -0x1.8p+44 (;=-26388279066624;))))
                    (f64.load offset=1048640
                      (local.get 6)))))
              (f64.add
                (f64.mul
                  (local.get 0)
                  (f64.add
                    (f64.mul
                      (local.get 0)
                      (f64.add
                        (f64.mul
                          (local.get 0)
                          (f64.add
                            (f64.mul
                              (local.get 0)
                              (f64.const 0x1.5d88003875c74p-10 (;=0.0013333559164630223;)))
                            (f64.const 0x1.3b2ab88f704p-7 (;=0.009618129842126066;))))
                        (f64.const 0x1.c6b08d704a0a6p-5 (;=0.0555041086648214;))))
                    (f64.const 0x1.ebfbdff82c575p-3 (;=0.2402265069591;))))
                (f64.const 0x1.62e42fefa39efp-1 (;=0.6931471805599453;)))))
          (i32.shr_s
            (local.get 5)
            (i32.const 8)))))
    (global.set $__stack_pointer
      (i32.add
        (local.get 1)
        (i32.const 16)))
    (local.get 0)
  )
  (func $_ZN4libm4math6scalbn6scalbn17hd5ee51b98c77623bE (;38;) (type 7) (param f64 i32) (result f64)
    (block ;; label = @1
      (block ;; label = @2
        (block ;; label = @3
          (block ;; label = @4
            (br_if 0 (;@4;)
              (i32.gt_s
                (local.get 1)
                (i32.const 1023)))
            (br_if 3 (;@1;)
              (i32.ge_s
                (local.get 1)
                (i32.const -1022)))
            (local.set 0
              (f64.mul
                (local.get 0)
                (f64.const 0x1p-969 (;=0.0000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000002004168360008973;))))
            (br_if 1 (;@3;)
              (i32.le_u
                (local.get 1)
                (i32.const -1992)))
            (local.set 1
              (i32.add
                (local.get 1)
                (i32.const 969)))
            (br 3 (;@1;)))
          (local.set 0
            (f64.mul
              (local.get 0)
              (f64.const 0x1p+1023 (;=89884656743115800000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000;))))
          (br_if 1 (;@2;)
            (i32.gt_u
              (local.get 1)
              (i32.const 2046)))
          (local.set 1
            (i32.add
              (local.get 1)
              (i32.const -1023)))
          (br 2 (;@1;)))
        (local.set 0
          (f64.mul
            (local.get 0)
            (f64.const 0x1p-969 (;=0.0000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000002004168360008973;))))
        (local.set 1
          (i32.add
            (select
              (local.get 1)
              (i32.const -2960)
              (i32.gt_u
                (local.get 1)
                (i32.const -2960)))
            (i32.const 1938)))
        (br 1 (;@1;)))
      (local.set 0
        (f64.mul
          (local.get 0)
          (f64.const 0x1p+1023 (;=89884656743115800000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000;))))
      (local.set 1
        (i32.add
          (select
            (local.get 1)
            (i32.const 3069)
            (i32.lt_u
              (local.get 1)
              (i32.const 3069)))
          (i32.const -2046))))
    (f64.mul
      (local.get 0)
      (f64.reinterpret_i64
        (i64.shl
          (i64.extend_i32_u
            (i32.and
              (i32.add
                (local.get 1)
                (i32.const 1023))
              (i32.const 2047)))
          (i64.const 52))))
  )
  (func $libm_exp2f (;39;) (type 1) (param f32) (result f32)
    (local i32 i32 i32 f32 f64 f64)
    (local.set 1
      (i32.sub
        (global.get $__stack_pointer)
        (i32.const 16)))
    (block ;; label = @1
      (block ;; label = @2
        (br_if 0 (;@2;)
          (i32.gt_u
            (local.tee 3
              (i32.and
                (local.tee 2
                  (i32.reinterpret_f32
                    (local.get 0)))
                (i32.const 2147483647)))
            (i32.const 1123811328)))
        (br_if 1 (;@1;)
          (i32.ge_u
            (local.get 3)
            (i32.const 855638017)))
        (return
          (f32.add
            (local.get 0)
            (f32.const 0x1p+0 (;=1;)))))
      (block ;; label = @2
        (block ;; label = @3
          (br_if 0 (;@3;)
            (i32.gt_u
              (local.get 3)
              (i32.const 2139095040)))
          (block ;; label = @4
            (br_if 0 (;@4;)
              (i32.gt_s
                (local.get 2)
                (i32.const 1124073471)))
            (br_if 3 (;@1;)
              (i32.ge_s
                (local.get 2)
                (i32.const 0)))
            (br_if 2 (;@2;)
              (i32.gt_u
                (local.get 2)
                (i32.const -1021968385)))
            (br_if 3 (;@1;)
              (i32.eqz
                (i32.and
                  (local.get 2)
                  (i32.const 65535))))
            (f32.store offset=12
              (local.get 1)
              (f32.div
                (f32.const -0x1.p-149 (;=-0.000000000000000000000000000000000000000000001;))
                (local.get 0)))
            (drop
              (f32.load offset=12
                (local.get 1)))
            (br 3 (;@1;)))
          (local.set 0
            (f32.mul
              (local.get 0)
              (f32.const 0x1p+127 (;=170141180000000000000000000000000000000;)))))
        (return
          (local.get 0)))
      (f32.store offset=12
        (local.get 1)
        (f32.div
          (f32.const -0x1.p-149 (;=-0.000000000000000000000000000000000000000000001;))
          (local.get 0)))
      (drop
        (f32.load offset=12
          (local.get 1)))
      (return
        (f32.const 0x0p+0 (;=0;))))
    (f32.demote_f64
      (f64.mul
        (f64.add
          (f64.add
            (local.tee 5
              (f64.load offset=1052736
                (i32.shl
                  (i32.and
                    (local.tee 3
                      (i32.add
                        (i32.reinterpret_f32
                          (local.tee 4
                            (f32.add
                              (local.get 0)
                              (f32.const 0x1.8p+19 (;=786432;)))))
                        (i32.const 8)))
                    (i32.const 15))
                  (i32.const 3))))
            (f64.mul
              (f64.add
                (f64.mul
                  (local.tee 6
                    (f64.promote_f32
                      (f32.sub
                        (local.get 0)
                        (f32.add
                          (local.get 4)
                          (f32.const -0x1.8p+19 (;=-786432;))))))
                  (f64.const 0x1.ebfbep-3 (;=0.24022650718688965;)))
                (f64.const 0x1.62e43p-1 (;=0.6931471824645996;)))
              (local.tee 5
                (f64.mul
                  (local.get 5)
                  (local.get 6)))))
          (f64.mul
            (f64.add
              (f64.mul
                (local.get 6)
                (f64.const 0x1.3b2c9cp-7 (;=0.009618354961276054;)))
              (f64.const 0x1.c6b348p-5 (;=0.055505409836769104;)))
            (f64.mul
              (f64.mul
                (local.get 6)
                (local.get 6))
              (local.get 5))))
        (f64.reinterpret_i64
          (i64.shl
            (i64.extend_i32_u
              (i32.add
                (i32.shr_u
                  (local.get 3)
                  (i32.const 4))
                (i32.const 1023)))
            (i64.const 52)))))
  )
  (func $libm_expf (;40;) (type 1) (param f32) (result f32)
    (call $_ZN4libm4math4expf4expf17hedd2067d7b29c452E
      (local.get 0))
  )
  (func $libm_expm1 (;41;) (type 0) (param f64) (result f64)
    (call $_ZN4libm4math5expm15expm117hf425b3a732f15702E
      (local.get 0))
  )
  (func $libm_expm1f (;42;) (type 1) (param f32) (result f32)
    (call $_ZN4libm4math6expm1f6expm1f17h8c93774ba8d4df49E
      (local.get 0))
  )
  (func $libm_fmod (;43;) (type 2) (param f64 f64) (result f64)
    (local i32 i64 i64 i64 i64 i64 i64 i64 i64 i32 i32 i32 i64)
    (global.set $__stack_pointer
      (local.tee 2
        (i32.sub
          (global.get $__stack_pointer)
          (i32.const 144))))
    (block ;; label = @1
      (block ;; label = @2
        (block ;; label = @3
          (block ;; label = @4
            (block ;; label = @5
              (block ;; label = @6
                (br_if 0 (;@6;)
                  (i64.gt_s
                    (local.tee 4
                      (i64.and
                        (local.tee 3
                          (i64.reinterpret_f64
                            (local.get 0)))
                        (i64.const 9223372036854775807)))
                    (i64.const 9218868437227405311)))
                (br_if 0 (;@6;)
                  (i64.eqz
                    (i64.and
                      (i64.sub
                        (i64.const 0)
                        (local.tee 5
                          (i64.reinterpret_f64
                            (local.get 1))))
                      (i64.const 9218868437227405312))))
                (br_if 5 (;@1;)
                  (i64.lt_u
                    (local.get 4)
                    (local.tee 6
                      (i64.and
                        (local.get 5)
                        (i64.const 9223372036854775807)))))
                (block ;; label = @7
                  (br_if 0 (;@7;)
                    (i64.lt_u
                      (local.tee 4
                        (i64.sub
                          (local.get 4)
                          (i64.and
                            (local.tee 7
                              (select
                                (i64.const 0)
                                (local.tee 5
                                  (i64.add
                                    (local.get 4)
                                    (i64.const -4503599627370496)))
                                (i64.gt_u
                                  (local.get 5)
                                  (local.get 4))))
                            (i64.const 9218868437227405312))))
                      (local.tee 10
                        (i64.shl
                          (local.tee 5
                            (i64.sub
                              (local.get 6)
                              (local.tee 9
                                (i64.and
                                  (local.tee 8
                                    (select
                                      (i64.const 0)
                                      (local.tee 5
                                        (i64.add
                                          (local.get 6)
                                          (i64.const -4503599627370496)))
                                      (i64.gt_u
                                        (local.get 5)
                                        (local.get 6))))
                                  (i64.const 9218868437227405312)))))
                          (i64.const 1)))))
                  (br_if 2 (;@5;)
                    (i64.eq
                      (local.get 6)
                      (local.get 9)))
                  (local.set 4
                    (i64.rem_u
                      (local.get 4)
                      (local.get 5))))
                (local.set 9
                  (i64.and
                    (local.get 3)
                    (i64.const -9223372036854775808)))
                (block ;; label = @7
                  (br_if 0 (;@7;)
                    (i32.gt_u
                      (local.tee 12
                        (i32.sub
                          (i32.wrap_i64
                            (local.tee 6
                              (i64.shr_u
                                (local.get 7)
                                (i64.const 52))))
                          (local.tee 11
                            (i32.wrap_i64
                              (local.tee 3
                                (i64.shr_u
                                  (local.get 8)
                                  (i64.const 52)))))))
                      (i32.const 31)))
                  (block ;; label = @8
                    (br_if 0 (;@8;)
                      (i64.eq
                        (local.get 6)
                        (local.get 3)))
                    (loop ;; label = @9
                      (local.set 4
                        (i64.shl
                          (i64.sub
                            (local.get 4)
                            (select
                              (i64.const 0)
                              (local.get 5)
                              (i64.lt_u
                                (local.get 4)
                                (local.get 5))))
                          (i64.const 1)))
                      (br_if 0 (;@9;)
                        (local.tee 12
                          (i32.add
                            (local.get 12)
                            (i32.const -1))))))
                  (local.set 4
                    (i64.sub
                      (local.get 4)
                      (select
                        (i64.const 0)
                        (local.get 5)
                        (i64.lt_u
                          (local.get 4)
                          (local.get 5)))))
                  (br 4 (;@3;)))
                (br_if 2 (;@4;)
                  (i32.ge_u
                    (local.get 12)
                    (i32.const 64)))
                (call $__ashlti3
                  (i32.add
                    (local.get 2)
                    (i32.const 128))
                  (local.get 4)
                  (i64.const 0)
                  (local.get 12))
                (br_if 2 (;@4;)
                  (i64.le_u
                    (local.get 5)
                    (local.tee 6
                      (i64.load offset=136
                        (local.get 2)))))
                (call $__umodti3
                  (local.get 2)
                  (i64.load offset=128
                    (local.get 2))
                  (local.get 6)
                  (local.get 5)
                  (i64.const 0))
                (local.set 4
                  (i64.load
                    (local.get 2)))
                (br 3 (;@3;)))
              (local.set 0
                (f64.div
                  (local.tee 0
                    (f64.mul
                      (local.get 0)
                      (local.get 1)))
                  (local.get 0)))
              (br 4 (;@1;)))
            (call $_ZN4core9panicking11panic_const23panic_const_rem_by_zero17h4d91c9c4a6b3b2e4E)
            (unreachable))
          (block ;; label = @4
            (block ;; label = @5
              (block ;; label = @6
                (block ;; label = @7
                  (block ;; label = @8
                    (block ;; label = @9
                      (block ;; label = @10
                        (br_if 0 (;@10;)
                          (i64.ge_u
                            (local.get 5)
                            (i64.const 4611686018427387904)))
                        (br_if 1 (;@9;)
                          (i64.ge_u
                            (local.get 4)
                            (local.get 10)))
                        (block ;; label = @11
                          (br_if 0 (;@11;)
                            (i64.eqz
                              (i64.and
                                (local.get 5)
                                (local.tee 6
                                  (i64.add
                                    (local.get 5)
                                    (i64.const -1))))))
                          (br_if 3 (;@8;)
                            (i64.le_u
                              (local.tee 7
                                (i64.shl
                                  (local.get 5)
                                  (local.tee 10
                                    (i64.extend_i32_u
                                      (i32.and
                                        (local.tee 13
                                          (i32.add
                                            (i32.wrap_i64
                                              (i64.clz
                                                (local.get 5)))
                                            (i32.const -2)))
                                        (i32.const 63))))))
                              (i64.const 2305843009213693952)))
                          (br_if 4 (;@7;)
                            (i64.ge_u
                              (local.get 7)
                              (i64.const 4611686018427387904)))
                          (br_if 5 (;@6;)
                            (i64.ge_u
                              (local.get 4)
                              (local.tee 8
                                (i64.shl
                                  (local.get 7)
                                  (i64.const 1)))))
                          (call $__udivti3
                            (i32.add
                              (local.get 2)
                              (i32.const 112))
                            (i64.const 0)
                            (local.tee 6
                              (i64.sub
                                (i64.const -9223372036854775808)
                                (local.get 8)))
                            (local.get 8)
                            (i64.const 0))
                          (call $__multi3
                            (i32.add
                              (local.get 2)
                              (i32.const 96))
                            (local.tee 5
                              (i64.load offset=112
                                (local.get 2)))
                            (i64.load offset=120
                              (local.get 2))
                            (local.get 8)
                            (i64.const 0))
                          (call $__multi3
                            (i32.add
                              (local.get 2)
                              (i32.const 80))
                            (local.get 5)
                            (i64.const 1)
                            (local.tee 14
                              (i64.shl
                                (local.get 4)
                                (i64.const 1)))
                            (i64.const 0))
                          (local.set 6
                            (i64.sub
                              (i64.sub
                                (local.get 6)
                                (i64.load offset=104
                                  (local.get 2)))
                              (i64.extend_i32_u
                                (i64.ne
                                  (local.tee 4
                                    (i64.load offset=96
                                      (local.get 2)))
                                  (i64.const 0)))))
                          (local.set 3
                            (i64.sub
                              (i64.const 0)
                              (local.get 4)))
                          (local.set 4
                            (i64.load offset=88
                              (local.get 2)))
                          (block ;; label = @12
                            (br_if 0 (;@12;)
                              (i32.gt_u
                                (local.tee 12
                                  (i32.add
                                    (local.get 13)
                                    (local.get 12)))
                                (i32.const 62)))
                            (local.set 5
                              (i64.load offset=80
                                (local.get 2)))
                            (br 8 (;@4;)))
                          (local.set 5
                            (i64.mul
                              (local.get 14)
                              (local.get 5)))
                          (loop ;; label = @12
                            (call $__multi3
                              (i32.add
                                (local.get 2)
                                (i32.const 64))
                              (local.get 3)
                              (local.get 6)
                              (local.get 4)
                              (i64.const 0))
                            (local.set 4
                              (i64.add
                                (i64.load offset=72
                                  (local.get 2))
                                (i64.shr_u
                                  (local.get 5)
                                  (i64.const 1))))
                            (local.set 5
                              (i64.load offset=64
                                (local.get 2)))
                            (br_if 0 (;@12;)
                              (i32.gt_u
                                (local.tee 12
                                  (i32.add
                                    (local.get 12)
                                    (i32.const -63)))
                                (i32.const 62)))
                            (br 8 (;@4;))))
                        (br_if 5 (;@5;)
                          (i32.lt_u
                            (local.get 12)
                            (i32.const 64)))
                        (br 8 (;@2;)))
                      (call $_ZN4core9panicking5panic17h0149fc8f1656305aE
                        (i32.const 34))
                      (unreachable))
                    (call $_ZN4core9panicking5panic17h0149fc8f1656305aE
                      (i32.const 30))
                    (unreachable))
                  (call $_ZN4core9panicking5panic17h0149fc8f1656305aE
                    (i32.const 43))
                  (unreachable))
                (call $_ZN4core9panicking5panic17h0149fc8f1656305aE
                  (i32.const 43))
                (unreachable))
              (call $_ZN4core9panicking5panic17h0149fc8f1656305aE
                (i32.const 23))
              (unreachable))
            (local.set 4
              (i64.and
                (i64.shl
                  (local.get 4)
                  (i64.extend_i32_u
                    (local.get 12)))
                (local.get 6)))
            (br 1 (;@3;)))
          (call $__ashlti3
            (i32.add
              (local.get 2)
              (i32.const 48))
            (local.get 5)
            (local.get 4)
            (local.get 12))
          (call $__multi3
            (i32.add
              (local.get 2)
              (i32.const 32))
            (local.get 3)
            (local.get 6)
            (i64.shr_u
              (local.get 4)
              (i64.extend_i32_u
                (i32.xor
                  (local.get 12)
                  (i32.const 63))))
            (i64.const 0))
          (call $__multi3
            (i32.add
              (local.get 2)
              (i32.const 16))
            (i64.add
              (i64.add
                (i64.add
                  (i64.load offset=40
                    (local.get 2))
                  (i64.and
                    (i64.load offset=56
                      (local.get 2))
                    (i64.const 9223372036854775807)))
                (i64.extend_i32_u
                  (i64.lt_u
                    (i64.add
                      (local.tee 4
                        (i64.load offset=32
                          (local.get 2)))
                      (i64.load offset=48
                        (local.get 2)))
                    (local.get 4))))
              (i64.const 2))
            (i64.const 0)
            (local.get 8)
            (i64.const 0))
          (local.set 4
            (i64.shr_u
              (i64.sub
                (local.tee 4
                  (i64.load offset=24
                    (local.get 2)))
                (select
                  (i64.const 0)
                  (local.get 7)
                  (i64.gt_u
                    (local.get 7)
                    (local.get 4))))
              (local.get 10))))
        (br_if 0 (;@2;)
          (i64.eqz
            (local.get 4)))
        (local.set 0
          (f64.reinterpret_i64
            (i64.add
              (i64.add
                (i64.shl
                  (i64.extend_i32_u
                    (i32.sub
                      (local.get 11)
                      (local.tee 12
                        (select
                          (local.tee 12
                            (i32.sub
                              (i32.const 52)
                              (i32.xor
                                (i32.wrap_i64
                                  (i64.clz
                                    (local.get 4)))
                                (i32.const 63))))
                          (local.get 11)
                          (i32.lt_u
                            (local.get 12)
                            (local.get 11))))))
                  (i64.const 52))
                (local.get 9))
              (i64.shl
                (local.get 4)
                (i64.extend_i32_u
                  (i32.and
                    (local.get 12)
                    (i32.const 63)))))))
        (br 1 (;@1;)))
      (local.set 0
        (f64.reinterpret_i64
          (local.get 9))))
    (global.set $__stack_pointer
      (i32.add
        (local.get 2)
        (i32.const 144)))
    (local.get 0)
  )
  (func $_ZN4core9panicking11panic_const23panic_const_rem_by_zero17h4d91c9c4a6b3b2e4E (;44;) (type 8)
    (call $_ZN4core9panicking9panic_fmt17h6651313c3e2c6c2fE)
    (unreachable)
  )
  (func $_ZN4core9panicking5panic17h0149fc8f1656305aE (;45;) (type 9) (param i32)
    (call $_ZN4core9panicking9panic_fmt17h6651313c3e2c6c2fE)
    (unreachable)
  )
  (func $libm_fmodf (;46;) (type 3) (param f32 f32) (result f32)
    (local i32 i32 i32 i32 i32 i32 i32 i32 i64 i64 i64)
    (block ;; label = @1
      (block ;; label = @2
        (block ;; label = @3
          (block ;; label = @4
            (block ;; label = @5
              (block ;; label = @6
                (br_if 0 (;@6;)
                  (i32.gt_s
                    (local.tee 3
                      (i32.and
                        (local.tee 2
                          (i32.reinterpret_f32
                            (local.get 0)))
                        (i32.const 2147483647)))
                    (i32.const 2139095039)))
                (br_if 0 (;@6;)
                  (i32.eqz
                    (i32.and
                      (i32.sub
                        (i32.const 0)
                        (local.tee 4
                          (i32.reinterpret_f32
                            (local.get 1))))
                      (i32.const 2139095040))))
                (br_if 5 (;@1;)
                  (i32.lt_u
                    (local.get 3)
                    (local.tee 4
                      (i32.and
                        (local.get 4)
                        (i32.const 2147483647)))))
                (block ;; label = @7
                  (br_if 0 (;@7;)
                    (i32.lt_u
                      (local.tee 5
                        (i32.sub
                          (local.get 3)
                          (i32.and
                            (local.tee 6
                              (select
                                (i32.const 0)
                                (local.tee 5
                                  (i32.add
                                    (local.get 3)
                                    (i32.const -8388608)))
                                (i32.gt_u
                                  (local.get 5)
                                  (local.get 3))))
                            (i32.const 2139095040))))
                      (local.tee 9
                        (i32.shl
                          (local.tee 3
                            (i32.sub
                              (local.get 4)
                              (local.tee 8
                                (i32.and
                                  (local.tee 7
                                    (select
                                      (i32.const 0)
                                      (local.tee 3
                                        (i32.add
                                          (local.get 4)
                                          (i32.const -8388608)))
                                      (i32.gt_u
                                        (local.get 3)
                                        (local.get 4))))
                                  (i32.const 2139095040)))))
                          (i32.const 1)))))
                  (br_if 2 (;@5;)
                    (i32.eq
                      (local.get 4)
                      (local.get 8)))
                  (local.set 5
                    (i32.rem_u
                      (local.get 5)
                      (local.get 3))))
                (local.set 8
                  (i32.and
                    (local.get 2)
                    (i32.const -2147483648)))
                (br_if 2 (;@4;)
                  (i32.ge_u
                    (local.tee 2
                      (i32.sub
                        (i32.shr_u
                          (local.get 6)
                          (i32.const 23))
                        (local.tee 4
                          (i32.shr_u
                            (local.get 7)
                            (i32.const 23)))))
                    (i32.const 32)))
                (br_if 2 (;@4;)
                  (i32.le_u
                    (local.get 3)
                    (i32.wrap_i64
                      (i64.shr_u
                        (local.tee 10
                          (i64.shl
                            (i64.extend_i32_u
                              (local.get 5))
                            (i64.extend_i32_u
                              (local.get 2))))
                        (i64.const 32)))))
                (local.set 3
                  (i32.wrap_i64
                    (i64.rem_u
                      (local.get 10)
                      (i64.extend_i32_u
                        (local.get 3)))))
                (br 3 (;@3;)))
              (return
                (f32.div
                  (local.tee 0
                    (f32.mul
                      (local.get 0)
                      (local.get 1)))
                  (local.get 0))))
            (call $_ZN4core9panicking11panic_const23panic_const_rem_by_zero17h4d91c9c4a6b3b2e4E)
            (unreachable))
          (block ;; label = @4
            (block ;; label = @5
              (block ;; label = @6
                (block ;; label = @7
                  (block ;; label = @8
                    (block ;; label = @9
                      (br_if 0 (;@9;)
                        (i32.ge_u
                          (local.get 3)
                          (i32.const 1073741824)))
                      (br_if 1 (;@8;)
                        (i32.ge_u
                          (local.get 5)
                          (local.get 9)))
                      (block ;; label = @10
                        (br_if 0 (;@10;)
                          (i32.eqz
                            (i32.and
                              (local.get 3)
                              (local.tee 6
                                (i32.add
                                  (local.get 3)
                                  (i32.const -1))))))
                        (br_if 3 (;@7;)
                          (i32.le_u
                            (local.tee 6
                              (i32.shl
                                (local.get 3)
                                (local.tee 7
                                  (i32.add
                                    (i32.clz
                                      (local.get 3))
                                    (i32.const -2)))))
                            (i32.const 536870912)))
                        (br_if 4 (;@6;)
                          (i32.ge_u
                            (local.get 6)
                            (i32.const 1073741824)))
                        (br_if 5 (;@5;)
                          (i32.ge_u
                            (local.get 5)
                            (local.tee 3
                              (i32.shl
                                (local.get 6)
                                (i32.const 1)))))
                        (local.set 12
                          (i64.sub
                            (local.tee 10
                              (i64.shl
                                (i64.extend_i32_u
                                  (i32.sub
                                    (i32.const -2147483648)
                                    (local.get 3)))
                                (i64.const 32)))
                            (i64.mul
                              (local.tee 10
                                (i64.div_u
                                  (local.get 10)
                                  (local.tee 11
                                    (i64.extend_i32_u
                                      (local.get 3)))))
                              (local.get 11))))
                        (local.set 10
                          (i64.mul
                            (i64.or
                              (i64.and
                                (local.get 10)
                                (i64.const 4294967295))
                              (i64.const 4294967296))
                            (i64.extend_i32_u
                              (i32.shl
                                (local.get 5)
                                (i32.const 1)))))
                        (block ;; label = @11
                          (br_if 0 (;@11;)
                            (i32.lt_u
                              (local.tee 3
                                (i32.add
                                  (local.get 7)
                                  (local.get 2)))
                              (i32.const 31)))
                          (loop ;; label = @12
                            (local.set 10
                              (i64.add
                                (i64.and
                                  (i64.shl
                                    (local.get 10)
                                    (i64.const 31))
                                  (i64.const 9223372032559808512))
                                (i64.mul
                                  (local.get 12)
                                  (i64.shr_u
                                    (local.get 10)
                                    (i64.const 32)))))
                            (br_if 0 (;@12;)
                              (i32.gt_u
                                (local.tee 3
                                  (i32.add
                                    (local.get 3)
                                    (i32.const -31)))
                                (i32.const 30)))))
                        (local.set 3
                          (i32.shr_u
                            (i32.sub
                              (local.tee 3
                                (i32.wrap_i64
                                  (i64.shr_u
                                    (i64.mul
                                      (i64.and
                                        (i64.add
                                          (i64.shr_u
                                            (i64.add
                                              (i64.mul
                                                (local.get 12)
                                                (i64.extend_i32_u
                                                  (i32.shr_u
                                                    (i32.wrap_i64
                                                      (i64.shr_u
                                                        (local.get 10)
                                                        (i64.const 32)))
                                                    (i32.xor
                                                      (local.get 3)
                                                      (i32.const 31)))))
                                              (i64.and
                                                (i64.shl
                                                  (local.get 10)
                                                  (i64.extend_i32_u
                                                    (local.get 3)))
                                                (i64.const 9223372036854775807)))
                                            (i64.const 32))
                                          (i64.const 2))
                                        (i64.const 4294967295))
                                      (local.get 11))
                                    (i64.const 32))))
                              (select
                                (i32.const 0)
                                (local.get 6)
                                (i32.gt_u
                                  (local.get 6)
                                  (local.get 3))))
                            (local.get 7)))
                        (br 7 (;@3;)))
                      (br_if 5 (;@4;)
                        (i32.lt_u
                          (local.get 2)
                          (i32.const 32)))
                      (br 7 (;@2;)))
                    (call $_ZN4core9panicking5panic17h0149fc8f1656305aE
                      (i32.const 34))
                    (unreachable))
                  (call $_ZN4core9panicking5panic17h0149fc8f1656305aE
                    (i32.const 30))
                  (unreachable))
                (call $_ZN4core9panicking5panic17h0149fc8f1656305aE
                  (i32.const 43))
                (unreachable))
              (call $_ZN4core9panicking5panic17h0149fc8f1656305aE
                (i32.const 43))
              (unreachable))
            (call $_ZN4core9panicking5panic17h0149fc8f1656305aE
              (i32.const 23))
            (unreachable))
          (local.set 3
            (i32.and
              (i32.shl
                (local.get 5)
                (local.get 2))
              (local.get 6))))
        (br_if 0 (;@2;)
          (i32.eqz
            (local.get 3)))
        (return
          (f32.reinterpret_i32
            (i32.add
              (i32.add
                (i32.shl
                  (i32.sub
                    (local.get 4)
                    (local.tee 2
                      (select
                        (local.tee 2
                          (i32.sub
                            (i32.const 23)
                            (i32.xor
                              (i32.clz
                                (local.get 3))
                              (i32.const 31))))
                        (local.get 4)
                        (i32.lt_u
                          (local.get 2)
                          (local.get 4)))))
                  (i32.const 23))
                (local.get 8))
              (i32.shl
                (local.get 3)
                (local.get 2))))))
      (local.set 0
        (f32.reinterpret_i32
          (local.get 8))))
    (local.get 0)
  )
  (func $libm_hypot (;47;) (type 2) (param f64 f64) (result f64)
    (local i64 i64 i64 i64 f64 f64 f64 f64)
    (local.set 1
      (f64.reinterpret_i64
        (local.tee 4
          (select
            (local.tee 2
              (i64.and
                (i64.reinterpret_f64
                  (local.get 0))
                (i64.const 9223372036854775807)))
            (local.tee 3
              (i64.and
                (i64.reinterpret_f64
                  (local.get 1))
                (i64.const 9223372036854775807)))
            (i64.lt_u
              (local.get 2)
              (local.get 3))))))
    (block ;; label = @1
      (block ;; label = @2
        (br_if 0 (;@2;)
          (i64.eq
            (local.tee 5
              (i64.shr_u
                (local.get 4)
                (i64.const 52)))
            (i64.const 2047)))
        (local.set 0
          (f64.reinterpret_i64
            (local.tee 2
              (select
                (local.get 2)
                (local.get 3)
                (i64.gt_u
                  (local.get 2)
                  (local.get 3))))))
        (br_if 1 (;@1;)
          (i64.eqz
            (local.get 4)))
        (br_if 1 (;@1;)
          (i64.eq
            (local.tee 3
              (i64.shr_u
                (local.get 2)
                (i64.const 52)))
            (i64.const 2047)))
        (block ;; label = @3
          (block ;; label = @4
            (block ;; label = @5
              (br_if 0 (;@5;)
                (i64.gt_s
                  (i64.sub
                    (local.get 3)
                    (local.get 5))
                  (i64.const 64)))
              (br_if 1 (;@4;)
                (i64.gt_u
                  (local.get 2)
                  (i64.const 6908521828386340863)))
              (local.set 6
                (f64.const 0x1p+0 (;=1;)))
              (br_if 2 (;@3;)
                (i64.ge_u
                  (local.get 4)
                  (i64.const 2580562586483294208)))
              (local.set 1
                (f64.mul
                  (local.get 1)
                  (f64.const 0x1p+700 (;=5260135901548374000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000;))))
              (local.set 0
                (f64.mul
                  (local.get 0)
                  (f64.const 0x1p+700 (;=5260135901548374000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000;))))
              (local.set 6
                (f64.const 0x1p-700 (;=0.000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000190109156629516;)))
              (br 2 (;@3;)))
            (return
              (f64.add
                (local.get 0)
                (local.get 1))))
          (local.set 1
            (f64.mul
              (local.get 1)
              (f64.const 0x1p-700 (;=0.000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000190109156629516;))))
          (local.set 0
            (f64.mul
              (local.get 0)
              (f64.const 0x1p-700 (;=0.000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000190109156629516;))))
          (local.set 6
            (f64.const 0x1p+700 (;=5260135901548374000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000;))))
        (local.set 1
          (f64.mul
            (local.get 6)
            (call $_ZN4libm4math4sqrt4sqrt17h3b6b03f022c75fd4E
              (f64.add
                (local.tee 7
                  (f64.mul
                    (local.get 0)
                    (local.get 0)))
                (f64.add
                  (local.tee 8
                    (f64.mul
                      (local.get 1)
                      (local.get 1)))
                  (f64.add
                    (f64.add
                      (f64.mul
                        (local.tee 1
                          (f64.sub
                            (local.get 1)
                            (local.tee 9
                              (f64.add
                                (local.tee 9
                                  (f64.mul
                                    (local.get 1)
                                    (f64.const 0x1.0000002p+27 (;=134217729;))))
                                (f64.sub
                                  (local.get 1)
                                  (local.get 9))))))
                        (local.get 1))
                      (f64.add
                        (f64.sub
                          (f64.mul
                            (local.get 9)
                            (local.get 9))
                          (local.get 8))
                        (f64.mul
                          (f64.add
                            (local.get 9)
                            (local.get 9))
                          (local.get 1))))
                    (f64.add
                      (f64.mul
                        (local.tee 0
                          (f64.sub
                            (local.get 0)
                            (local.tee 1
                              (f64.add
                                (local.tee 1
                                  (f64.mul
                                    (local.get 0)
                                    (f64.const 0x1.0000002p+27 (;=134217729;))))
                                (f64.sub
                                  (local.get 0)
                                  (local.get 1))))))
                        (local.get 0))
                      (f64.add
                        (f64.sub
                          (f64.mul
                            (local.get 1)
                            (local.get 1))
                          (local.get 7))
                        (f64.mul
                          (f64.add
                            (local.get 1)
                            (local.get 1))
                          (local.get 0)))))))))))
      (return
        (local.get 1)))
    (local.get 0)
  )
  (func $libm_hypotf (;48;) (type 3) (param f32 f32) (result f32)
    (local i32 i32 i32 f32 f64)
    (local.set 1
      (f32.reinterpret_i32
        (local.tee 4
          (select
            (local.tee 2
              (i32.and
                (i32.reinterpret_f32
                  (local.get 0))
                (i32.const 2147483647)))
            (local.tee 3
              (i32.and
                (i32.reinterpret_f32
                  (local.get 1))
                (i32.const 2147483647)))
            (i32.lt_u
              (local.get 2)
              (local.get 3))))))
    (block ;; label = @1
      (br_if 0 (;@1;)
        (i32.eq
          (local.get 4)
          (i32.const 2139095040)))
      (local.set 0
        (f32.reinterpret_i32
          (local.tee 2
            (select
              (local.get 2)
              (local.get 3)
              (i32.gt_u
                (local.get 2)
                (local.get 3))))))
      (block ;; label = @2
        (block ;; label = @3
          (br_if 0 (;@3;)
            (i32.gt_u
              (local.get 2)
              (i32.const 2139095039)))
          (br_if 0 (;@3;)
            (i32.eqz
              (local.get 4)))
          (br_if 1 (;@2;)
            (i32.lt_u
              (i32.sub
                (local.get 2)
                (local.get 4))
              (i32.const 209715200))))
        (return
          (f32.add
            (local.get 0)
            (local.get 1))))
      (block ;; label = @2
        (block ;; label = @3
          (br_if 0 (;@3;)
            (i32.gt_u
              (local.get 2)
              (i32.const 1568669695)))
          (local.set 5
            (f32.const 0x1p+0 (;=1;)))
          (br_if 1 (;@2;)
            (i32.ge_u
              (local.get 4)
              (i32.const 562036736)))
          (local.set 1
            (f32.mul
              (local.get 1)
              (f32.const 0x1p+90 (;=1237940100000000000000000000;))))
          (local.set 0
            (f32.mul
              (local.get 0)
              (f32.const 0x1p+90 (;=1237940100000000000000000000;))))
          (local.set 5
            (f32.const 0x1p-90 (;=0.0000000000000000000000000008077936;)))
          (br 1 (;@2;)))
        (local.set 1
          (f32.mul
            (local.get 1)
            (f32.const 0x1p-90 (;=0.0000000000000000000000000008077936;))))
        (local.set 0
          (f32.mul
            (local.get 0)
            (f32.const 0x1p-90 (;=0.0000000000000000000000000008077936;))))
        (local.set 5
          (f32.const 0x1p+90 (;=1237940100000000000000000000;))))
      (local.set 1
        (f32.mul
          (local.get 5)
          (call $_ZN4libm4math4sqrt5sqrtf17h8c1b66187740c44bE
            (f32.demote_f64
              (f64.add
                (f64.mul
                  (local.tee 6
                    (f64.promote_f32
                      (local.get 1)))
                  (local.get 6))
                (f64.mul
                  (local.tee 6
                    (f64.promote_f32
                      (local.get 0)))
                  (local.get 6))))))))
    (local.get 1)
  )
  (func $libm_log (;49;) (type 0) (param f64) (result f64)
    (call $_ZN4libm4math3log3log17h781c40c93ff4fcfcE
      (local.get 0))
  )
  (func $libm_log10 (;50;) (type 0) (param f64) (result f64)
    (local i64 i32 i64 i32 f64 f64 f64 f64 f64 f64)
    (block ;; label = @1
      (block ;; label = @2
        (block ;; label = @3
          (block ;; label = @4
            (br_if 0 (;@4;)
              (i64.lt_s
                (local.tee 1
                  (i64.reinterpret_f64
                    (local.get 0)))
                (i64.const 4503599627370496)))
            (br_if 3 (;@1;)
              (i64.gt_u
                (local.get 1)
                (i64.const 9218868437227405311)))
            (local.set 2
              (i32.const -1023))
            (block ;; label = @5
              (br_if 0 (;@5;)
                (i64.eq
                  (local.tee 3
                    (i64.shr_u
                      (local.get 1)
                      (i64.const 32)))
                  (i64.const 1072693248)))
              (local.set 4
                (i32.wrap_i64
                  (local.get 3)))
              (br 2 (;@3;)))
            (local.set 4
              (i32.const 1072693248))
            (br_if 1 (;@3;)
              (i32.wrap_i64
                (local.get 1)))
            (return
              (f64.const 0x0p+0 (;=0;))))
          (block ;; label = @4
            (br_if 0 (;@4;)
              (f64.ne
                (local.get 0)
                (f64.const 0x0p+0 (;=0;))))
            (return
              (f64.div
                (f64.const -0x1p+0 (;=-1;))
                (f64.mul
                  (local.get 0)
                  (local.get 0)))))
          (br_if 1 (;@2;)
            (i64.lt_s
              (local.get 1)
              (i64.const 0)))
          (local.set 4
            (i32.wrap_i64
              (i64.shr_u
                (local.tee 1
                  (i64.reinterpret_f64
                    (f64.mul
                      (local.get 0)
                      (f64.const 0x1p+54 (;=18014398509481984;)))))
                (i64.const 32))))
          (local.set 2
            (i32.const -1077)))
        (return
          (f64.add
            (local.tee 10
              (f64.add
                (local.tee 6
                  (f64.mul
                    (local.tee 5
                      (f64.convert_i32_s
                        (i32.add
                          (local.get 2)
                          (i32.shr_u
                            (local.tee 4
                              (i32.add
                                (local.get 4)
                                (i32.const 614242)))
                            (i32.const 20)))))
                    (f64.const 0x1.34413509f6p-2 (;=0.30102999566361177;))))
                (local.tee 9
                  (f64.mul
                    (local.tee 8
                      (f64.reinterpret_i64
                        (i64.and
                          (i64.reinterpret_f64
                            (f64.sub
                              (local.tee 0
                                (f64.add
                                  (f64.reinterpret_i64
                                    (i64.or
                                      (i64.shl
                                        (i64.extend_i32_u
                                          (i32.add
                                            (i32.and
                                              (local.get 4)
                                              (i32.const 1048575))
                                            (i32.const 1072079006)))
                                        (i64.const 32))
                                      (i64.and
                                        (local.get 1)
                                        (i64.const 4294967295))))
                                  (f64.const -0x1p+0 (;=-1;))))
                              (local.tee 7
                                (f64.mul
                                  (local.get 0)
                                  (f64.mul
                                    (local.get 0)
                                    (f64.const 0x1p-1 (;=0.5;)))))))
                          (i64.const -4294967296))))
                    (f64.const 0x1.bcb7b152p-2 (;=0.4342944818781689;))))))
            (f64.add
              (f64.add
                (local.get 9)
                (f64.sub
                  (local.get 6)
                  (local.get 10)))
              (f64.add
                (f64.mul
                  (local.tee 0
                    (f64.add
                      (f64.sub
                        (f64.sub
                          (local.get 0)
                          (local.get 8))
                        (local.get 7))
                      (f64.mul
                        (local.tee 0
                          (f64.div
                            (local.get 0)
                            (f64.add
                              (local.get 0)
                              (f64.const 0x1p+1 (;=2;)))))
                        (f64.add
                          (local.get 7)
                          (f64.add
                            (f64.mul
                              (local.tee 0
                                (f64.mul
                                  (local.tee 6
                                    (f64.mul
                                      (local.get 0)
                                      (local.get 0)))
                                  (local.get 6)))
                              (f64.add
                                (f64.mul
                                  (local.get 0)
                                  (f64.add
                                    (f64.mul
                                      (local.get 0)
                                      (f64.const 0x1.39a09d078c69fp-3 (;=0.15313837699209373;)))
                                    (f64.const 0x1.c71c51d8e78afp-3 (;=0.22222198432149784;))))
                                (f64.const 0x1.999999997fa04p-2 (;=0.3999999999940942;))))
                            (f64.mul
                              (local.get 6)
                              (f64.add
                                (f64.mul
                                  (local.get 0)
                                  (f64.add
                                    (f64.mul
                                      (local.get 0)
                                      (f64.add
                                        (f64.mul
                                          (local.get 0)
                                          (f64.const 0x1.2f112df3e5244p-3 (;=0.14798198605116586;)))
                                        (f64.const 0x1.7466496cb03dep-3 (;=0.1818357216161805;))))
                                    (f64.const 0x1.2492494229359p-2 (;=0.2857142874366239;))))
                                (f64.const 0x1.5555555555593p-1 (;=0.6666666666666735;)))))))))
                  (f64.const 0x1.bcb7b152p-2 (;=0.4342944818781689;)))
                (f64.add
                  (f64.mul
                    (local.get 5)
                    (f64.const 0x1.9fef311f12b36p-42 (;=0.0000000000003694239077158931;)))
                  (f64.mul
                    (f64.add
                      (local.get 0)
                      (local.get 8))
                    (f64.const 0x1.b9438ca9aadd5p-36 (;=0.000000000025082946711645275;)))))))))
      (local.set 0
        (f64.div
          (f64.sub
            (local.get 0)
            (local.get 0))
          (f64.const 0x0p+0 (;=0;)))))
    (local.get 0)
  )
  (func $libm_log10f (;51;) (type 1) (param f32) (result f32)
    (local i32 i32 f32 f32 f32)
    (block ;; label = @1
      (block ;; label = @2
        (block ;; label = @3
          (br_if 0 (;@3;)
            (i32.lt_s
              (local.tee 1
                (i32.reinterpret_f32
                  (local.get 0)))
              (i32.const 8388608)))
          (br_if 1 (;@2;)
            (i32.gt_u
              (local.get 1)
              (i32.const 2139095039)))
          (local.set 2
            (i32.const -127))
          (local.set 0
            (f32.const 0x0p+0 (;=0;)))
          (br_if 1 (;@2;)
            (i32.eq
              (local.get 1)
              (i32.const 1065353216)))
          (br 2 (;@1;)))
        (block ;; label = @3
          (br_if 0 (;@3;)
            (f32.ne
              (local.get 0)
              (f32.const 0x0p+0 (;=0;))))
          (return
            (f32.div
              (f32.const -0x1p+0 (;=-1;))
              (f32.mul
                (local.get 0)
                (local.get 0)))))
        (block ;; label = @3
          (br_if 0 (;@3;)
            (i32.lt_s
              (local.get 1)
              (i32.const 0)))
          (local.set 1
            (i32.reinterpret_f32
              (f32.mul
                (local.get 0)
                (f32.const 0x1p+25 (;=33554432;)))))
          (local.set 2
            (i32.const -152))
          (br 2 (;@1;)))
        (local.set 0
          (f32.div
            (f32.sub
              (local.get 0)
              (local.get 0))
            (f32.const 0x0p+0 (;=0;)))))
      (return
        (local.get 0)))
    (f32.add
      (f32.mul
        (local.tee 3
          (f32.convert_i32_s
            (i32.add
              (local.get 2)
              (i32.shr_u
                (local.tee 1
                  (i32.add
                    (local.get 1)
                    (i32.const 4913933)))
                (i32.const 23)))))
        (f32.const 0x1.3441p-2 (;=0.3010292;)))
      (f32.add
        (f32.mul
          (local.tee 5
            (f32.reinterpret_i32
              (i32.and
                (i32.reinterpret_f32
                  (f32.sub
                    (local.tee 0
                      (f32.add
                        (f32.reinterpret_i32
                          (i32.add
                            (i32.and
                              (local.get 1)
                              (i32.const 8388607))
                            (i32.const 1060439283)))
                        (f32.const -0x1p+0 (;=-1;))))
                    (local.tee 4
                      (f32.mul
                        (local.get 0)
                        (f32.mul
                          (local.get 0)
                          (f32.const 0x1p-1 (;=0.5;)))))))
                (i32.const -4096))))
          (f32.const 0x1.bccp-2 (;=0.43432617;)))
        (f32.add
          (f32.mul
            (local.tee 0
              (f32.add
                (f32.sub
                  (f32.sub
                    (local.get 0)
                    (local.get 5))
                  (local.get 4))
                (f32.mul
                  (local.tee 0
                    (f32.div
                      (local.get 0)
                      (f32.add
                        (local.get 0)
                        (f32.const 0x1p+1 (;=2;)))))
                  (f32.add
                    (local.get 4)
                    (f32.add
                      (f32.mul
                        (local.tee 0
                          (f32.mul
                            (local.get 0)
                            (local.get 0)))
                        (f32.add
                          (f32.mul
                            (local.tee 0
                              (f32.mul
                                (local.get 0)
                                (local.get 0)))
                            (f32.const 0x1.23d3dcp-2 (;=0.28498787;)))
                          (f32.const 0x1.555554p-1 (;=0.6666666;))))
                      (f32.mul
                        (local.get 0)
                        (f32.add
                          (f32.mul
                            (local.get 0)
                            (f32.const 0x1.f13c4cp-3 (;=0.24279079;)))
                          (f32.const 0x1.999c26p-2 (;=0.40000972;)))))))))
            (f32.const 0x1.bccp-2 (;=0.43432617;)))
          (f32.add
            (f32.mul
              (local.get 3)
              (f32.const 0x1.a84fb6p-21 (;=0.0000007903415;)))
            (f32.mul
              (f32.add
                (local.get 0)
                (local.get 5))
              (f32.const -0x1.09d5b2p-15 (;=-0.00003168997;)))))))
  )
  (func $libm_log1p (;52;) (type 0) (param f64) (result f64)
    (call $_ZN4libm4math5log1p5log1p17h5d4b372f78bb46e9E
      (local.get 0))
  )
  (func $libm_log1pf (;53;) (type 1) (param f32) (result f32)
    (call $_ZN4libm4math6log1pf6log1pf17h4c967d18f426e9e4E
      (local.get 0))
  )
  (func $libm_log2 (;54;) (type 0) (param f64) (result f64)
    (local i64 i32 i64 i32 f64 f64 f64 f64 f64)
    (block ;; label = @1
      (block ;; label = @2
        (block ;; label = @3
          (block ;; label = @4
            (br_if 0 (;@4;)
              (i64.lt_s
                (local.tee 1
                  (i64.reinterpret_f64
                    (local.get 0)))
                (i64.const 4503599627370496)))
            (br_if 3 (;@1;)
              (i64.gt_u
                (local.get 1)
                (i64.const 9218868437227405311)))
            (local.set 2
              (i32.const -1023))
            (block ;; label = @5
              (br_if 0 (;@5;)
                (i64.eq
                  (local.tee 3
                    (i64.shr_u
                      (local.get 1)
                      (i64.const 32)))
                  (i64.const 1072693248)))
              (local.set 4
                (i32.wrap_i64
                  (local.get 3)))
              (br 2 (;@3;)))
            (local.set 4
              (i32.const 1072693248))
            (br_if 1 (;@3;)
              (i32.wrap_i64
                (local.get 1)))
            (return
              (f64.const 0x0p+0 (;=0;))))
          (block ;; label = @4
            (br_if 0 (;@4;)
              (f64.ne
                (local.get 0)
                (f64.const 0x0p+0 (;=0;))))
            (return
              (f64.div
                (f64.const -0x1p+0 (;=-1;))
                (f64.mul
                  (local.get 0)
                  (local.get 0)))))
          (br_if 1 (;@2;)
            (i64.lt_s
              (local.get 1)
              (i64.const 0)))
          (local.set 4
            (i32.wrap_i64
              (i64.shr_u
                (local.tee 1
                  (i64.reinterpret_f64
                    (f64.mul
                      (local.get 0)
                      (f64.const 0x1p+54 (;=18014398509481984;)))))
                (i64.const 32))))
          (local.set 2
            (i32.const -1077)))
        (return
          (f64.add
            (local.tee 9
              (f64.add
                (local.tee 7
                  (f64.mul
                    (local.tee 6
                      (f64.reinterpret_i64
                        (i64.and
                          (i64.reinterpret_f64
                            (f64.sub
                              (local.tee 0
                                (f64.add
                                  (f64.reinterpret_i64
                                    (i64.or
                                      (i64.shl
                                        (i64.extend_i32_u
                                          (i32.add
                                            (i32.and
                                              (local.tee 4
                                                (i32.add
                                                  (local.get 4)
                                                  (i32.const 614242)))
                                              (i32.const 1048575))
                                            (i32.const 1072079006)))
                                        (i64.const 32))
                                      (i64.and
                                        (local.get 1)
                                        (i64.const 4294967295))))
                                  (f64.const -0x1p+0 (;=-1;))))
                              (local.tee 5
                                (f64.mul
                                  (local.get 0)
                                  (f64.mul
                                    (local.get 0)
                                    (f64.const 0x1p-1 (;=0.5;)))))))
                          (i64.const -4294967296))))
                    (f64.const 0x1.71547652p+0 (;=1.4426950407214463;))))
                (local.tee 8
                  (f64.convert_i32_s
                    (i32.add
                      (local.get 2)
                      (i32.shr_u
                        (local.get 4)
                        (i32.const 20)))))))
            (f64.add
              (f64.add
                (local.get 7)
                (f64.sub
                  (local.get 8)
                  (local.get 9)))
              (f64.add
                (f64.mul
                  (local.tee 0
                    (f64.add
                      (f64.sub
                        (f64.sub
                          (local.get 0)
                          (local.get 6))
                        (local.get 5))
                      (f64.mul
                        (local.tee 0
                          (f64.div
                            (local.get 0)
                            (f64.add
                              (local.get 0)
                              (f64.const 0x1p+1 (;=2;)))))
                        (f64.add
                          (local.get 5)
                          (f64.add
                            (f64.mul
                              (local.tee 0
                                (f64.mul
                                  (local.tee 7
                                    (f64.mul
                                      (local.get 0)
                                      (local.get 0)))
                                  (local.get 7)))
                              (f64.add
                                (f64.mul
                                  (local.get 0)
                                  (f64.add
                                    (f64.mul
                                      (local.get 0)
                                      (f64.const 0x1.39a09d078c69fp-3 (;=0.15313837699209373;)))
                                    (f64.const 0x1.c71c51d8e78afp-3 (;=0.22222198432149784;))))
                                (f64.const 0x1.999999997fa04p-2 (;=0.3999999999940942;))))
                            (f64.mul
                              (local.get 7)
                              (f64.add
                                (f64.mul
                                  (local.get 0)
                                  (f64.add
                                    (f64.mul
                                      (local.get 0)
                                      (f64.add
                                        (f64.mul
                                          (local.get 0)
                                          (f64.const 0x1.2f112df3e5244p-3 (;=0.14798198605116586;)))
                                        (f64.const 0x1.7466496cb03dep-3 (;=0.1818357216161805;))))
                                    (f64.const 0x1.2492494229359p-2 (;=0.2857142874366239;))))
                                (f64.const 0x1.5555555555593p-1 (;=0.6666666666666735;)))))))))
                  (f64.const 0x1.71547652p+0 (;=1.4426950407214463;)))
                (f64.mul
                  (f64.add
                    (local.get 0)
                    (local.get 6))
                  (f64.const 0x1.705fc2eefa2p-33 (;=0.00000000016751713164886512;))))))))
      (local.set 0
        (f64.div
          (f64.sub
            (local.get 0)
            (local.get 0))
          (f64.const 0x0p+0 (;=0;)))))
    (local.get 0)
  )
  (func $libm_log2f (;55;) (type 1) (param f32) (result f32)
    (local i32 i32 f32 f32)
    (block ;; label = @1
      (block ;; label = @2
        (block ;; label = @3
          (br_if 0 (;@3;)
            (i32.lt_s
              (local.tee 1
                (i32.reinterpret_f32
                  (local.get 0)))
              (i32.const 8388608)))
          (br_if 1 (;@2;)
            (i32.gt_u
              (local.get 1)
              (i32.const 2139095039)))
          (local.set 2
            (i32.const -127))
          (local.set 0
            (f32.const 0x0p+0 (;=0;)))
          (br_if 1 (;@2;)
            (i32.eq
              (local.get 1)
              (i32.const 1065353216)))
          (br 2 (;@1;)))
        (block ;; label = @3
          (br_if 0 (;@3;)
            (f32.ne
              (local.get 0)
              (f32.const 0x0p+0 (;=0;))))
          (return
            (f32.div
              (f32.const -0x1p+0 (;=-1;))
              (f32.mul
                (local.get 0)
                (local.get 0)))))
        (block ;; label = @3
          (br_if 0 (;@3;)
            (i32.lt_s
              (local.get 1)
              (i32.const 0)))
          (local.set 1
            (i32.reinterpret_f32
              (f32.mul
                (local.get 0)
                (f32.const 0x1p+25 (;=33554432;)))))
          (local.set 2
            (i32.const -152))
          (br 2 (;@1;)))
        (local.set 0
          (f32.div
            (f32.sub
              (local.get 0)
              (local.get 0))
            (f32.const 0x0p+0 (;=0;)))))
      (return
        (local.get 0)))
    (f32.add
      (f32.add
        (f32.mul
          (local.tee 4
            (f32.reinterpret_i32
              (i32.and
                (i32.reinterpret_f32
                  (f32.sub
                    (local.tee 0
                      (f32.add
                        (f32.reinterpret_i32
                          (i32.add
                            (i32.and
                              (local.tee 1
                                (i32.add
                                  (local.get 1)
                                  (i32.const 4913933)))
                              (i32.const 8388607))
                            (i32.const 1060439283)))
                        (f32.const -0x1p+0 (;=-1;))))
                    (local.tee 3
                      (f32.mul
                        (local.get 0)
                        (f32.mul
                          (local.get 0)
                          (f32.const 0x1p-1 (;=0.5;)))))))
                (i32.const -4096))))
          (f32.const 0x1.716p+0 (;=1.4428711;)))
        (f32.add
          (f32.mul
            (local.tee 0
              (f32.add
                (f32.sub
                  (f32.sub
                    (local.get 0)
                    (local.get 4))
                  (local.get 3))
                (f32.mul
                  (local.tee 0
                    (f32.div
                      (local.get 0)
                      (f32.add
                        (local.get 0)
                        (f32.const 0x1p+1 (;=2;)))))
                  (f32.add
                    (local.get 3)
                    (f32.add
                      (f32.mul
                        (local.tee 0
                          (f32.mul
                            (local.get 0)
                            (local.get 0)))
                        (f32.add
                          (f32.mul
                            (local.tee 0
                              (f32.mul
                                (local.get 0)
                                (local.get 0)))
                            (f32.const 0x1.23d3dcp-2 (;=0.28498787;)))
                          (f32.const 0x1.555554p-1 (;=0.6666666;))))
                      (f32.mul
                        (local.get 0)
                        (f32.add
                          (f32.mul
                            (local.get 0)
                            (f32.const 0x1.f13c4cp-3 (;=0.24279079;)))
                          (f32.const 0x1.999c26p-2 (;=0.40000972;)))))))))
            (f32.const 0x1.716p+0 (;=1.4428711;)))
          (f32.mul
            (f32.add
              (local.get 0)
              (local.get 4))
            (f32.const -0x1.7135a8p-13 (;=-0.00017605285;)))))
      (f32.convert_i32_s
        (i32.add
          (local.get 2)
          (i32.shr_u
            (local.get 1)
            (i32.const 23)))))
  )
  (func $libm_logf (;56;) (type 1) (param f32) (result f32)
    (call $_ZN4libm4math4logf4logf17h7b88872ed73a994aE
      (local.get 0))
  )
  (func $libm_pow (;57;) (type 2) (param f64 f64) (result f64)
    (local f64 i64 i32 i32 i32 i64 i32 i64 i32 i32 i32 i32 i32 f64 f64 f64 f64)
    (local.set 2
      (f64.const 0x1p+0 (;=1;)))
    (block ;; label = @1
      (br_if 0 (;@1;)
        (i32.eqz
          (i32.or
            (local.tee 5
              (i32.and
                (local.tee 4
                  (i32.wrap_i64
                    (i64.shr_u
                      (local.tee 3
                        (i64.reinterpret_f64
                          (local.get 1)))
                      (i64.const 32))))
                (i32.const 2147483647)))
            (local.tee 6
              (i32.wrap_i64
                (local.get 3))))))
      (local.set 8
        (i32.wrap_i64
          (local.tee 7
            (i64.reinterpret_f64
              (local.get 0)))))
      (block ;; label = @2
        (br_if 0 (;@2;)
          (i64.ne
            (local.tee 9
              (i64.shr_u
                (local.get 7)
                (i64.const 32)))
            (i64.const 1072693248)))
        (br_if 1 (;@1;)
          (i32.eqz
            (local.get 8))))
      (block ;; label = @2
        (block ;; label = @3
          (block ;; label = @4
            (block ;; label = @5
              (block ;; label = @6
                (block ;; label = @7
                  (br_if 0 (;@7;)
                    (i32.gt_u
                      (local.tee 11
                        (i32.and
                          (local.tee 10
                            (i32.wrap_i64
                              (local.get 9)))
                          (i32.const 2147483647)))
                      (i32.const 2146435072)))
                  (block ;; label = @8
                    (block ;; label = @9
                      (br_if 0 (;@9;)
                        (i32.ne
                          (local.get 11)
                          (i32.const 2146435072)))
                      (br_if 2 (;@7;)
                        (local.get 8))
                      (br_if 2 (;@7;)
                        (i32.gt_u
                          (local.get 5)
                          (i32.const 2146435072)))
                      (br 1 (;@8;)))
                    (br_if 1 (;@7;)
                      (i32.ge_u
                        (local.get 5)
                        (i32.const 2146435073))))
                  (block ;; label = @8
                    (br_if 0 (;@8;)
                      (i32.eqz
                        (local.get 6)))
                    (br_if 1 (;@7;)
                      (i32.eq
                        (local.get 5)
                        (i32.const 2146435072))))
                  (br_if 1 (;@6;)
                    (i64.lt_s
                      (local.get 7)
                      (i64.const 0)))
                  (br 2 (;@5;)))
                (return
                  (f64.add
                    (local.get 0)
                    (local.get 1))))
              (local.set 12
                (i32.const 2))
              (br_if 1 (;@4;)
                (i32.gt_u
                  (local.get 5)
                  (i32.const 1128267775)))
              (br_if 0 (;@5;)
                (i32.lt_u
                  (local.get 5)
                  (i32.const 1072693248)))
              (local.set 13
                (i32.shr_u
                  (local.get 5)
                  (i32.const 20)))
              (block ;; label = @6
                (br_if 0 (;@6;)
                  (i32.gt_u
                    (local.get 5)
                    (i32.const 1094713343)))
                (local.set 12
                  (i32.const 0))
                (br_if 4 (;@2;)
                  (local.get 6))
                (local.set 12
                  (i32.const 0))
                (br_if 3 (;@3;)
                  (i32.ne
                    (i32.shl
                      (local.tee 13
                        (i32.shr_u
                          (local.get 5)
                          (local.tee 6
                            (i32.sub
                              (i32.const 1043)
                              (local.get 13)))))
                      (local.get 6))
                    (local.get 5)))
                (local.set 12
                  (i32.sub
                    (i32.const 2)
                    (i32.and
                      (local.get 13)
                      (i32.const 1))))
                (br 3 (;@3;)))
              (local.set 12
                (i32.const 0))
              (br_if 1 (;@4;)
                (i32.ne
                  (i32.shl
                    (local.tee 14
                      (i32.shr_u
                        (local.get 6)
                        (local.tee 13
                          (i32.sub
                            (i32.const 1075)
                            (local.get 13)))))
                    (local.get 13))
                  (local.get 6)))
              (local.set 12
                (i32.sub
                  (i32.const 2)
                  (i32.and
                    (local.get 14)
                    (i32.const 1))))
              (br 1 (;@4;)))
            (local.set 12
              (i32.const 0)))
          (br_if 1 (;@2;)
            (local.get 6)))
        (block ;; label = @3
          (block ;; label = @4
            (block ;; label = @5
              (block ;; label = @6
                (block ;; label = @7
                  (block ;; label = @8
                    (br_if 0 (;@8;)
                      (i32.eq
                        (local.get 5)
                        (i32.const 1072693248)))
                    (br_if 1 (;@7;)
                      (i32.ne
                        (local.get 5)
                        (i32.const 2146435072)))
                    (br_if 7 (;@1;)
                      (i32.eqz
                        (i32.or
                          (i32.add
                            (local.get 11)
                            (i32.const -1072693248))
                          (local.get 8))))
                    (br_if 5 (;@3;)
                      (i32.gt_u
                        (local.get 11)
                        (i32.const 1072693247)))
                    (return
                      (select
                        (f64.const 0x0p+0 (;=0;))
                        (f64.neg
                          (local.get 1))
                        (i64.gt_s
                          (local.get 3)
                          (i64.const -1)))))
                  (br_if 1 (;@6;)
                    (i64.le_s
                      (local.get 3)
                      (i64.const -1)))
                  (return
                    (local.get 0)))
                (br_if 2 (;@4;)
                  (i32.eq
                    (local.get 4)
                    (i32.const 1071644672)))
                (br_if 1 (;@5;)
                  (i32.eq
                    (local.get 4)
                    (i32.const 1073741824)))
                (br 4 (;@2;)))
              (return
                (f64.div
                  (f64.const 0x1p+0 (;=1;))
                  (local.get 0))))
            (return
              (f64.mul
                (local.get 0)
                (local.get 0))))
          (br_if 1 (;@2;)
            (i64.lt_s
              (local.get 7)
              (i64.const 0)))
          (return
            (call $_ZN4libm4math4sqrt4sqrt17h3b6b03f022c75fd4E
              (local.get 0))))
        (return
          (select
            (local.get 1)
            (f64.const 0x0p+0 (;=0;))
            (i64.gt_s
              (local.get 3)
              (i64.const -1)))))
      (local.set 2
        (f64.abs
          (local.get 0)))
      (block ;; label = @2
        (block ;; label = @3
          (br_if 0 (;@3;)
            (local.get 8))
          (block ;; label = @4
            (br_if 0 (;@4;)
              (i32.gt_s
                (local.get 10)
                (i32.const -1)))
            (br_if 2 (;@2;)
              (i32.eq
                (local.get 10)
                (i32.const -2147483648)))
            (br_if 2 (;@2;)
              (i32.eq
                (local.get 10)
                (i32.const -1074790400)))
            (br_if 1 (;@3;)
              (i32.ne
                (local.get 10)
                (i32.const -1048576)))
            (br 2 (;@2;)))
          (br_if 1 (;@2;)
            (i32.eqz
              (local.get 10)))
          (br_if 1 (;@2;)
            (i32.eq
              (local.get 10)
              (i32.const 1072693248)))
          (br_if 1 (;@2;)
            (i32.eq
              (local.get 10)
              (i32.const 2146435072))))
        (local.set 15
          (f64.const 0x1p+0 (;=1;)))
        (block ;; label = @3
          (br_if 0 (;@3;)
            (i64.ge_s
              (local.get 7)
              (i64.const 0)))
          (block ;; label = @4
            (block ;; label = @5
              (br_table 0 (;@5;) 1 (;@4;) 2 (;@3;)
                (local.get 12)))
            (return
              (f64.div
                (local.tee 1
                  (f64.sub
                    (local.get 0)
                    (local.get 0)))
                (local.get 1))))
          (local.set 15
            (f64.const -0x1p+0 (;=-1;))))
        (block ;; label = @3
          (block ;; label = @4
            (br_if 0 (;@4;)
              (i32.gt_u
                (local.get 5)
                (i32.const 1105199104)))
            (local.set 2
              (select
                (local.tee 0
                  (f64.mul
                    (local.get 2)
                    (f64.const 0x1p+53 (;=9007199254740992;))))
                (local.get 2)
                (local.tee 8
                  (i32.lt_u
                    (local.get 11)
                    (i32.const 1048576)))))
            (local.set 5
              (i32.or
                (local.tee 6
                  (i32.and
                    (local.tee 4
                      (select
                        (i32.wrap_i64
                          (i64.shr_u
                            (i64.reinterpret_f64
                              (local.get 0))
                            (i64.const 32)))
                        (local.get 11)
                        (local.get 8)))
                    (i32.const 1048575)))
                (i32.const 1072693248)))
            (local.set 4
              (i32.add
                (select
                  (i32.const -1076)
                  (i32.const -1023)
                  (local.get 8))
                (i32.shr_s
                  (local.get 4)
                  (i32.const 20))))
            (local.set 8
              (i32.const 0))
            (block ;; label = @5
              (br_if 0 (;@5;)
                (i32.lt_u
                  (local.get 6)
                  (i32.const 235663)))
              (block ;; label = @6
                (br_if 0 (;@6;)
                  (i32.ge_u
                    (local.get 6)
                    (i32.const 767610)))
                (local.set 8
                  (i32.const 1))
                (br 1 (;@5;)))
              (local.set 5
                (i32.or
                  (local.get 6)
                  (i32.const 1071644672)))
              (local.set 4
                (i32.add
                  (local.get 4)
                  (i32.const 1))))
            (local.set 17
              (f64.sub
                (local.tee 2
                  (f64.add
                    (f64.load offset=1053504
                      (local.tee 6
                        (i32.shl
                          (local.get 8)
                          (i32.const 3))))
                    (f64.add
                      (f64.mul
                        (f64.sub
                          (local.tee 17
                            (f64.add
                              (f64.mul
                                (local.tee 0
                                  (f64.mul
                                    (local.tee 2
                                      (f64.div
                                        (f64.const 0x1p+0 (;=1;))
                                        (f64.add
                                          (local.tee 0
                                            (f64.load offset=1053488
                                              (local.get 6)))
                                          (local.tee 16
                                            (f64.reinterpret_i64
                                              (i64.or
                                                (i64.shl
                                                  (i64.extend_i32_u
                                                    (local.get 5))
                                                  (i64.const 32))
                                                (i64.and
                                                  (i64.reinterpret_f64
                                                    (local.get 2))
                                                  (i64.const 4294967295))))))))
                                    (f64.sub
                                      (f64.sub
                                        (local.tee 17
                                          (f64.sub
                                            (local.get 16)
                                            (local.get 0)))
                                        (f64.mul
                                          (local.tee 18
                                            (f64.reinterpret_i64
                                              (i64.shl
                                                (i64.extend_i32_u
                                                  (i32.add
                                                    (i32.add
                                                      (i32.shl
                                                        (local.get 8)
                                                        (i32.const 18))
                                                      (i32.shr_u
                                                        (local.get 5)
                                                        (i32.const 1)))
                                                    (i32.const 537395200)))
                                                (i64.const 32))))
                                          (local.tee 2
                                            (f64.reinterpret_i64
                                              (i64.and
                                                (i64.reinterpret_f64
                                                  (local.tee 17
                                                    (f64.mul
                                                      (local.get 17)
                                                      (local.get 2))))
                                                (i64.const -4294967296))))))
                                      (f64.mul
                                        (f64.add
                                          (f64.sub
                                            (local.get 0)
                                            (local.get 18))
                                          (local.get 16))
                                        (local.get 2)))))
                                (local.tee 0
                                  (f64.reinterpret_i64
                                    (i64.and
                                      (i64.reinterpret_f64
                                        (f64.add
                                          (f64.add
                                            (local.tee 16
                                              (f64.mul
                                                (local.get 2)
                                                (local.get 2)))
                                            (f64.const 0x1.8p+1 (;=3;)))
                                          (local.tee 18
                                            (f64.add
                                              (f64.mul
                                                (local.get 0)
                                                (f64.add
                                                  (local.get 17)
                                                  (local.get 2)))
                                              (f64.mul
                                                (f64.mul
                                                  (local.tee 0
                                                    (f64.mul
                                                      (local.get 17)
                                                      (local.get 17)))
                                                  (local.get 0))
                                                (f64.add
                                                  (f64.mul
                                                    (local.get 0)
                                                    (f64.add
                                                      (f64.mul
                                                        (local.get 0)
                                                        (f64.add
                                                          (f64.mul
                                                            (local.get 0)
                                                            (f64.add
                                                              (f64.mul
                                                                (local.get 0)
                                                                (f64.add
                                                                  (f64.mul
                                                                    (local.get 0)
                                                                    (f64.const 0x1.a7e284a454eefp-3 (;=0.20697501780033842;)))
                                                                  (f64.const 0x1.d864a93c9db65p-3 (;=0.23066074577556175;))))
                                                              (f64.const 0x1.17460a91d4101p-2 (;=0.272728123808534;))))
                                                          (f64.const 0x1.55555518f264dp-2 (;=0.33333332981837743;))))
                                                      (f64.const 0x1.b6db6db6fabffp-2 (;=0.4285714285785502;))))
                                                  (f64.const 0x1.3333333333303p-1 (;=0.5999999999999946;))))))))
                                      (i64.const -4294967296)))))
                              (f64.mul
                                (local.get 17)
                                (f64.sub
                                  (local.get 18)
                                  (f64.sub
                                    (f64.add
                                      (local.get 0)
                                      (f64.const -0x1.8p+1 (;=-3;)))
                                    (local.get 16))))))
                          (f64.sub
                            (local.tee 0
                              (f64.reinterpret_i64
                                (i64.and
                                  (i64.reinterpret_f64
                                    (f64.add
                                      (local.get 17)
                                      (local.tee 2
                                        (f64.mul
                                          (local.get 2)
                                          (local.get 0)))))
                                  (i64.const -4294967296))))
                            (local.get 2)))
                        (f64.const 0x1.ec709dc3a03fdp-1 (;=0.9617966939259756;)))
                      (f64.mul
                        (local.get 0)
                        (f64.const -0x1.e2fe0145b01f5p-28 (;=-0.000000007028461650952758;))))))
                (f64.sub
                  (f64.sub
                    (f64.sub
                      (local.tee 0
                        (f64.reinterpret_i64
                          (i64.and
                            (i64.reinterpret_f64
                              (f64.add
                                (f64.add
                                  (local.tee 17
                                    (f64.load offset=1053520
                                      (local.get 6)))
                                  (f64.add
                                    (local.get 2)
                                    (local.tee 16
                                      (f64.mul
                                        (local.get 0)
                                        (f64.const 0x1.ec709ep-1 (;=0.9617967009544373;))))))
                                (local.tee 2
                                  (f64.convert_i32_s
                                    (local.get 4)))))
                            (i64.const -4294967296))))
                      (local.get 2))
                    (local.get 17))
                  (local.get 16))))
            (br 1 (;@3;)))
          (block ;; label = @4
            (block ;; label = @5
              (block ;; label = @6
                (br_if 0 (;@6;)
                  (i32.gt_u
                    (local.get 5)
                    (i32.const 1139802112)))
                (br_if 2 (;@4;)
                  (i32.lt_u
                    (local.get 11)
                    (i32.const 1072693247)))
                (br_if 1 (;@5;)
                  (i32.gt_u
                    (local.get 11)
                    (i32.const 1072693248)))
                (local.set 17
                  (f64.sub
                    (local.tee 2
                      (f64.add
                        (f64.mul
                          (local.tee 0
                            (f64.add
                              (local.get 2)
                              (f64.const -0x1p+0 (;=-1;))))
                          (f64.const 0x1.4ae0bf85ddf44p-26 (;=0.000000019259629911266175;)))
                        (f64.mul
                          (f64.mul
                            (f64.mul
                              (local.get 0)
                              (local.get 0))
                            (f64.sub
                              (f64.const 0x1p-1 (;=0.5;))
                              (f64.mul
                                (local.get 0)
                                (f64.add
                                  (f64.mul
                                    (local.get 0)
                                    (f64.const -0x1p-2 (;=-0.25;)))
                                  (f64.const 0x1.5555555555555p-2 (;=0.3333333333333333;))))))
                          (f64.const -0x1.71547652b82fep+0 (;=-1.4426950408889634;)))))
                    (f64.sub
                      (local.tee 0
                        (f64.reinterpret_i64
                          (i64.and
                            (i64.reinterpret_f64
                              (f64.add
                                (local.get 2)
                                (local.tee 17
                                  (f64.mul
                                    (local.get 0)
                                    (f64.const 0x1.715476p+0 (;=1.4426950216293335;))))))
                            (i64.const -4294967296))))
                      (local.get 17))))
                (br 3 (;@3;)))
              (block ;; label = @6
                (br_if 0 (;@6;)
                  (i32.gt_u
                    (local.get 11)
                    (i32.const 1072693247)))
                (return
                  (select
                    (f64.const inf (;=inf;))
                    (f64.const 0x0p+0 (;=0;))
                    (i64.lt_s
                      (local.get 3)
                      (i64.const 0)))))
              (return
                (select
                  (f64.const inf (;=inf;))
                  (f64.const 0x0p+0 (;=0;))
                  (i32.gt_s
                    (local.get 4)
                    (i32.const 0)))))
            (block ;; label = @5
              (br_if 0 (;@5;)
                (i32.gt_s
                  (local.get 4)
                  (i32.const 0)))
              (return
                (f64.mul
                  (f64.mul
                    (local.get 15)
                    (f64.const 0x1.56e1fc2f8f359p-997 (;=0.000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000001;)))
                  (f64.const 0x1.56e1fc2f8f359p-997 (;=0.000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000001;)))))
            (return
              (f64.mul
                (f64.mul
                  (local.get 15)
                  (f64.const 0x1.7e43c8800759cp+996 (;=1000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000;)))
                (f64.const 0x1.7e43c8800759cp+996 (;=1000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000;)))))
          (block ;; label = @4
            (br_if 0 (;@4;)
              (i64.lt_s
                (local.get 3)
                (i64.const 0)))
            (return
              (f64.mul
                (f64.mul
                  (local.get 15)
                  (f64.const 0x1.56e1fc2f8f359p-997 (;=0.000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000001;)))
                (f64.const 0x1.56e1fc2f8f359p-997 (;=0.000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000001;)))))
          (local.set 2
            (f64.mul
              (f64.mul
                (local.get 15)
                (f64.const 0x1.7e43c8800759cp+996 (;=1000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000;)))
              (f64.const 0x1.7e43c8800759cp+996 (;=1000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000;))))
          (br 2 (;@1;)))
        (local.set 8
          (i32.wrap_i64
            (local.tee 3
              (i64.reinterpret_f64
                (local.tee 0
                  (f64.add
                    (local.tee 2
                      (f64.mul
                        (local.get 0)
                        (local.tee 16
                          (f64.reinterpret_i64
                            (i64.and
                              (local.get 3)
                              (i64.const -4294967296))))))
                    (local.tee 1
                      (f64.add
                        (f64.mul
                          (f64.sub
                            (local.get 1)
                            (local.get 16))
                          (local.get 0))
                        (f64.mul
                          (local.get 1)
                          (local.get 17))))))))))
        (block ;; label = @3
          (block ;; label = @4
            (block ;; label = @5
              (br_if 0 (;@5;)
                (i32.gt_s
                  (local.tee 5
                    (i32.wrap_i64
                      (i64.shr_u
                        (local.get 3)
                        (i64.const 32))))
                  (i32.const 1083179007)))
              (br_if 2 (;@3;)
                (i32.le_u
                  (i32.and
                    (local.get 5)
                    (i32.const 2147482624))
                  (i32.const 1083231231)))
              (br_if 1 (;@4;)
                (i32.or
                  (i32.add
                    (local.get 5)
                    (i32.const 1064252416))
                  (local.get 8)))
              (br_if 2 (;@3;)
                (i32.eqz
                  (f64.le
                    (local.get 1)
                    (f64.sub
                      (local.get 0)
                      (local.get 2)))))
              (return
                (f64.mul
                  (f64.mul
                    (local.get 15)
                    (f64.const 0x1.56e1fc2f8f359p-997 (;=0.000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000001;)))
                  (f64.const 0x1.56e1fc2f8f359p-997 (;=0.000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000001;)))))
            (block ;; label = @5
              (br_if 0 (;@5;)
                (i32.eqz
                  (i32.or
                    (i32.add
                      (local.get 5)
                      (i32.const -1083179008))
                    (local.get 8))))
              (return
                (f64.mul
                  (f64.mul
                    (local.get 15)
                    (f64.const 0x1.7e43c8800759cp+996 (;=1000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000;)))
                  (f64.const 0x1.7e43c8800759cp+996 (;=1000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000;)))))
            (br_if 1 (;@3;)
              (i32.eqz
                (f64.gt
                  (f64.add
                    (local.get 1)
                    (f64.const 0x1.71547652b82fep-54 (;=0.00000000000000008008566259537294;)))
                  (f64.sub
                    (local.get 0)
                    (local.get 2)))))
            (return
              (f64.mul
                (f64.mul
                  (local.get 15)
                  (f64.const 0x1.7e43c8800759cp+996 (;=1000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000;)))
                (f64.const 0x1.7e43c8800759cp+996 (;=1000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000;)))))
          (return
            (f64.mul
              (f64.mul
                (local.get 15)
                (f64.const 0x1.56e1fc2f8f359p-997 (;=0.000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000001;)))
              (f64.const 0x1.56e1fc2f8f359p-997 (;=0.000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000001;)))))
        (local.set 8
          (i32.const 0))
        (block ;; label = @3
          (br_if 0 (;@3;)
            (i32.le_u
              (i32.and
                (local.get 5)
                (i32.const 2147483647))
              (i32.const 1071644672)))
          (local.set 8
            (select
              (i32.sub
                (i32.const 0)
                (local.tee 8
                  (i32.shr_u
                    (i32.or
                      (i32.and
                        (local.tee 5
                          (i32.add
                            (i32.shr_u
                              (i32.const 1048576)
                              (i32.add
                                (i32.shr_u
                                  (local.get 5)
                                  (i32.const 20))
                                (i32.const 2)))
                            (local.get 5)))
                        (i32.const 1048575))
                      (i32.const 1048576))
                    (i32.sub
                      (i32.const 19)
                      (local.tee 6
                        (i32.shr_u
                          (local.get 5)
                          (i32.const 20)))))))
              (local.get 8)
              (i64.lt_s
                (local.get 3)
                (i64.const 0))))
          (local.set 3
            (i64.reinterpret_f64
              (f64.add
                (local.get 1)
                (local.tee 2
                  (f64.sub
                    (local.get 2)
                    (f64.reinterpret_i64
                      (i64.shl
                        (i64.extend_i32_u
                          (i32.and
                            (i32.shr_s
                              (i32.const -1048576)
                              (i32.add
                                (local.get 6)
                                (i32.const 1)))
                            (local.get 5)))
                        (i64.const 32)))))))))
        (block ;; label = @3
          (block ;; label = @4
            (br_if 0 (;@4;)
              (i32.lt_s
                (local.tee 5
                  (i32.add
                    (i32.shl
                      (local.get 8)
                      (i32.const 20))
                    (i32.wrap_i64
                      (i64.shr_u
                        (local.tee 3
                          (i64.reinterpret_f64
                            (local.tee 1
                              (f64.add
                                (f64.sub
                                  (local.tee 1
                                    (f64.add
                                      (local.tee 17
                                        (f64.mul
                                          (local.tee 0
                                            (f64.reinterpret_i64
                                              (i64.and
                                                (local.get 3)
                                                (i64.const -4294967296))))
                                          (f64.const 0x1.62e43p-1 (;=0.6931471824645996;))))
                                      (local.tee 2
                                        (f64.add
                                          (f64.mul
                                            (f64.sub
                                              (local.get 1)
                                              (f64.sub
                                                (local.get 0)
                                                (local.get 2)))
                                            (f64.const 0x1.62e42fefa39efp-1 (;=0.6931471805599453;)))
                                          (f64.mul
                                            (local.get 0)
                                            (f64.const -0x1.05c610ca86c39p-29 (;=-0.000000001904654299957768;)))))))
                                  (f64.sub
                                    (f64.div
                                      (f64.mul
                                        (local.get 1)
                                        (local.tee 0
                                          (f64.sub
                                            (local.get 1)
                                            (f64.mul
                                              (local.tee 0
                                                (f64.mul
                                                  (local.get 1)
                                                  (local.get 1)))
                                              (f64.add
                                                (f64.mul
                                                  (local.get 0)
                                                  (f64.add
                                                    (f64.mul
                                                      (local.get 0)
                                                      (f64.add
                                                        (f64.mul
                                                          (local.get 0)
                                                          (f64.add
                                                            (f64.mul
                                                              (local.get 0)
                                                              (f64.const 0x1.6376972bea4dp-25 (;=0.000000041381367970572385;)))
                                                            (f64.const -0x1.bbd41c5d26bf1p-20 (;=-0.0000016533902205465252;))))
                                                        (f64.const 0x1.1566aaf25de2cp-14 (;=0.00006613756321437934;))))
                                                    (f64.const -0x1.6c16c16bebd93p-9 (;=-0.0027777777777015593;))))
                                                (f64.const 0x1.555555555553ep-3 (;=0.16666666666666602;)))))))
                                      (f64.add
                                        (local.get 0)
                                        (f64.const -0x1p+1 (;=-2;))))
                                    (f64.add
                                      (local.tee 0
                                        (f64.sub
                                          (local.get 2)
                                          (f64.sub
                                            (local.get 1)
                                            (local.get 17))))
                                      (f64.mul
                                        (local.get 1)
                                        (local.get 0)))))
                                (f64.const 0x1p+0 (;=1;))))))
                        (i64.const 32)))))
                (i32.const 1048576)))
            (local.set 1
              (f64.reinterpret_i64
                (i64.or
                  (i64.shl
                    (i64.extend_i32_u
                      (local.get 5))
                    (i64.const 32))
                  (i64.and
                    (local.get 3)
                    (i64.const 4294967295)))))
            (br 1 (;@3;)))
          (local.set 1
            (call $_ZN4libm4math6scalbn6scalbn17hd5ee51b98c77623bE
              (local.get 1)
              (local.get 8))))
        (return
          (f64.mul
            (local.get 15)
            (local.get 1))))
      (local.set 2
        (select
          (f64.div
            (f64.const 0x1p+0 (;=1;))
            (local.get 2))
          (local.get 2)
          (i64.lt_s
            (local.get 3)
            (i64.const 0))))
      (br_if 0 (;@1;)
        (i64.gt_s
          (local.get 7)
          (i64.const -1)))
      (block ;; label = @2
        (br_if 0 (;@2;)
          (i32.or
            (local.get 12)
            (i32.add
              (local.get 11)
              (i32.const -1072693248))))
        (return
          (f64.div
            (local.tee 1
              (f64.sub
                (local.get 2)
                (local.get 2)))
            (local.get 1))))
      (return
        (select
          (f64.neg
            (local.get 2))
          (local.get 2)
          (i32.eq
            (local.get 12)
            (i32.const 1)))))
    (local.get 2)
  )
  (func $libm_powf (;58;) (type 3) (param f32 f32) (result f32)
    (local f32 i32 i32 i32 f32 i32 i32 i32 i32 f32 f32 f32)
    (local.set 2
      (f32.const 0x1p+0 (;=1;)))
    (block ;; label = @1
      (block ;; label = @2
        (block ;; label = @3
          (block ;; label = @4
            (br_if 0 (;@4;)
              (i32.eq
                (local.tee 3
                  (i32.reinterpret_f32
                    (local.get 0)))
                (i32.const 1065353216)))
            (br_if 0 (;@4;)
              (i32.eqz
                (local.tee 5
                  (i32.and
                    (local.tee 4
                      (i32.reinterpret_f32
                        (local.get 1)))
                    (i32.const 2147483647)))))
            (block ;; label = @5
              (block ;; label = @6
                (block ;; label = @7
                  (br_if 0 (;@7;)
                    (i32.gt_u
                      (local.tee 7
                        (i32.reinterpret_f32
                          (local.tee 6
                            (f32.abs
                              (local.get 0)))))
                      (i32.const 2139095040)))
                  (br_if 0 (;@7;)
                    (i32.gt_u
                      (local.get 5)
                      (i32.const 2139095040)))
                  (br_if 1 (;@6;)
                    (i32.ge_s
                      (local.get 3)
                      (i32.const 0)))
                  (local.set 8
                    (i32.const 2))
                  (br_if 2 (;@5;)
                    (i32.gt_u
                      (local.get 5)
                      (i32.const 1266679807)))
                  (br_if 1 (;@6;)
                    (i32.lt_u
                      (local.get 5)
                      (i32.const 1065353216)))
                  (local.set 8
                    (i32.const 0))
                  (br_if 2 (;@5;)
                    (i32.ne
                      (i32.shl
                        (local.tee 10
                          (i32.shr_u
                            (local.get 5)
                            (local.tee 9
                              (i32.sub
                                (i32.const 150)
                                (i32.shr_u
                                  (local.get 5)
                                  (i32.const 23))))))
                        (local.get 9))
                      (local.get 5)))
                  (local.set 8
                    (i32.sub
                      (i32.const 2)
                      (i32.and
                        (local.get 10)
                        (i32.const 1))))
                  (br 2 (;@5;)))
                (return
                  (f32.add
                    (local.get 0)
                    (local.get 1))))
              (local.set 8
                (i32.const 0)))
            (block ;; label = @5
              (block ;; label = @6
                (br_if 0 (;@6;)
                  (i32.eq
                    (local.get 5)
                    (i32.const 1065353216)))
                (br_if 1 (;@5;)
                  (i32.ne
                    (local.get 5)
                    (i32.const 2139095040)))
                (block ;; label = @7
                  (block ;; label = @8
                    (br_table 4 (;@4;) 1 (;@7;) 0 (;@8;)
                      (i32.and
                        (i32.sub
                          (i32.gt_s
                            (local.get 7)
                            (i32.const 1065353216))
                          (i32.lt_s
                            (local.get 7)
                            (i32.const 1065353216)))
                        (i32.const 255))))
                  (return
                    (select
                      (f32.const 0x0p+0 (;=0;))
                      (f32.neg
                        (local.get 1))
                      (i32.gt_s
                        (local.get 4)
                        (i32.const -1)))))
                (return
                  (select
                    (local.get 1)
                    (f32.const 0x0p+0 (;=0;))
                    (i32.gt_s
                      (local.get 4)
                      (i32.const -1)))))
              (br_if 2 (;@3;)
                (i32.le_s
                  (local.get 4)
                  (i32.const -1)))
              (return
                (local.get 0)))
            (block ;; label = @5
              (block ;; label = @6
                (br_if 0 (;@6;)
                  (i32.eq
                    (local.get 4)
                    (i32.const 1056964608)))
                (br_if 1 (;@5;)
                  (i32.ne
                    (local.get 4)
                    (i32.const 1073741824)))
                (return
                  (f32.mul
                    (local.get 0)
                    (local.get 0))))
              (br_if 3 (;@2;)
                (i32.gt_s
                  (local.get 3)
                  (i32.const -1))))
            (block ;; label = @5
              (block ;; label = @6
                (block ;; label = @7
                  (block ;; label = @8
                    (block ;; label = @9
                      (block ;; label = @10
                        (br_if 0 (;@10;)
                          (i32.eq
                            (i32.and
                              (local.get 3)
                              (i32.const 1073741823))
                            (i32.const 1065353216)))
                        (br_if 1 (;@9;)
                          (local.get 7)))
                      (local.set 2
                        (select
                          (f32.div
                            (f32.const 0x1p+0 (;=1;))
                            (local.get 6))
                          (local.get 6)
                          (i32.lt_s
                            (local.get 4)
                            (i32.const 0))))
                      (br_if 5 (;@4;)
                        (i32.ge_s
                          (local.get 3)
                          (i32.const 0)))
                      (br_if 1 (;@8;)
                        (i32.or
                          (local.get 8)
                          (i32.add
                            (local.get 7)
                            (i32.const -1065353216))))
                      (return
                        (f32.div
                          (local.tee 0
                            (f32.sub
                              (local.get 2)
                              (local.get 2)))
                          (local.get 0))))
                    (local.set 11
                      (f32.const 0x1p+0 (;=1;)))
                    (br_if 3 (;@5;)
                      (i32.ge_s
                        (local.get 3)
                        (i32.const 0)))
                    (br_table 1 (;@7;) 2 (;@6;) 3 (;@5;)
                      (local.get 8)))
                  (return
                    (select
                      (f32.neg
                        (local.get 2))
                      (local.get 2)
                      (i32.eq
                        (local.get 8)
                        (i32.const 1)))))
                (return
                  (f32.div
                    (local.tee 0
                      (f32.sub
                        (local.get 0)
                        (local.get 0)))
                    (local.get 0))))
              (local.set 11
                (f32.const -0x1p+0 (;=-1;))))
            (block ;; label = @5
              (br_if 0 (;@5;)
                (i32.gt_u
                  (local.get 5)
                  (i32.const 1291845632)))
              (local.set 5
                (i32.or
                  (local.tee 7
                    (i32.and
                      (local.tee 8
                        (select
                          (i32.reinterpret_f32
                            (f32.mul
                              (local.get 6)
                              (f32.const 0x1p+24 (;=16777216;))))
                          (local.get 7)
                          (local.tee 3
                            (i32.lt_u
                              (local.get 7)
                              (i32.const 8388608)))))
                      (i32.const 8388607)))
                  (i32.const 1065353216)))
              (local.set 8
                (i32.add
                  (select
                    (i32.const -151)
                    (i32.const -127)
                    (local.get 3))
                  (i32.shr_s
                    (local.get 8)
                    (i32.const 23))))
              (local.set 3
                (i32.const 0))
              (block ;; label = @6
                (br_if 0 (;@6;)
                  (i32.lt_u
                    (local.get 7)
                    (i32.const 1885298)))
                (block ;; label = @7
                  (br_if 0 (;@7;)
                    (i32.ge_u
                      (local.get 7)
                      (i32.const 6140887)))
                  (local.set 3
                    (i32.const 1))
                  (br 1 (;@6;)))
                (local.set 5
                  (i32.or
                    (local.get 7)
                    (i32.const 1056964608)))
                (local.set 8
                  (i32.add
                    (local.get 8)
                    (i32.const 1))))
              (local.set 2
                (f32.sub
                  (local.tee 2
                    (f32.add
                      (f32.load offset=1048584
                        (local.tee 7
                          (i32.shl
                            (local.get 3)
                            (i32.const 2))))
                      (f32.add
                        (f32.mul
                          (f32.sub
                            (local.tee 6
                              (f32.add
                                (f32.mul
                                  (local.tee 0
                                    (f32.mul
                                      (local.tee 2
                                        (f32.div
                                          (f32.const 0x1p+0 (;=1;))
                                          (f32.add
                                            (local.tee 0
                                              (f32.load offset=1048576
                                                (local.get 7)))
                                            (local.tee 12
                                              (f32.reinterpret_i32
                                                (local.get 5))))))
                                      (f32.sub
                                        (f32.sub
                                          (local.tee 6
                                            (f32.sub
                                              (local.get 12)
                                              (local.get 0)))
                                          (f32.mul
                                            (local.tee 13
                                              (f32.reinterpret_i32
                                                (i32.add
                                                  (i32.add
                                                    (i32.and
                                                      (i32.shr_u
                                                        (local.get 5)
                                                        (i32.const 1))
                                                      (i32.const 536866816))
                                                    (i32.shl
                                                      (local.get 3)
                                                      (i32.const 21)))
                                                  (i32.const 541065216))))
                                            (local.tee 2
                                              (f32.reinterpret_i32
                                                (i32.and
                                                  (i32.reinterpret_f32
                                                    (local.tee 6
                                                      (f32.mul
                                                        (local.get 6)
                                                        (local.get 2))))
                                                  (i32.const -4096))))))
                                        (f32.mul
                                          (f32.add
                                            (f32.sub
                                              (local.get 0)
                                              (local.get 13))
                                            (local.get 12))
                                          (local.get 2)))))
                                  (local.tee 0
                                    (f32.reinterpret_i32
                                      (i32.and
                                        (i32.reinterpret_f32
                                          (f32.add
                                            (f32.add
                                              (local.tee 12
                                                (f32.mul
                                                  (local.get 2)
                                                  (local.get 2)))
                                              (f32.const 0x1.8p+1 (;=3;)))
                                            (local.tee 13
                                              (f32.add
                                                (f32.mul
                                                  (local.get 0)
                                                  (f32.add
                                                    (local.get 6)
                                                    (local.get 2)))
                                                (f32.mul
                                                  (f32.mul
                                                    (local.tee 0
                                                      (f32.mul
                                                        (local.get 6)
                                                        (local.get 6)))
                                                    (local.get 0))
                                                  (f32.add
                                                    (f32.mul
                                                      (local.get 0)
                                                      (f32.add
                                                        (f32.mul
                                                          (local.get 0)
                                                          (f32.add
                                                            (f32.mul
                                                              (local.get 0)
                                                              (f32.add
                                                                (f32.mul
                                                                  (local.get 0)
                                                                  (f32.add
                                                                    (f32.mul
                                                                      (local.get 0)
                                                                      (f32.const 0x1.a7e284p-3 (;=0.20697501;)))
                                                                    (f32.const 0x1.d864aap-3 (;=0.23066075;))))
                                                                (f32.const 0x1.17460ap-2 (;=0.27272812;))))
                                                            (f32.const 0x1.555556p-2 (;=0.33333334;))))
                                                        (f32.const 0x1.b6db6ep-2 (;=0.42857143;))))
                                                    (f32.const 0x1.333334p-1 (;=0.6;))))))))
                                        (i32.const -4096)))))
                                (f32.mul
                                  (local.get 6)
                                  (f32.sub
                                    (local.get 13)
                                    (f32.sub
                                      (f32.add
                                        (local.get 0)
                                        (f32.const -0x1.8p+1 (;=-3;)))
                                      (local.get 12))))))
                            (f32.sub
                              (local.tee 0
                                (f32.reinterpret_i32
                                  (i32.and
                                    (i32.reinterpret_f32
                                      (f32.add
                                        (local.get 6)
                                        (local.tee 2
                                          (f32.mul
                                            (local.get 2)
                                            (local.get 0)))))
                                    (i32.const -4096))))
                              (local.get 2)))
                          (f32.const 0x1.ec709ep-1 (;=0.9617967;)))
                        (f32.mul
                          (local.get 0)
                          (f32.const -0x1.ec478cp-14 (;=-0.000117368574;))))))
                  (f32.sub
                    (f32.sub
                      (f32.sub
                        (local.tee 0
                          (f32.reinterpret_i32
                            (i32.and
                              (i32.reinterpret_f32
                                (f32.add
                                  (f32.add
                                    (local.tee 6
                                      (f32.load offset=1048592
                                        (local.get 7)))
                                    (f32.add
                                      (local.get 2)
                                      (local.tee 12
                                        (f32.mul
                                          (local.get 0)
                                          (f32.const 0x1.ec8p-1 (;=0.96191406;))))))
                                  (local.tee 2
                                    (f32.convert_i32_s
                                      (local.get 8)))))
                              (i32.const -4096))))
                        (local.get 2))
                      (local.get 6))
                    (local.get 12))))
              (br 4 (;@1;)))
            (block ;; label = @5
              (br_if 0 (;@5;)
                (i32.lt_u
                  (local.get 7)
                  (i32.const 1065353208)))
              (block ;; label = @6
                (br_if 0 (;@6;)
                  (i32.gt_u
                    (local.get 7)
                    (i32.const 1065353223)))
                (local.set 2
                  (f32.sub
                    (local.tee 2
                      (f32.add
                        (f32.mul
                          (local.tee 0
                            (f32.add
                              (local.get 6)
                              (f32.const -0x1p+0 (;=-1;))))
                          (f32.const 0x1.d94aep-18 (;=0.0000070526075;)))
                        (f32.mul
                          (f32.mul
                            (f32.mul
                              (local.get 0)
                              (local.get 0))
                            (f32.sub
                              (f32.const 0x1p-1 (;=0.5;))
                              (f32.mul
                                (local.get 0)
                                (f32.add
                                  (f32.mul
                                    (local.get 0)
                                    (f32.const -0x1p-2 (;=-0.25;)))
                                  (f32.const 0x1.555556p-2 (;=0.33333334;))))))
                          (f32.const -0x1.715476p+0 (;=-1.442695;)))))
                    (f32.sub
                      (local.tee 0
                        (f32.reinterpret_i32
                          (i32.and
                            (i32.reinterpret_f32
                              (f32.add
                                (local.get 2)
                                (local.tee 6
                                  (f32.mul
                                    (local.get 0)
                                    (f32.const 0x1.7154p+0 (;=1.442688;))))))
                            (i32.const -4096))))
                      (local.get 6))))
                (br 5 (;@1;)))
              (block ;; label = @6
                (br_if 0 (;@6;)
                  (i32.gt_s
                    (local.get 4)
                    (i32.const 0)))
                (return
                  (f32.mul
                    (f32.mul
                      (local.get 11)
                      (f32.const 0x1.4484cp-100 (;=0.000000000000000000000000000001;)))
                    (f32.const 0x1.4484cp-100 (;=0.000000000000000000000000000001;)))))
              (return
                (f32.mul
                  (f32.mul
                    (local.get 11)
                    (f32.const 0x1.93e594p+99 (;=1000000000000000000000000000000;)))
                  (f32.const 0x1.93e594p+99 (;=1000000000000000000000000000000;)))))
            (block ;; label = @5
              (br_if 0 (;@5;)
                (i32.lt_s
                  (local.get 4)
                  (i32.const 0)))
              (return
                (f32.mul
                  (f32.mul
                    (local.get 11)
                    (f32.const 0x1.4484cp-100 (;=0.000000000000000000000000000001;)))
                  (f32.const 0x1.4484cp-100 (;=0.000000000000000000000000000001;)))))
            (local.set 2
              (f32.mul
                (f32.mul
                  (local.get 11)
                  (f32.const 0x1.93e594p+99 (;=1000000000000000000000000000000;)))
                (f32.const 0x1.93e594p+99 (;=1000000000000000000000000000000;)))))
          (return
            (local.get 2)))
        (return
          (f32.div
            (f32.const 0x1p+0 (;=1;))
            (local.get 0))))
      (return
        (call $_ZN4libm4math4sqrt5sqrtf17h8c1b66187740c44bE
          (local.get 0))))
    (block ;; label = @1
      (block ;; label = @2
        (block ;; label = @3
          (block ;; label = @4
            (br_if 0 (;@4;)
              (i32.gt_s
                (local.tee 5
                  (i32.reinterpret_f32
                    (local.tee 1
                      (f32.add
                        (local.tee 12
                          (f32.mul
                            (local.get 0)
                            (local.tee 6
                              (f32.reinterpret_i32
                                (i32.and
                                  (local.get 4)
                                  (i32.const -4096))))))
                        (local.tee 0
                          (f32.add
                            (f32.mul
                              (f32.sub
                                (local.get 1)
                                (local.get 6))
                              (local.get 0))
                            (f32.mul
                              (local.get 1)
                              (local.get 2))))))))
                (i32.const 1124073472)))
            (br_if 1 (;@3;)
              (i32.ne
                (local.get 5)
                (i32.const 1124073472)))
            (br_if 2 (;@2;)
              (i32.eqz
                (f32.gt
                  (f32.add
                    (local.get 0)
                    (f32.const 0x1.715478p-25 (;=0.000000042995666;)))
                  (f32.sub
                    (local.get 1)
                    (local.get 12)))))
            (return
              (f32.mul
                (f32.mul
                  (local.get 11)
                  (f32.const 0x1.93e594p+99 (;=1000000000000000000000000000000;)))
                (f32.const 0x1.93e594p+99 (;=1000000000000000000000000000000;)))))
          (return
            (f32.mul
              (f32.mul
                (local.get 11)
                (f32.const 0x1.93e594p+99 (;=1000000000000000000000000000000;)))
              (f32.const 0x1.93e594p+99 (;=1000000000000000000000000000000;)))))
        (block ;; label = @3
          (block ;; label = @4
            (br_if 0 (;@4;)
              (i32.gt_u
                (local.tee 4
                  (i32.and
                    (i32.reinterpret_f32
                      (local.get 1))
                    (i32.const 2147483647)))
                (i32.const 1125515264)))
            (br_if 1 (;@3;)
              (i32.ne
                (local.get 5)
                (i32.const -1021968384)))
            (br_if 1 (;@3;)
              (i32.eqz
                (f32.le
                  (local.get 0)
                  (f32.sub
                    (local.get 1)
                    (local.get 12)))))
            (return
              (f32.mul
                (f32.mul
                  (local.get 11)
                  (f32.const 0x1.4484cp-100 (;=0.000000000000000000000000000001;)))
                (f32.const 0x1.4484cp-100 (;=0.000000000000000000000000000001;)))))
          (return
            (f32.mul
              (f32.mul
                (local.get 11)
                (f32.const 0x1.4484cp-100 (;=0.000000000000000000000000000001;)))
              (f32.const 0x1.4484cp-100 (;=0.000000000000000000000000000001;)))))
        (local.set 3
          (i32.const 0))
        (br_if 1 (;@1;)
          (i32.le_u
            (local.get 4)
            (i32.const 1056964608))))
      (local.set 3
        (select
          (i32.sub
            (i32.const 0)
            (local.tee 3
              (i32.shr_u
                (i32.or
                  (i32.and
                    (local.tee 4
                      (i32.add
                        (i32.shr_u
                          (i32.const 8388608)
                          (i32.add
                            (i32.shr_u
                              (local.get 5)
                              (i32.const 23))
                            (i32.const 2)))
                        (local.get 5)))
                    (i32.const 8388607))
                  (i32.const 8388608))
                (i32.sub
                  (i32.const 22)
                  (local.tee 7
                    (i32.shr_u
                      (local.get 4)
                      (i32.const 23)))))))
          (local.get 3)
          (i32.lt_s
            (local.get 5)
            (i32.const 0))))
      (local.set 5
        (i32.reinterpret_f32
          (f32.add
            (local.get 0)
            (local.tee 12
              (f32.sub
                (local.get 12)
                (f32.reinterpret_i32
                  (i32.and
                    (i32.shr_s
                      (i32.const -8388608)
                      (i32.add
                        (local.get 7)
                        (i32.const 1)))
                    (local.get 4)))))))))
    (block ;; label = @1
      (block ;; label = @2
        (br_if 0 (;@2;)
          (i32.lt_s
            (local.tee 5
              (i32.add
                (i32.shl
                  (local.get 3)
                  (i32.const 23))
                (i32.reinterpret_f32
                  (local.tee 0
                    (f32.add
                      (f32.sub
                        (local.tee 0
                          (f32.add
                            (local.tee 2
                              (f32.mul
                                (local.tee 1
                                  (f32.reinterpret_i32
                                    (i32.and
                                      (local.get 5)
                                      (i32.const -32768))))
                                (f32.const 0x1.62e4p-1 (;=0.69314575;))))
                            (local.tee 6
                              (f32.add
                                (f32.mul
                                  (local.get 1)
                                  (f32.const 0x1.7f7d18p-20 (;=0.0000014286065;)))
                                (f32.mul
                                  (f32.sub
                                    (local.get 0)
                                    (f32.sub
                                      (local.get 1)
                                      (local.get 12)))
                                  (f32.const 0x1.62e43p-1 (;=0.6931472;)))))))
                        (f32.sub
                          (f32.div
                            (f32.mul
                              (local.get 0)
                              (local.tee 1
                                (f32.sub
                                  (local.get 0)
                                  (f32.mul
                                    (local.tee 1
                                      (f32.mul
                                        (local.get 0)
                                        (local.get 0)))
                                    (f32.add
                                      (f32.mul
                                        (local.get 1)
                                        (f32.add
                                          (f32.mul
                                            (local.get 1)
                                            (f32.add
                                              (f32.mul
                                                (local.get 1)
                                                (f32.add
                                                  (f32.mul
                                                    (local.get 1)
                                                    (f32.const 0x1.637698p-25 (;=0.00000004138137;)))
                                                  (f32.const -0x1.bbd41cp-20 (;=-0.0000016533902;))))
                                              (f32.const 0x1.1566aap-14 (;=0.00006613756;))))
                                          (f32.const -0x1.6c16c2p-9 (;=-0.0027777778;))))
                                      (f32.const 0x1.555556p-3 (;=0.16666667;)))))))
                            (f32.add
                              (local.get 1)
                              (f32.const -0x1p+1 (;=-2;))))
                          (f32.add
                            (local.tee 1
                              (f32.sub
                                (local.get 6)
                                (f32.sub
                                  (local.get 0)
                                  (local.get 2))))
                            (f32.mul
                              (local.get 0)
                              (local.get 1)))))
                      (f32.const 0x1p+0 (;=1;)))))))
            (i32.const 8388608)))
        (local.set 0
          (f32.reinterpret_i32
            (local.get 5)))
        (br 1 (;@1;)))
      (local.set 0
        (call $_ZN4libm4math6scalbn7scalbnf17ha430054e259e38a4E
          (local.get 0)
          (local.get 3))))
    (f32.mul
      (local.get 11)
      (local.get 0))
  )
  (func $_ZN4libm4math6scalbn7scalbnf17ha430054e259e38a4E (;59;) (type 10) (param f32 i32) (result f32)
    (block ;; label = @1
      (block ;; label = @2
        (block ;; label = @3
          (block ;; label = @4
            (br_if 0 (;@4;)
              (i32.gt_s
                (local.get 1)
                (i32.const 127)))
            (br_if 3 (;@1;)
              (i32.ge_s
                (local.get 1)
                (i32.const -126)))
            (local.set 0
              (f32.mul
                (local.get 0)
                (f32.const 0x1p-102 (;=0.00000000000000000000000000000019721523;))))
            (br_if 1 (;@3;)
              (i32.le_u
                (local.get 1)
                (i32.const -229)))
            (local.set 1
              (i32.add
                (local.get 1)
                (i32.const 102)))
            (br 3 (;@1;)))
          (local.set 0
            (f32.mul
              (local.get 0)
              (f32.const 0x1p+127 (;=170141180000000000000000000000000000000;))))
          (br_if 1 (;@2;)
            (i32.gt_u
              (local.get 1)
              (i32.const 254)))
          (local.set 1
            (i32.add
              (local.get 1)
              (i32.const -127)))
          (br 2 (;@1;)))
        (local.set 0
          (f32.mul
            (local.get 0)
            (f32.const 0x1p-102 (;=0.00000000000000000000000000000019721523;))))
        (local.set 1
          (i32.add
            (select
              (local.get 1)
              (i32.const -330)
              (i32.gt_u
                (local.get 1)
                (i32.const -330)))
            (i32.const 204)))
        (br 1 (;@1;)))
      (local.set 0
        (f32.mul
          (local.get 0)
          (f32.const 0x1p+127 (;=170141180000000000000000000000000000000;))))
      (local.set 1
        (i32.add
          (select
            (local.get 1)
            (i32.const 381)
            (i32.lt_u
              (local.get 1)
              (i32.const 381)))
          (i32.const -254))))
    (f32.mul
      (local.get 0)
      (f32.reinterpret_i32
        (i32.and
          (i32.add
            (i32.shl
              (local.get 1)
              (i32.const 23))
            (i32.const 1065353216))
          (i32.const 2139095040))))
  )
  (func $libm_sin (;60;) (type 0) (param f64) (result f64)
    (local i32 i32 f64 f64 f64)
    (global.set $__stack_pointer
      (local.tee 1
        (i32.sub
          (global.get $__stack_pointer)
          (i32.const 32))))
    (block ;; label = @1
      (block ;; label = @2
        (br_if 0 (;@2;)
          (i32.lt_u
            (local.tee 2
              (i32.and
                (i32.wrap_i64
                  (i64.shr_u
                    (i64.reinterpret_f64
                      (local.get 0))
                    (i64.const 32)))
                (i32.const 2147483647)))
            (i32.const 1072243196)))
        (block ;; label = @3
          (block ;; label = @4
            (block ;; label = @5
              (block ;; label = @6
                (block ;; label = @7
                  (br_if 0 (;@7;)
                    (i32.gt_u
                      (local.get 2)
                      (i32.const 2146435071)))
                  (call $_ZN4libm4math8rem_pio28rem_pio217hcfd3034c1d4391f0E
                    (i32.add
                      (local.get 1)
                      (i32.const 8))
                    (local.get 0))
                  (local.set 3
                    (f64.load offset=24
                      (local.get 1)))
                  (local.set 0
                    (f64.load offset=8
                      (local.get 1)))
                  (br_table 2 (;@5;) 3 (;@4;) 4 (;@3;) 1 (;@6;) 2 (;@5;)
                    (i32.and
                      (i32.load offset=16
                        (local.get 1))
                      (i32.const 3))))
                (local.set 0
                  (f64.sub
                    (local.get 0)
                    (local.get 0)))
                (br 5 (;@1;)))
              (local.set 0
                (f64.neg
                  (call $_ZN4libm4math5k_cos5k_cos17h5eb36bbcae756a29E
                    (local.get 0)
                    (local.get 3))))
              (br 4 (;@1;)))
            (local.set 0
              (f64.sub
                (local.get 0)
                (f64.add
                  (f64.mul
                    (local.tee 5
                      (f64.mul
                        (local.get 0)
                        (local.tee 4
                          (f64.mul
                            (local.get 0)
                            (local.get 0)))))
                    (f64.const 0x1.5555555555549p-3 (;=0.16666666666666632;)))
                  (f64.sub
                    (f64.mul
                      (local.get 4)
                      (f64.sub
                        (f64.mul
                          (local.get 3)
                          (f64.const 0x1p-1 (;=0.5;)))
                        (f64.mul
                          (local.get 5)
                          (f64.add
                            (f64.mul
                              (f64.mul
                                (local.get 4)
                                (f64.mul
                                  (local.get 4)
                                  (local.get 4)))
                              (f64.add
                                (f64.mul
                                  (local.get 4)
                                  (f64.const 0x1.5d93a5acfd57cp-33 (;=0.000000000158969099521155;)))
                                (f64.const -0x1.ae5e68a2b9cebp-26 (;=-0.000000025050760253406863;))))
                            (f64.add
                              (f64.mul
                                (local.get 4)
                                (f64.add
                                  (f64.mul
                                    (local.get 4)
                                    (f64.const 0x1.71de357b1fe7dp-19 (;=0.0000027557313707070068;)))
                                  (f64.const -0x1.a01a019c161d5p-13 (;=-0.0001984126982985795;))))
                              (f64.const 0x1.111111110f8a6p-7 (;=0.00833333333332249;)))))))
                    (local.get 3)))))
            (br 3 (;@1;)))
          (local.set 0
            (call $_ZN4libm4math5k_cos5k_cos17h5eb36bbcae756a29E
              (local.get 0)
              (local.get 3)))
          (br 2 (;@1;)))
        (local.set 0
          (f64.neg
            (f64.sub
              (local.get 0)
              (f64.add
                (f64.mul
                  (local.tee 5
                    (f64.mul
                      (local.get 0)
                      (local.tee 4
                        (f64.mul
                          (local.get 0)
                          (local.get 0)))))
                  (f64.const 0x1.5555555555549p-3 (;=0.16666666666666632;)))
                (f64.sub
                  (f64.mul
                    (local.get 4)
                    (f64.sub
                      (f64.mul
                        (local.get 3)
                        (f64.const 0x1p-1 (;=0.5;)))
                      (f64.mul
                        (local.get 5)
                        (f64.add
                          (f64.mul
                            (f64.mul
                              (local.get 4)
                              (f64.mul
                                (local.get 4)
                                (local.get 4)))
                            (f64.add
                              (f64.mul
                                (local.get 4)
                                (f64.const 0x1.5d93a5acfd57cp-33 (;=0.000000000158969099521155;)))
                              (f64.const -0x1.ae5e68a2b9cebp-26 (;=-0.000000025050760253406863;))))
                          (f64.add
                            (f64.mul
                              (local.get 4)
                              (f64.add
                                (f64.mul
                                  (local.get 4)
                                  (f64.const 0x1.71de357b1fe7dp-19 (;=0.0000027557313707070068;)))
                                (f64.const -0x1.a01a019c161d5p-13 (;=-0.0001984126982985795;))))
                            (f64.const 0x1.111111110f8a6p-7 (;=0.00833333333332249;)))))))
                  (local.get 3))))))
        (br 1 (;@1;)))
      (block ;; label = @2
        (br_if 0 (;@2;)
          (i32.lt_u
            (local.get 2)
            (i32.const 1045430272)))
        (local.set 0
          (f64.add
            (local.get 0)
            (f64.mul
              (f64.mul
                (local.get 0)
                (local.tee 3
                  (f64.mul
                    (local.get 0)
                    (local.get 0))))
              (f64.add
                (f64.mul
                  (local.get 3)
                  (f64.add
                    (f64.mul
                      (f64.mul
                        (local.get 3)
                        (f64.mul
                          (local.get 3)
                          (local.get 3)))
                      (f64.add
                        (f64.mul
                          (local.get 3)
                          (f64.const 0x1.5d93a5acfd57cp-33 (;=0.000000000158969099521155;)))
                        (f64.const -0x1.ae5e68a2b9cebp-26 (;=-0.000000025050760253406863;))))
                    (f64.add
                      (f64.mul
                        (local.get 3)
                        (f64.add
                          (f64.mul
                            (local.get 3)
                            (f64.const 0x1.71de357b1fe7dp-19 (;=0.0000027557313707070068;)))
                          (f64.const -0x1.a01a019c161d5p-13 (;=-0.0001984126982985795;))))
                      (f64.const 0x1.111111110f8a6p-7 (;=0.00833333333332249;)))))
                (f64.const -0x1.5555555555549p-3 (;=-0.16666666666666632;))))))
        (br 1 (;@1;)))
      (block ;; label = @2
        (br_if 0 (;@2;)
          (i32.lt_u
            (local.get 2)
            (i32.const 1048576)))
        (f64.store offset=8
          (local.get 1)
          (f64.add
            (local.get 0)
            (f64.const 0x1p+120 (;=1329227995784916000000000000000000000;))))
        (drop
          (f64.load offset=8
            (local.get 1)))
        (br 1 (;@1;)))
      (f64.store offset=8
        (local.get 1)
        (f64.mul
          (local.get 0)
          (f64.const 0x1p-120 (;=0.000000000000000000000000000000000000752316384526264;))))
      (drop
        (f64.load offset=8
          (local.get 1))))
    (global.set $__stack_pointer
      (i32.add
        (local.get 1)
        (i32.const 32)))
    (local.get 0)
  )
  (func $libm_sinf (;61;) (type 1) (param f32) (result f32)
    (local i32 f64 i32 i32 f64 f64)
    (global.set $__stack_pointer
      (local.tee 1
        (i32.sub
          (global.get $__stack_pointer)
          (i32.const 16))))
    (local.set 2
      (f64.promote_f32
        (local.get 0)))
    (block ;; label = @1
      (block ;; label = @2
        (br_if 0 (;@2;)
          (i32.lt_u
            (local.tee 4
              (i32.and
                (local.tee 3
                  (i32.reinterpret_f32
                    (local.get 0)))
                (i32.const 2147483647)))
            (i32.const 1061752795)))
        (block ;; label = @3
          (br_if 0 (;@3;)
            (i32.lt_u
              (local.get 4)
              (i32.const 1081824210)))
          (block ;; label = @4
            (br_if 0 (;@4;)
              (i32.lt_u
                (local.get 4)
                (i32.const 1088565718)))
            (block ;; label = @5
              (block ;; label = @6
                (block ;; label = @7
                  (block ;; label = @8
                    (block ;; label = @9
                      (br_if 0 (;@9;)
                        (i32.gt_u
                          (local.get 4)
                          (i32.const 2139095039)))
                      (call $_ZN4libm4math9rem_pio2f9rem_pio2f17h3b6adc3e5afb8880E
                        (local.get 1)
                        (local.get 0))
                      (local.set 2
                        (f64.load offset=8
                          (local.get 1)))
                      (br_table 2 (;@7;) 3 (;@6;) 4 (;@5;) 1 (;@8;) 2 (;@7;)
                        (i32.and
                          (i32.load
                            (local.get 1))
                          (i32.const 3))))
                    (local.set 0
                      (f32.sub
                        (local.get 0)
                        (local.get 0)))
                    (br 7 (;@1;)))
                  (local.set 0
                    (f32.neg
                      (f32.demote_f64
                        (f64.add
                          (f64.add
                            (f64.add
                              (f64.mul
                                (local.tee 2
                                  (f64.mul
                                    (local.get 2)
                                    (local.get 2)))
                                (f64.const -0x1.ffffffd0c5e81p-2 (;=-0.499999997251031;)))
                              (f64.const 0x1p+0 (;=1;)))
                            (f64.mul
                              (local.tee 5
                                (f64.mul
                                  (local.get 2)
                                  (local.get 2)))
                              (f64.const 0x1.55553e1053a42p-5 (;=0.04166662332373906;))))
                          (f64.mul
                            (f64.mul
                              (local.get 2)
                              (local.get 5))
                            (f64.add
                              (f64.mul
                                (local.get 2)
                                (f64.const 0x1.99342e0ee5069p-16 (;=0.00002439044879627741;)))
                              (f64.const -0x1.6c087e80f1e27p-10 (;=-0.001388676377460993;))))))))
                  (br 6 (;@1;)))
                (local.set 0
                  (f32.demote_f64
                    (f64.add
                      (f64.mul
                        (f64.mul
                          (local.tee 6
                            (f64.mul
                              (local.get 2)
                              (local.tee 5
                                (f64.mul
                                  (local.get 2)
                                  (local.get 2)))))
                          (f64.mul
                            (local.get 5)
                            (local.get 5)))
                        (f64.add
                          (f64.mul
                            (local.get 5)
                            (f64.const 0x1.6cd878c3b46a7p-19 (;=0.000002718311493989822;)))
                          (f64.const -0x1.a00f9e2cae774p-13 (;=-0.00019839334836096632;))))
                      (f64.add
                        (local.get 2)
                        (f64.mul
                          (local.get 6)
                          (f64.add
                            (f64.mul
                              (local.get 5)
                              (f64.const 0x1.11110896efbb2p-7 (;=0.008333329385889463;)))
                            (f64.const -0x1.5555554cbac77p-3 (;=-0.16666666641626524;))))))))
                (br 5 (;@1;)))
              (local.set 0
                (f32.demote_f64
                  (f64.add
                    (f64.add
                      (f64.add
                        (f64.mul
                          (local.tee 2
                            (f64.mul
                              (local.get 2)
                              (local.get 2)))
                          (f64.const -0x1.ffffffd0c5e81p-2 (;=-0.499999997251031;)))
                        (f64.const 0x1p+0 (;=1;)))
                      (f64.mul
                        (local.tee 5
                          (f64.mul
                            (local.get 2)
                            (local.get 2)))
                        (f64.const 0x1.55553e1053a42p-5 (;=0.04166662332373906;))))
                    (f64.mul
                      (f64.mul
                        (local.get 2)
                        (local.get 5))
                      (f64.add
                        (f64.mul
                          (local.get 2)
                          (f64.const 0x1.99342e0ee5069p-16 (;=0.00002439044879627741;)))
                        (f64.const -0x1.6c087e80f1e27p-10 (;=-0.001388676377460993;)))))))
              (br 4 (;@1;)))
            (local.set 0
              (f32.demote_f64
                (f64.add
                  (f64.mul
                    (f64.mul
                      (local.tee 6
                        (f64.mul
                          (local.tee 5
                            (f64.mul
                              (local.get 2)
                              (local.get 2)))
                          (f64.neg
                            (local.get 2))))
                      (f64.mul
                        (local.get 5)
                        (local.get 5)))
                    (f64.add
                      (f64.mul
                        (local.get 5)
                        (f64.const 0x1.6cd878c3b46a7p-19 (;=0.000002718311493989822;)))
                      (f64.const -0x1.a00f9e2cae774p-13 (;=-0.00019839334836096632;))))
                  (f64.sub
                    (f64.mul
                      (local.get 6)
                      (f64.add
                        (f64.mul
                          (local.get 5)
                          (f64.const 0x1.11110896efbb2p-7 (;=0.008333329385889463;)))
                        (f64.const -0x1.5555554cbac77p-3 (;=-0.16666666641626524;))))
                    (local.get 2)))))
            (br 3 (;@1;)))
          (block ;; label = @4
            (br_if 0 (;@4;)
              (i32.lt_u
                (local.get 4)
                (i32.const 1085271520)))
            (local.set 0
              (f32.demote_f64
                (f64.add
                  (f64.mul
                    (f64.mul
                      (local.tee 6
                        (f64.mul
                          (local.tee 5
                            (f64.add
                              (select
                                (f64.const -0x1.921fb54442d18p+2 (;=-6.283185307179586;))
                                (f64.const 0x1.921fb54442d18p+2 (;=6.283185307179586;))
                                (i32.gt_s
                                  (local.get 3)
                                  (i32.const -1)))
                              (local.get 2)))
                          (local.tee 2
                            (f64.mul
                              (local.get 5)
                              (local.get 5)))))
                      (f64.mul
                        (local.get 2)
                        (local.get 2)))
                    (f64.add
                      (f64.mul
                        (local.get 2)
                        (f64.const 0x1.6cd878c3b46a7p-19 (;=0.000002718311493989822;)))
                      (f64.const -0x1.a00f9e2cae774p-13 (;=-0.00019839334836096632;))))
                  (f64.add
                    (local.get 5)
                    (f64.mul
                      (local.get 6)
                      (f64.add
                        (f64.mul
                          (local.get 2)
                          (f64.const 0x1.11110896efbb2p-7 (;=0.008333329385889463;)))
                        (f64.const -0x1.5555554cbac77p-3 (;=-0.16666666641626524;))))))))
            (br 3 (;@1;)))
          (block ;; label = @4
            (br_if 0 (;@4;)
              (i32.lt_s
                (local.get 3)
                (i32.const 0)))
            (local.set 0
              (f32.neg
                (f32.demote_f64
                  (f64.add
                    (f64.add
                      (f64.add
                        (f64.mul
                          (local.tee 2
                            (f64.mul
                              (local.tee 2
                                (f64.add
                                  (local.get 2)
                                  (f64.const -0x1.2d97c7f3321d2p+2 (;=-4.71238898038469;))))
                              (local.get 2)))
                          (f64.const -0x1.ffffffd0c5e81p-2 (;=-0.499999997251031;)))
                        (f64.const 0x1p+0 (;=1;)))
                      (f64.mul
                        (local.tee 5
                          (f64.mul
                            (local.get 2)
                            (local.get 2)))
                        (f64.const 0x1.55553e1053a42p-5 (;=0.04166662332373906;))))
                    (f64.mul
                      (f64.mul
                        (local.get 2)
                        (local.get 5))
                      (f64.add
                        (f64.mul
                          (local.get 2)
                          (f64.const 0x1.99342e0ee5069p-16 (;=0.00002439044879627741;)))
                        (f64.const -0x1.6c087e80f1e27p-10 (;=-0.001388676377460993;))))))))
            (br 3 (;@1;)))
          (local.set 0
            (f32.demote_f64
              (f64.add
                (f64.add
                  (f64.add
                    (f64.mul
                      (local.tee 2
                        (f64.mul
                          (local.tee 2
                            (f64.add
                              (local.get 2)
                              (f64.const 0x1.2d97c7f3321d2p+2 (;=4.71238898038469;))))
                          (local.get 2)))
                      (f64.const -0x1.ffffffd0c5e81p-2 (;=-0.499999997251031;)))
                    (f64.const 0x1p+0 (;=1;)))
                  (f64.mul
                    (local.tee 5
                      (f64.mul
                        (local.get 2)
                        (local.get 2)))
                    (f64.const 0x1.55553e1053a42p-5 (;=0.04166662332373906;))))
                (f64.mul
                  (f64.mul
                    (local.get 2)
                    (local.get 5))
                  (f64.add
                    (f64.mul
                      (local.get 2)
                      (f64.const 0x1.99342e0ee5069p-16 (;=0.00002439044879627741;)))
                    (f64.const -0x1.6c087e80f1e27p-10 (;=-0.001388676377460993;)))))))
          (br 2 (;@1;)))
        (block ;; label = @3
          (br_if 0 (;@3;)
            (i32.lt_u
              (local.get 4)
              (i32.const 1075235812)))
          (local.set 0
            (f32.demote_f64
              (f64.add
                (f64.mul
                  (f64.mul
                    (local.tee 6
                      (f64.mul
                        (local.tee 2
                          (f64.mul
                            (local.tee 5
                              (f64.add
                                (select
                                  (f64.const -0x1.921fb54442d18p+1 (;=-3.141592653589793;))
                                  (f64.const 0x1.921fb54442d18p+1 (;=3.141592653589793;))
                                  (i32.gt_s
                                    (local.get 3)
                                    (i32.const -1)))
                                (local.get 2)))
                            (local.get 5)))
                        (f64.neg
                          (local.get 5))))
                    (f64.mul
                      (local.get 2)
                      (local.get 2)))
                  (f64.add
                    (f64.mul
                      (local.get 2)
                      (f64.const 0x1.6cd878c3b46a7p-19 (;=0.000002718311493989822;)))
                    (f64.const -0x1.a00f9e2cae774p-13 (;=-0.00019839334836096632;))))
                (f64.sub
                  (f64.mul
                    (local.get 6)
                    (f64.add
                      (f64.mul
                        (local.get 2)
                        (f64.const 0x1.11110896efbb2p-7 (;=0.008333329385889463;)))
                      (f64.const -0x1.5555554cbac77p-3 (;=-0.16666666641626524;))))
                  (local.get 5)))))
          (br 2 (;@1;)))
        (block ;; label = @3
          (br_if 0 (;@3;)
            (i32.lt_s
              (local.get 3)
              (i32.const 0)))
          (local.set 0
            (f32.demote_f64
              (f64.add
                (f64.add
                  (f64.add
                    (f64.mul
                      (local.tee 2
                        (f64.mul
                          (local.tee 2
                            (f64.add
                              (local.get 2)
                              (f64.const -0x1.921fb54442d18p+0 (;=-1.5707963267948966;))))
                          (local.get 2)))
                      (f64.const -0x1.ffffffd0c5e81p-2 (;=-0.499999997251031;)))
                    (f64.const 0x1p+0 (;=1;)))
                  (f64.mul
                    (local.tee 5
                      (f64.mul
                        (local.get 2)
                        (local.get 2)))
                    (f64.const 0x1.55553e1053a42p-5 (;=0.04166662332373906;))))
                (f64.mul
                  (f64.mul
                    (local.get 2)
                    (local.get 5))
                  (f64.add
                    (f64.mul
                      (local.get 2)
                      (f64.const 0x1.99342e0ee5069p-16 (;=0.00002439044879627741;)))
                    (f64.const -0x1.6c087e80f1e27p-10 (;=-0.001388676377460993;)))))))
          (br 2 (;@1;)))
        (local.set 0
          (f32.neg
            (f32.demote_f64
              (f64.add
                (f64.add
                  (f64.add
                    (f64.mul
                      (local.tee 2
                        (f64.mul
                          (local.tee 2
                            (f64.add
                              (local.get 2)
                              (f64.const 0x1.921fb54442d18p+0 (;=1.5707963267948966;))))
                          (local.get 2)))
                      (f64.const -0x1.ffffffd0c5e81p-2 (;=-0.499999997251031;)))
                    (f64.const 0x1p+0 (;=1;)))
                  (f64.mul
                    (local.tee 5
                      (f64.mul
                        (local.get 2)
                        (local.get 2)))
                    (f64.const 0x1.55553e1053a42p-5 (;=0.04166662332373906;))))
                (f64.mul
                  (f64.mul
                    (local.get 2)
                    (local.get 5))
                  (f64.add
                    (f64.mul
                      (local.get 2)
                      (f64.const 0x1.99342e0ee5069p-16 (;=0.00002439044879627741;)))
                    (f64.const -0x1.6c087e80f1e27p-10 (;=-0.001388676377460993;))))))))
        (br 1 (;@1;)))
      (block ;; label = @2
        (br_if 0 (;@2;)
          (i32.lt_u
            (local.get 4)
            (i32.const 964689920)))
        (local.set 0
          (f32.demote_f64
            (f64.add
              (f64.mul
                (f64.mul
                  (local.tee 6
                    (f64.mul
                      (local.tee 5
                        (f64.mul
                          (local.get 2)
                          (local.get 2)))
                      (local.get 2)))
                  (f64.mul
                    (local.get 5)
                    (local.get 5)))
                (f64.add
                  (f64.mul
                    (local.get 5)
                    (f64.const 0x1.6cd878c3b46a7p-19 (;=0.000002718311493989822;)))
                  (f64.const -0x1.a00f9e2cae774p-13 (;=-0.00019839334836096632;))))
              (f64.add
                (f64.mul
                  (local.get 6)
                  (f64.add
                    (f64.mul
                      (local.get 5)
                      (f64.const 0x1.11110896efbb2p-7 (;=0.008333329385889463;)))
                    (f64.const -0x1.5555554cbac77p-3 (;=-0.16666666641626524;))))
                (local.get 2)))))
        (br 1 (;@1;)))
      (f32.store
        (local.get 1)
        (select
          (f32.mul
            (local.get 0)
            (f32.const 0x1p-120 (;=0.0000000000000000000000000000000000007523164;)))
          (f32.add
            (local.get 0)
            (f32.const 0x1p+120 (;=1329228000000000000000000000000000000;)))
          (i32.lt_u
            (local.get 4)
            (i32.const 8388608))))
      (drop
        (f32.load
          (local.get 1))))
    (global.set $__stack_pointer
      (i32.add
        (local.get 1)
        (i32.const 16)))
    (local.get 0)
  )
  (func $libm_sinh (;62;) (type 0) (param f64) (result f64)
    (local f64 f64 i64)
    (local.set 1
      (f64.copysign
        (f64.const 0x1p-1 (;=0.5;))
        (local.get 0)))
    (block ;; label = @1
      (br_if 0 (;@1;)
        (i64.lt_u
          (local.tee 3
            (i64.reinterpret_f64
              (local.tee 2
                (f64.abs
                  (local.get 0)))))
          (i64.const 4649454526309335040)))
      (return
        (f64.mul
          (f64.add
            (local.get 1)
            (local.get 1))
          (f64.mul
            (f64.mul
              (call $_ZN4libm4math3exp3exp17h8eb8b2450c3bf8abE
                (f64.add
                  (local.get 2)
                  (f64.const -0x1.62066151add8bp+10 (;=-1416.0996898839683;))))
              (f64.const 0x1p+1021 (;=22471164185778950000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000;)))
            (f64.const 0x1p+1021 (;=22471164185778950000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000;))))))
    (local.set 2
      (call $_ZN4libm4math5expm15expm117hf425b3a732f15702E
        (local.get 2)))
    (block ;; label = @1
      (br_if 0 (;@1;)
        (i64.lt_u
          (local.get 3)
          (i64.const 4607182418800017408)))
      (return
        (f64.mul
          (local.get 1)
          (f64.add
            (local.get 2)
            (f64.div
              (local.get 2)
              (f64.add
                (local.get 2)
                (f64.const 0x1p+0 (;=1;))))))))
    (block ;; label = @1
      (br_if 0 (;@1;)
        (i64.lt_u
          (local.get 3)
          (i64.const 4490088828488384512)))
      (local.set 0
        (f64.mul
          (local.get 1)
          (f64.sub
            (f64.add
              (local.get 2)
              (local.get 2))
            (f64.div
              (f64.mul
                (local.get 2)
                (local.get 2))
              (f64.add
                (local.get 2)
                (f64.const 0x1p+0 (;=1;))))))))
    (local.get 0)
  )
  (func $libm_sinhf (;63;) (type 1) (param f32) (result f32)
    (local f32 f32 i32)
    (local.set 1
      (f32.copysign
        (f32.const 0x1p-1 (;=0.5;))
        (local.get 0)))
    (block ;; label = @1
      (br_if 0 (;@1;)
        (i32.lt_u
          (local.tee 3
            (i32.reinterpret_f32
              (local.tee 2
                (f32.abs
                  (local.get 0)))))
          (i32.const 1118925335)))
      (return
        (f32.mul
          (f32.add
            (local.get 1)
            (local.get 1))
          (f32.mul
            (f32.mul
              (call $_ZN4libm4math4expf4expf17hedd2067d7b29c452E
                (f32.add
                  (local.get 2)
                  (f32.const -0x1.45c778p+7 (;=-162.88959;))))
              (f32.const 0x1p+117 (;=166153500000000000000000000000000000;)))
            (f32.const 0x1p+117 (;=166153500000000000000000000000000000;))))))
    (local.set 2
      (call $_ZN4libm4math6expm1f6expm1f17h8c93774ba8d4df49E
        (local.get 2)))
    (block ;; label = @1
      (br_if 0 (;@1;)
        (i32.lt_u
          (local.get 3)
          (i32.const 1065353216)))
      (return
        (f32.mul
          (local.get 1)
          (f32.add
            (local.get 2)
            (f32.div
              (local.get 2)
              (f32.add
                (local.get 2)
                (f32.const 0x1p+0 (;=1;))))))))
    (block ;; label = @1
      (br_if 0 (;@1;)
        (i32.lt_u
          (local.get 3)
          (i32.const 964689920)))
      (local.set 0
        (f32.mul
          (local.get 1)
          (f32.sub
            (f32.add
              (local.get 2)
              (local.get 2))
            (f32.div
              (f32.mul
                (local.get 2)
                (local.get 2))
              (f32.add
                (local.get 2)
                (f32.const 0x1p+0 (;=1;))))))))
    (local.get 0)
  )
  (func $libm_tan (;64;) (type 0) (param f64) (result f64)
    (local i32 i32)
    (global.set $__stack_pointer
      (local.tee 1
        (i32.sub
          (global.get $__stack_pointer)
          (i32.const 32))))
    (block ;; label = @1
      (block ;; label = @2
        (br_if 0 (;@2;)
          (i32.lt_u
            (local.tee 2
              (i32.and
                (i32.wrap_i64
                  (i64.shr_u
                    (i64.reinterpret_f64
                      (local.get 0))
                    (i64.const 32)))
                (i32.const 2147483647)))
            (i32.const 1072243196)))
        (block ;; label = @3
          (br_if 0 (;@3;)
            (i32.gt_u
              (local.get 2)
              (i32.const 2146435071)))
          (call $_ZN4libm4math8rem_pio28rem_pio217hcfd3034c1d4391f0E
            (i32.add
              (local.get 1)
              (i32.const 8))
            (local.get 0))
          (local.set 0
            (call $_ZN4libm4math5k_tan5k_tan17he70947e0d23fd5ccE
              (f64.load offset=8
                (local.get 1))
              (f64.load offset=24
                (local.get 1))
              (i32.and
                (i32.load offset=16
                  (local.get 1))
                (i32.const 1))))
          (br 2 (;@1;)))
        (local.set 0
          (f64.sub
            (local.get 0)
            (local.get 0)))
        (br 1 (;@1;)))
      (block ;; label = @2
        (br_if 0 (;@2;)
          (i32.lt_u
            (local.get 2)
            (i32.const 1044381696)))
        (local.set 0
          (call $_ZN4libm4math5k_tan5k_tan17he70947e0d23fd5ccE
            (local.get 0)
            (f64.const 0x0p+0 (;=0;))
            (i32.const 0)))
        (br 1 (;@1;)))
      (f64.store offset=8
        (local.get 1)
        (select
          (f64.mul
            (local.get 0)
            (f64.const 0x1p-120 (;=0.000000000000000000000000000000000000752316384526264;)))
          (f64.add
            (local.get 0)
            (f64.const 0x1p+120 (;=1329227995784916000000000000000000000;)))
          (i32.lt_u
            (local.get 2)
            (i32.const 1048576))))
      (drop
        (f64.load offset=8
          (local.get 1))))
    (global.set $__stack_pointer
      (i32.add
        (local.get 1)
        (i32.const 32)))
    (local.get 0)
  )
  (func $_ZN4libm4math5k_tan5k_tan17he70947e0d23fd5ccE (;65;) (type 11) (param f64 f64 i32) (result f64)
    (local i64 i32 f64 f64 f64)
    (block ;; label = @1
      (br_if 0 (;@1;)
        (i32.eqz
          (local.tee 4
            (i64.gt_u
              (i64.and
                (local.tee 3
                  (i64.reinterpret_f64
                    (local.get 0)))
                (i64.const 9223372002495037440))
              (i64.const 4604249089280835584)))))
      (local.set 0
        (f64.add
          (f64.sub
            (f64.const 0x1.921fb54442d18p-1 (;=0.7853981633974483;))
            (f64.abs
              (local.get 0)))
          (f64.sub
            (f64.const 0x1.1a62633145c07p-55 (;=0.00000000000000003061616997868383;))
            (select
              (f64.neg
                (local.get 1))
              (local.get 1)
              (i64.lt_s
                (local.get 3)
                (i64.const 0))))))
      (local.set 1
        (f64.const 0x0p+0 (;=0;))))
    (local.set 7
      (f64.add
        (local.get 0)
        (local.tee 5
          (f64.add
            (f64.mul
              (local.tee 6
                (f64.mul
                  (local.get 0)
                  (local.tee 5
                    (f64.mul
                      (local.get 0)
                      (local.get 0)))))
              (f64.const 0x1.5555555555563p-2 (;=0.3333333333333341;)))
            (f64.add
              (local.get 1)
              (f64.mul
                (local.get 5)
                (f64.add
                  (local.get 1)
                  (f64.mul
                    (local.get 6)
                    (f64.add
                      (f64.add
                        (f64.mul
                          (local.tee 7
                            (f64.mul
                              (local.get 5)
                              (local.get 5)))
                          (f64.add
                            (f64.mul
                              (local.get 7)
                              (f64.add
                                (f64.mul
                                  (local.get 7)
                                  (f64.add
                                    (f64.mul
                                      (local.get 7)
                                      (f64.add
                                        (f64.mul
                                          (local.get 7)
                                          (f64.const -0x1.375cbdb605373p-16 (;=-0.000018558637485527546;)))
                                        (f64.const 0x1.47e88a03792a6p-14 (;=0.00007817944429395571;))))
                                    (f64.const 0x1.344d8f2f26501p-11 (;=0.0005880412408202641;))))
                                (f64.const 0x1.d6d22c9560328p-9 (;=0.0035920791075913124;))))
                            (f64.const 0x1.664f48406d637p-6 (;=0.021869488294859542;))))
                        (f64.const 0x1.111111110fe7ap-3 (;=0.13333333333320124;)))
                      (f64.mul
                        (local.get 5)
                        (f64.add
                          (f64.mul
                            (local.get 7)
                            (f64.add
                              (f64.mul
                                (local.get 7)
                                (f64.add
                                  (f64.mul
                                    (local.get 7)
                                    (f64.add
                                      (f64.mul
                                        (local.get 7)
                                        (f64.add
                                          (f64.mul
                                            (local.get 7)
                                            (f64.const 0x1.b2a7074bf7ad4p-16 (;=0.00002590730518636337;)))
                                          (f64.const 0x1.2b80f32f0a7e9p-14 (;=0.00007140724913826082;))))
                                      (f64.const 0x1.026f71a8d1068p-12 (;=0.0002464631348184699;))))
                                  (f64.const 0x1.7dbc8fee08315p-10 (;=0.0014562094543252903;))))
                              (f64.const 0x1.226e3e96e8493p-7 (;=0.0088632398235993;))))
                          (f64.const 0x1.ba1ba1bb341fep-5 (;=0.05396825397622605;)))))))))))))
    (block ;; label = @1
      (br_if 0 (;@1;)
        (local.get 4))
      (block ;; label = @2
        (br_if 0 (;@2;)
          (i32.eqz
            (local.get 2)))
        (local.set 7
          (f64.add
            (f64.mul
              (local.tee 1
                (f64.div
                  (f64.const -0x1p+0 (;=-1;))
                  (local.get 7)))
              (f64.add
                (f64.add
                  (f64.mul
                    (local.tee 6
                      (f64.reinterpret_i64
                        (i64.and
                          (i64.reinterpret_f64
                            (local.get 7))
                          (i64.const -4294967296))))
                    (local.tee 7
                      (f64.reinterpret_i64
                        (i64.and
                          (i64.reinterpret_f64
                            (local.get 1))
                          (i64.const -4294967296)))))
                  (f64.const 0x1p+0 (;=1;)))
                (f64.mul
                  (f64.sub
                    (local.get 5)
                    (f64.sub
                      (local.get 6)
                      (local.get 0)))
                  (local.get 7))))
            (local.get 7))))
      (return
        (local.get 7)))
    (select
      (f64.neg
        (local.tee 7
          (f64.sub
            (local.tee 1
              (f64.sub
                (f64.const 0x1p+0 (;=1;))
                (f64.convert_i32_u
                  (i32.shl
                    (local.get 2)
                    (i32.const 1)))))
            (f64.add
              (local.tee 7
                (f64.add
                  (local.get 0)
                  (f64.sub
                    (local.get 5)
                    (f64.div
                      (f64.mul
                        (local.get 7)
                        (local.get 7))
                      (f64.add
                        (local.get 1)
                        (local.get 7))))))
              (local.get 7)))))
      (local.get 7)
      (i64.lt_s
        (local.get 3)
        (i64.const 0)))
  )
  (func $libm_tanf (;66;) (type 1) (param f32) (result f32)
    (local i32 f64 i32 i32 f64 f64)
    (global.set $__stack_pointer
      (local.tee 1
        (i32.sub
          (global.get $__stack_pointer)
          (i32.const 16))))
    (local.set 2
      (f64.promote_f32
        (local.get 0)))
    (block ;; label = @1
      (block ;; label = @2
        (br_if 0 (;@2;)
          (i32.lt_u
            (local.tee 4
              (i32.and
                (local.tee 3
                  (i32.reinterpret_f32
                    (local.get 0)))
                (i32.const 2147483647)))
            (i32.const 1061752795)))
        (block ;; label = @3
          (br_if 0 (;@3;)
            (i32.lt_u
              (local.get 4)
              (i32.const 1081824210)))
          (block ;; label = @4
            (br_if 0 (;@4;)
              (i32.lt_u
                (local.get 4)
                (i32.const 1088565718)))
            (block ;; label = @5
              (br_if 0 (;@5;)
                (i32.gt_u
                  (local.get 4)
                  (i32.const 2139095039)))
              (call $_ZN4libm4math9rem_pio2f9rem_pio2f17h3b6adc3e5afb8880E
                (local.get 1)
                (local.get 0))
              (local.set 0
                (f32.demote_f64
                  (select
                    (f64.div
                      (f64.const -0x1p+0 (;=-1;))
                      (local.tee 2
                        (f64.add
                          (f64.add
                            (local.tee 5
                              (f64.load offset=8
                                (local.get 1)))
                            (f64.mul
                              (local.tee 5
                                (f64.mul
                                  (local.get 5)
                                  (local.tee 2
                                    (f64.mul
                                      (local.get 5)
                                      (local.get 5)))))
                              (f64.add
                                (f64.mul
                                  (local.get 2)
                                  (f64.const 0x1.112fd38999f72p-3 (;=0.13339200271297674;)))
                                (f64.const 0x1.5554d3418c99fp-2 (;=0.3333313950307914;)))))
                          (f64.mul
                            (f64.mul
                              (local.get 5)
                              (local.tee 6
                                (f64.mul
                                  (local.get 2)
                                  (local.get 2))))
                            (f64.add
                              (f64.add
                                (f64.mul
                                  (local.get 2)
                                  (f64.const 0x1.91df3908c33cep-6 (;=0.024528318116654728;)))
                                (f64.const 0x1.b54c91d865afep-5 (;=0.05338123784456704;)))
                              (f64.mul
                                (local.get 6)
                                (f64.add
                                  (f64.mul
                                    (local.get 2)
                                    (f64.const 0x1.362b9bf971bcdp-7 (;=0.009465647849436732;)))
                                  (f64.const 0x1.85dadfcecf44ep-9 (;=0.002974357433599673;)))))))))
                    (local.get 2)
                    (i32.and
                      (i32.load
                        (local.get 1))
                      (i32.const 1)))))
              (br 4 (;@1;)))
            (local.set 0
              (f32.sub
                (local.get 0)
                (local.get 0)))
            (br 3 (;@1;)))
          (block ;; label = @4
            (br_if 0 (;@4;)
              (i32.lt_u
                (local.get 4)
                (i32.const 1085271520)))
            (local.set 0
              (f32.demote_f64
                (f64.add
                  (f64.add
                    (local.tee 5
                      (f64.add
                        (select
                          (f64.const -0x1.921fb54442d18p+2 (;=-6.283185307179586;))
                          (f64.const 0x1.921fb54442d18p+2 (;=6.283185307179586;))
                          (i32.gt_s
                            (local.get 3)
                            (i32.const -1)))
                        (local.get 2)))
                    (f64.mul
                      (local.tee 5
                        (f64.mul
                          (local.get 5)
                          (local.tee 2
                            (f64.mul
                              (local.get 5)
                              (local.get 5)))))
                      (f64.add
                        (f64.mul
                          (local.get 2)
                          (f64.const 0x1.112fd38999f72p-3 (;=0.13339200271297674;)))
                        (f64.const 0x1.5554d3418c99fp-2 (;=0.3333313950307914;)))))
                  (f64.mul
                    (f64.mul
                      (local.get 5)
                      (local.tee 6
                        (f64.mul
                          (local.get 2)
                          (local.get 2))))
                    (f64.add
                      (f64.add
                        (f64.mul
                          (local.get 2)
                          (f64.const 0x1.91df3908c33cep-6 (;=0.024528318116654728;)))
                        (f64.const 0x1.b54c91d865afep-5 (;=0.05338123784456704;)))
                      (f64.mul
                        (local.get 6)
                        (f64.add
                          (f64.mul
                            (local.get 2)
                            (f64.const 0x1.362b9bf971bcdp-7 (;=0.009465647849436732;)))
                          (f64.const 0x1.85dadfcecf44ep-9 (;=0.002974357433599673;)))))))))
            (br 3 (;@1;)))
          (local.set 0
            (f32.demote_f64
              (f64.div
                (f64.const -0x1p+0 (;=-1;))
                (f64.add
                  (f64.add
                    (local.tee 5
                      (f64.add
                        (select
                          (f64.const -0x1.2d97c7f3321d2p+2 (;=-4.71238898038469;))
                          (f64.const 0x1.2d97c7f3321d2p+2 (;=4.71238898038469;))
                          (i32.gt_s
                            (local.get 3)
                            (i32.const -1)))
                        (local.get 2)))
                    (f64.mul
                      (local.tee 5
                        (f64.mul
                          (local.get 5)
                          (local.tee 2
                            (f64.mul
                              (local.get 5)
                              (local.get 5)))))
                      (f64.add
                        (f64.mul
                          (local.get 2)
                          (f64.const 0x1.112fd38999f72p-3 (;=0.13339200271297674;)))
                        (f64.const 0x1.5554d3418c99fp-2 (;=0.3333313950307914;)))))
                  (f64.mul
                    (f64.mul
                      (local.get 5)
                      (local.tee 6
                        (f64.mul
                          (local.get 2)
                          (local.get 2))))
                    (f64.add
                      (f64.add
                        (f64.mul
                          (local.get 2)
                          (f64.const 0x1.91df3908c33cep-6 (;=0.024528318116654728;)))
                        (f64.const 0x1.b54c91d865afep-5 (;=0.05338123784456704;)))
                      (f64.mul
                        (local.get 6)
                        (f64.add
                          (f64.mul
                            (local.get 2)
                            (f64.const 0x1.362b9bf971bcdp-7 (;=0.009465647849436732;)))
                          (f64.const 0x1.85dadfcecf44ep-9 (;=0.002974357433599673;))))))))))
          (br 2 (;@1;)))
        (block ;; label = @3
          (br_if 0 (;@3;)
            (i32.lt_u
              (local.get 4)
              (i32.const 1075235812)))
          (local.set 0
            (f32.demote_f64
              (f64.add
                (f64.add
                  (local.tee 5
                    (f64.add
                      (select
                        (f64.const -0x1.921fb54442d18p+1 (;=-3.141592653589793;))
                        (f64.const 0x1.921fb54442d18p+1 (;=3.141592653589793;))
                        (i32.gt_s
                          (local.get 3)
                          (i32.const -1)))
                      (local.get 2)))
                  (f64.mul
                    (local.tee 5
                      (f64.mul
                        (local.get 5)
                        (local.tee 2
                          (f64.mul
                            (local.get 5)
                            (local.get 5)))))
                    (f64.add
                      (f64.mul
                        (local.get 2)
                        (f64.const 0x1.112fd38999f72p-3 (;=0.13339200271297674;)))
                      (f64.const 0x1.5554d3418c99fp-2 (;=0.3333313950307914;)))))
                (f64.mul
                  (f64.mul
                    (local.get 5)
                    (local.tee 6
                      (f64.mul
                        (local.get 2)
                        (local.get 2))))
                  (f64.add
                    (f64.add
                      (f64.mul
                        (local.get 2)
                        (f64.const 0x1.91df3908c33cep-6 (;=0.024528318116654728;)))
                      (f64.const 0x1.b54c91d865afep-5 (;=0.05338123784456704;)))
                    (f64.mul
                      (local.get 6)
                      (f64.add
                        (f64.mul
                          (local.get 2)
                          (f64.const 0x1.362b9bf971bcdp-7 (;=0.009465647849436732;)))
                        (f64.const 0x1.85dadfcecf44ep-9 (;=0.002974357433599673;)))))))))
          (br 2 (;@1;)))
        (local.set 0
          (f32.demote_f64
            (f64.div
              (f64.const -0x1p+0 (;=-1;))
              (f64.add
                (f64.add
                  (local.tee 5
                    (f64.add
                      (select
                        (f64.const -0x1.921fb54442d18p+0 (;=-1.5707963267948966;))
                        (f64.const 0x1.921fb54442d18p+0 (;=1.5707963267948966;))
                        (i32.gt_s
                          (local.get 3)
                          (i32.const -1)))
                      (local.get 2)))
                  (f64.mul
                    (local.tee 5
                      (f64.mul
                        (local.get 5)
                        (local.tee 2
                          (f64.mul
                            (local.get 5)
                            (local.get 5)))))
                    (f64.add
                      (f64.mul
                        (local.get 2)
                        (f64.const 0x1.112fd38999f72p-3 (;=0.13339200271297674;)))
                      (f64.const 0x1.5554d3418c99fp-2 (;=0.3333313950307914;)))))
                (f64.mul
                  (f64.mul
                    (local.get 5)
                    (local.tee 6
                      (f64.mul
                        (local.get 2)
                        (local.get 2))))
                  (f64.add
                    (f64.add
                      (f64.mul
                        (local.get 2)
                        (f64.const 0x1.91df3908c33cep-6 (;=0.024528318116654728;)))
                      (f64.const 0x1.b54c91d865afep-5 (;=0.05338123784456704;)))
                    (f64.mul
                      (local.get 6)
                      (f64.add
                        (f64.mul
                          (local.get 2)
                          (f64.const 0x1.362b9bf971bcdp-7 (;=0.009465647849436732;)))
                        (f64.const 0x1.85dadfcecf44ep-9 (;=0.002974357433599673;))))))))))
        (br 1 (;@1;)))
      (block ;; label = @2
        (br_if 0 (;@2;)
          (i32.lt_u
            (local.get 4)
            (i32.const 964689920)))
        (local.set 0
          (f32.demote_f64
            (f64.add
              (f64.add
                (f64.mul
                  (local.tee 6
                    (f64.mul
                      (local.tee 5
                        (f64.mul
                          (local.get 2)
                          (local.get 2)))
                      (local.get 2)))
                  (f64.add
                    (f64.mul
                      (local.get 5)
                      (f64.const 0x1.112fd38999f72p-3 (;=0.13339200271297674;)))
                    (f64.const 0x1.5554d3418c99fp-2 (;=0.3333313950307914;))))
                (local.get 2))
              (f64.mul
                (f64.mul
                  (local.get 6)
                  (local.tee 2
                    (f64.mul
                      (local.get 5)
                      (local.get 5))))
                (f64.add
                  (f64.add
                    (f64.mul
                      (local.get 5)
                      (f64.const 0x1.91df3908c33cep-6 (;=0.024528318116654728;)))
                    (f64.const 0x1.b54c91d865afep-5 (;=0.05338123784456704;)))
                  (f64.mul
                    (local.get 2)
                    (f64.add
                      (f64.mul
                        (local.get 5)
                        (f64.const 0x1.362b9bf971bcdp-7 (;=0.009465647849436732;)))
                      (f64.const 0x1.85dadfcecf44ep-9 (;=0.002974357433599673;)))))))))
        (br 1 (;@1;)))
      (f32.store
        (local.get 1)
        (select
          (f32.mul
            (local.get 0)
            (f32.const 0x1p-120 (;=0.0000000000000000000000000000000000007523164;)))
          (f32.add
            (local.get 0)
            (f32.const 0x1p+120 (;=1329228000000000000000000000000000000;)))
          (i32.lt_u
            (local.get 4)
            (i32.const 8388608))))
      (drop
        (f32.load
          (local.get 1))))
    (global.set $__stack_pointer
      (i32.add
        (local.get 1)
        (i32.const 16)))
    (local.get 0)
  )
  (func $libm_tanh (;67;) (type 0) (param f64) (result f64)
    (local i32 f64 i64)
    (global.set $__stack_pointer
      (local.tee 1
        (i32.sub
          (global.get $__stack_pointer)
          (i32.const 16))))
    (block ;; label = @1
      (block ;; label = @2
        (block ;; label = @3
          (br_if 0 (;@3;)
            (i64.gt_u
              (local.tee 3
                (i64.reinterpret_f64
                  (local.tee 2
                    (f64.abs
                      (local.get 0)))))
              (i64.const 4603122931675955199)))
          (br_if 1 (;@2;)
            (i64.gt_u
              (local.get 3)
              (i64.const 4598272728187797503)))
          (block ;; label = @4
            (br_if 0 (;@4;)
              (i64.gt_u
                (local.get 3)
                (i64.const 4503599627370495)))
            (f32.store offset=12
              (local.get 1)
              (f32.demote_f64
                (local.get 2)))
            (drop
              (f32.load offset=12
                (local.get 1)))
            (br 3 (;@1;)))
          (local.set 2
            (f64.div
              (f64.neg
                (local.tee 2
                  (call $_ZN4libm4math5expm15expm117hf425b3a732f15702E
                    (f64.mul
                      (local.get 2)
                      (f64.const -0x1p+1 (;=-2;))))))
              (f64.add
                (local.get 2)
                (f64.const 0x1p+1 (;=2;)))))
          (br 2 (;@1;)))
        (block ;; label = @3
          (br_if 0 (;@3;)
            (i64.gt_u
              (local.get 3)
              (i64.const 4626322721511309311)))
          (local.set 2
            (f64.sub
              (f64.const 0x1p+0 (;=1;))
              (f64.div
                (f64.const 0x1p+1 (;=2;))
                (f64.add
                  (call $_ZN4libm4math5expm15expm117hf425b3a732f15702E
                    (f64.add
                      (local.get 2)
                      (local.get 2)))
                  (f64.const 0x1p+1 (;=2;))))))
          (br 2 (;@1;)))
        (local.set 2
          (f64.add
            (f64.div
              (f64.const -0x0p+0 (;=-0;))
              (local.get 2))
            (f64.const 0x1p+0 (;=1;))))
        (br 1 (;@1;)))
      (local.set 2
        (f64.div
          (local.tee 2
            (call $_ZN4libm4math5expm15expm117hf425b3a732f15702E
              (f64.add
                (local.get 2)
                (local.get 2))))
          (f64.add
            (local.get 2)
            (f64.const 0x1p+1 (;=2;))))))
    (global.set $__stack_pointer
      (i32.add
        (local.get 1)
        (i32.const 16)))
    (select
      (f64.neg
        (local.get 2))
      (local.get 2)
      (i64.lt_s
        (i64.reinterpret_f64
          (local.get 0))
        (i64.const 0)))
  )
  (func $libm_tanhf (;68;) (type 1) (param f32) (result f32)
    (local i32 f32 i32)
    (global.set $__stack_pointer
      (local.tee 1
        (i32.sub
          (global.get $__stack_pointer)
          (i32.const 16))))
    (block ;; label = @1
      (block ;; label = @2
        (block ;; label = @3
          (br_if 0 (;@3;)
            (i32.gt_u
              (local.tee 3
                (i32.reinterpret_f32
                  (local.tee 2
                    (f32.abs
                      (local.get 0)))))
              (i32.const 1057791828)))
          (br_if 1 (;@2;)
            (i32.gt_u
              (local.get 3)
              (i32.const 1048757624)))
          (block ;; label = @4
            (br_if 0 (;@4;)
              (i32.gt_u
                (local.get 3)
                (i32.const 8388607)))
            (f32.store offset=12
              (local.get 1)
              (f32.mul
                (local.get 0)
                (local.get 0)))
            (drop
              (f32.load offset=12
                (local.get 1)))
            (br 3 (;@1;)))
          (local.set 2
            (f32.div
              (f32.neg
                (local.tee 2
                  (call $_ZN4libm4math6expm1f6expm1f17h8c93774ba8d4df49E
                    (f32.mul
                      (local.get 2)
                      (f32.const -0x1p+1 (;=-2;))))))
              (f32.add
                (local.get 2)
                (f32.const 0x1p+1 (;=2;)))))
          (br 2 (;@1;)))
        (block ;; label = @3
          (br_if 0 (;@3;)
            (i32.gt_u
              (local.get 3)
              (i32.const 1092616192)))
          (local.set 2
            (f32.sub
              (f32.const 0x1p+0 (;=1;))
              (f32.div
                (f32.const 0x1p+1 (;=2;))
                (f32.add
                  (call $_ZN4libm4math6expm1f6expm1f17h8c93774ba8d4df49E
                    (f32.add
                      (local.get 2)
                      (local.get 2)))
                  (f32.const 0x1p+1 (;=2;))))))
          (br 2 (;@1;)))
        (local.set 2
          (f32.add
            (f32.div
              (f32.const 0x0p+0 (;=0;))
              (local.get 2))
            (f32.const 0x1p+0 (;=1;))))
        (br 1 (;@1;)))
      (local.set 2
        (f32.div
          (local.tee 2
            (call $_ZN4libm4math6expm1f6expm1f17h8c93774ba8d4df49E
              (f32.add
                (local.get 2)
                (local.get 2))))
          (f32.add
            (local.get 2)
            (f32.const 0x1p+1 (;=2;))))))
    (global.set $__stack_pointer
      (i32.add
        (local.get 1)
        (i32.const 16)))
    (select
      (f32.neg
        (local.get 2))
      (local.get 2)
      (i32.lt_s
        (i32.reinterpret_f32
          (local.get 0))
        (i32.const 0)))
  )
  (func $_ZN4core9panicking9panic_fmt17h6651313c3e2c6c2fE (;69;) (type 8)
    (unreachable)
  )
  (func $_ZN4libm4math14rem_pio2_large14rem_pio2_large17hd7141cf13a36ffbcE (;70;) (type 12) (param i32 i32 i32 i32 i32) (result i32)
    (local i32 i32 i32 i32 i32 i32 i32 f64 i32 i32 i32 i32 i32 i32 i32 i32 i32 f64 f64 i64 i64 i64 i32 i32 i32)
    (global.set $__stack_pointer
      (local.tee 5
        (i32.sub
          (global.get $__stack_pointer)
          (i32.const 560))))
    (local.set 6
      (i32.const 0))
    (block ;; label = @1
      (br_if 0 (;@1;)
        (local.tee 7
          (i32.eqz
            (i32.const 160))))
      (memory.fill
        (local.get 5)
        (i32.const 0)
        (i32.const 160)))
    (block ;; label = @1
      (br_if 0 (;@1;)
        (local.get 7))
      (memory.fill
        (i32.add
          (local.get 5)
          (i32.const 160))
        (i32.const 0)
        (i32.const 160)))
    (block ;; label = @1
      (br_if 0 (;@1;)
        (local.get 7))
      (memory.fill
        (i32.add
          (local.get 5)
          (i32.const 320))
        (i32.const 0)
        (i32.const 160)))
    (block ;; label = @1
      (br_if 0 (;@1;)
        (i32.eqz
          (i32.const 80)))
      (memory.fill
        (i32.add
          (local.get 5)
          (i32.const 480))
        (i32.const 0)
        (i32.const 80)))
    (local.set 9
      (i32.add
        (local.tee 8
          (i32.load offset=1052888
            (i32.shl
              (local.get 4)
              (i32.const 2))))
        (local.tee 7
          (i32.add
            (local.get 1)
            (i32.const -1)))))
    (local.set 10
      (i32.sub
        (local.tee 11
          (select
            (local.tee 10
              (i32.div_s
                (i32.add
                  (local.get 3)
                  (i32.const -3))
                (i32.const 24)))
            (i32.const 0)
            (i32.gt_s
              (local.get 10)
              (i32.const 0))))
        (local.get 7)))
    (local.set 1
      (i32.add
        (i32.sub
          (i32.shl
            (local.get 11)
            (i32.const 2))
          (i32.shl
            (local.get 1)
            (i32.const 2)))
        (i32.const 1052908)))
    (loop ;; label = @1
      (block ;; label = @2
        (block ;; label = @3
          (br_if 0 (;@3;)
            (i32.ge_s
              (local.get 10)
              (i32.const 0)))
          (local.set 12
            (f64.const 0x0p+0 (;=0;)))
          (br 1 (;@2;)))
        (local.set 12
          (f64.convert_i32_s
            (i32.load
              (local.get 1)))))
      (f64.store
        (i32.add
          (local.get 5)
          (i32.shl
            (local.get 6)
            (i32.const 3)))
        (local.get 12))
      (block ;; label = @2
        (br_if 0 (;@2;)
          (i32.ge_u
            (local.get 6)
            (local.get 9)))
        (local.set 1
          (i32.add
            (local.get 1)
            (i32.const 4)))
        (local.set 10
          (i32.add
            (local.get 10)
            (i32.const 1)))
        (br_if 1 (;@1;)
          (i32.le_u
            (local.tee 6
              (i32.add
                (local.get 6)
                (i32.lt_u
                  (local.get 6)
                  (local.get 9))))
            (local.get 9)))))
    (local.set 10
      (i32.const 0))
    (loop ;; label = @1
      (local.set 9
        (i32.add
          (local.get 10)
          (local.get 7)))
      (local.set 12
        (f64.const 0x0p+0 (;=0;)))
      (local.set 6
        (i32.const 0))
      (block ;; label = @2
        (loop ;; label = @3
          (local.set 12
            (f64.add
              (local.get 12)
              (f64.mul
                (f64.load
                  (i32.add
                    (local.get 0)
                    (i32.shl
                      (local.get 6)
                      (i32.const 3))))
                (f64.load
                  (i32.add
                    (local.get 5)
                    (i32.shl
                      (i32.sub
                        (local.get 9)
                        (local.get 6))
                      (i32.const 3)))))))
          (br_if 1 (;@2;)
            (i32.ge_u
              (local.get 6)
              (local.get 7)))
          (br_if 0 (;@3;)
            (i32.le_u
              (local.tee 6
                (i32.add
                  (local.get 6)
                  (i32.lt_u
                    (local.get 6)
                    (local.get 7))))
              (local.get 7)))))
      (f64.store
        (i32.add
          (i32.add
            (local.get 5)
            (i32.const 320))
          (i32.shl
            (local.get 10)
            (i32.const 3)))
        (local.get 12))
      (block ;; label = @2
        (br_if 0 (;@2;)
          (i32.ge_u
            (local.get 10)
            (local.get 8)))
        (br_if 1 (;@1;)
          (i32.le_u
            (local.tee 10
              (i32.add
                (local.get 10)
                (i32.lt_u
                  (local.get 10)
                  (local.get 8))))
            (local.get 8)))))
    (local.set 14
      (i32.add
        (local.tee 13
          (i32.add
            (i32.add
              (local.get 5)
              (i32.const 480))
            (i32.const -4)))
        (i32.shl
          (local.get 8)
          (i32.const 2))))
    (local.set 16
      (i32.and
        (i32.sub
          (i32.const 47)
          (local.tee 15
            (i32.add
              (local.get 3)
              (i32.mul
                (local.get 11)
                (i32.const -24)))))
        (i32.const 31)))
    (local.set 17
      (i32.and
        (i32.sub
          (i32.const 48)
          (local.get 15))
        (i32.const 31)))
    (local.set 3
      (i32.add
        (local.get 5)
        (i32.const 312)))
    (local.set 19
      (i32.gt_s
        (local.tee 18
          (i32.add
            (local.get 15)
            (i32.const -24)))
        (i32.const 0)))
    (local.set 20
      (i32.add
        (local.get 18)
        (i32.const -1)))
    (local.set 1
      (local.get 8))
    (block ;; label = @1
      (loop ;; label = @2
        (local.set 12
          (f64.load
            (i32.add
              (i32.add
                (local.get 5)
                (i32.const 320))
              (i32.shl
                (local.tee 21
                  (local.get 1))
                (i32.const 3)))))
        (block ;; label = @3
          (br_if 0 (;@3;)
            (i32.eqz
              (local.get 21)))
          (local.set 9
            (i32.add
              (local.get 5)
              (i32.const 480)))
          (local.set 6
            (local.get 21))
          (loop ;; label = @4
            (i32.store
              (local.get 9)
              (i32.trunc_sat_f64_s
                (f64.add
                  (local.get 12)
                  (f64.mul
                    (local.tee 22
                      (f64.convert_i32_s
                        (i32.trunc_sat_f64_s
                          (f64.mul
                            (local.get 12)
                            (f64.const 0x1p-24 (;=0.00000005960464477539063;))))))
                    (f64.const -0x1p+24 (;=-16777216;))))))
            (local.set 12
              (f64.add
                (f64.load
                  (i32.add
                    (local.get 3)
                    (i32.shl
                      (local.get 6)
                      (i32.const 3))))
                (local.get 22)))
            (br_if 1 (;@3;)
              (local.tee 10
                (i32.eq
                  (local.get 6)
                  (i32.const 1))))
            (local.set 9
              (i32.add
                (local.get 9)
                (i32.const 4)))
            (br_if 0 (;@4;)
              (local.tee 6
                (select
                  (i32.const 1)
                  (i32.add
                    (local.get 6)
                    (i32.const -1))
                  (local.get 10))))))
        (block ;; label = @3
          (block ;; label = @4
            (br_if 0 (;@4;)
              (i32.gt_u
                (local.tee 6
                  (i32.and
                    (i32.wrap_i64
                      (i64.shr_u
                        (local.tee 24
                          (i64.reinterpret_f64
                            (local.tee 23
                              (f64.mul
                                (local.tee 22
                                  (call $_ZN4libm4math6scalbn6scalbn17hd5ee51b98c77623bE
                                    (local.get 12)
                                    (local.get 18)))
                                (f64.const 0x1p-3 (;=0.125;))))))
                        (i64.const 52)))
                    (i32.const 2047)))
                (i32.const 1074)))
            (block ;; label = @5
              (br_if 0 (;@5;)
                (i32.gt_u
                  (local.get 6)
                  (i32.const 1022)))
              (local.set 12
                (f64.const 0x0p+0 (;=0;)))
              (br_if 2 (;@3;)
                (i64.gt_s
                  (local.get 24)
                  (i64.const -1)))
              (local.set 12
                (select
                  (local.get 23)
                  (f64.const -0x1p+0 (;=-1;))
                  (f64.eq
                    (local.get 23)
                    (f64.const 0x0p+0 (;=0;)))))
              (br 2 (;@3;)))
            (local.set 12
              (local.get 23))
            (br_if 1 (;@3;)
              (i64.eqz
                (i64.and
                  (local.tee 26
                    (i64.shr_u
                      (i64.const 4503599627370495)
                      (local.tee 25
                        (i64.extend_i32_u
                          (i32.add
                            (local.get 6)
                            (i32.const -1023))))))
                  (local.get 24))))
            (local.set 12
              (f64.reinterpret_i64
                (i64.and
                  (i64.add
                    (i64.and
                      (i64.shr_s
                        (local.get 24)
                        (i64.const 63))
                      (local.get 26))
                    (local.get 24))
                  (i64.shr_s
                    (i64.const -4503599627370496)
                    (local.get 25)))))
            (br 1 (;@3;)))
          (local.set 12
            (local.get 23)))
        (local.set 12
          (f64.sub
            (local.tee 12
              (f64.add
                (local.get 22)
                (f64.mul
                  (local.get 12)
                  (f64.const -0x1p+3 (;=-8;)))))
            (f64.convert_i32_s
              (local.tee 27
                (i32.trunc_sat_f64_s
                  (local.get 12))))))
        (block ;; label = @3
          (block ;; label = @4
            (block ;; label = @5
              (block ;; label = @6
                (block ;; label = @7
                  (br_if 0 (;@7;)
                    (local.get 19))
                  (block ;; label = @8
                    (br_if 0 (;@8;)
                      (local.get 18))
                    (local.set 28
                      (i32.shr_s
                        (i32.load
                          (i32.add
                            (local.get 13)
                            (i32.shl
                              (local.get 21)
                              (i32.const 2))))
                        (i32.const 23)))
                    (br 2 (;@6;)))
                  (local.set 28
                    (i32.const 2))
                  (local.set 29
                    (i32.const 0))
                  (br_if 4 (;@3;)
                    (i32.eqz
                      (f64.ge
                        (local.get 12)
                        (f64.const 0x1p-1 (;=0.5;)))))
                  (br 2 (;@5;)))
                (i32.store
                  (local.tee 6
                    (i32.add
                      (local.get 13)
                      (i32.shl
                        (local.get 21)
                        (i32.const 2))))
                  (local.tee 9
                    (i32.sub
                      (local.tee 6
                        (i32.load
                          (local.get 6)))
                      (i32.shl
                        (local.tee 6
                          (i32.shr_s
                            (local.get 6)
                            (local.get 17)))
                        (local.get 17)))))
                (local.set 28
                  (i32.shr_s
                    (local.get 9)
                    (local.get 16)))
                (local.set 27
                  (i32.add
                    (local.get 6)
                    (local.get 27))))
              (br_if 1 (;@4;)
                (i32.lt_s
                  (local.get 28)
                  (i32.const 1))))
            (local.set 9
              (i32.const 1))
            (block ;; label = @5
              (br_if 0 (;@5;)
                (i32.eqz
                  (local.get 21)))
              (local.set 10
                (i32.const 0))
              (local.set 6
                (i32.add
                  (local.get 5)
                  (i32.const 480)))
              (local.set 1
                (local.get 21))
              (loop ;; label = @6
                (local.set 9
                  (i32.load
                    (local.get 6)))
                (block ;; label = @7
                  (block ;; label = @8
                    (block ;; label = @9
                      (block ;; label = @10
                        (br_if 0 (;@10;)
                          (i32.eqz
                            (local.get 10)))
                        (local.set 10
                          (i32.const 16777215))
                        (br 1 (;@9;)))
                      (br_if 1 (;@8;)
                        (i32.eqz
                          (local.get 9)))
                      (local.set 10
                        (i32.const 16777216)))
                    (i32.store
                      (local.get 6)
                      (i32.sub
                        (local.get 10)
                        (local.get 9)))
                    (local.set 10
                      (i32.const 1))
                    (local.set 9
                      (i32.const 0))
                    (br 1 (;@7;)))
                  (local.set 10
                    (i32.const 0))
                  (local.set 9
                    (i32.const 1)))
                (local.set 6
                  (i32.add
                    (local.get 6)
                    (i32.const 4)))
                (br_if 0 (;@6;)
                  (local.tee 1
                    (i32.add
                      (local.get 1)
                      (i32.const -1))))))
            (block ;; label = @5
              (br_if 0 (;@5;)
                (i32.lt_s
                  (local.get 18)
                  (i32.const 1)))
              (local.set 6
                (i32.const 8388607))
              (block ;; label = @6
                (block ;; label = @7
                  (br_table 1 (;@6;) 0 (;@7;) 2 (;@5;)
                    (local.get 20)))
                (local.set 6
                  (i32.const 4194303)))
              (i32.store
                (local.tee 10
                  (i32.add
                    (local.get 13)
                    (i32.shl
                      (local.get 21)
                      (i32.const 2))))
                (i32.and
                  (i32.load
                    (local.get 10))
                  (local.get 6))))
            (local.set 27
              (i32.add
                (local.get 27)
                (i32.const 1)))
            (local.set 29
              (i32.const 2))
            (br_if 1 (;@3;)
              (i32.ne
                (local.get 28)
                (i32.const 2)))
            (local.set 12
              (f64.sub
                (f64.const 0x1p+0 (;=1;))
                (local.get 12)))
            (br_if 1 (;@3;)
              (local.get 9))
            (local.set 12
              (f64.sub
                (local.get 12)
                (call $_ZN4libm4math6scalbn6scalbn17hd5ee51b98c77623bE
                  (f64.const 0x1p+0 (;=1;))
                  (local.get 18))))
            (br 1 (;@3;)))
          (local.set 29
            (local.get 28)))
        (block ;; label = @3
          (br_if 0 (;@3;)
            (f64.ne
              (local.get 12)
              (f64.const 0x0p+0 (;=0;))))
          (block ;; label = @4
            (br_if 0 (;@4;)
              (i32.gt_u
                (local.get 8)
                (local.tee 6
                  (i32.add
                    (local.get 21)
                    (i32.const -1)))))
            (local.set 9
              (i32.const 0))
            (block ;; label = @5
              (loop ;; label = @6
                (local.set 9
                  (i32.or
                    (i32.load
                      (i32.add
                        (i32.add
                          (local.get 5)
                          (i32.const 480))
                        (i32.shl
                          (local.get 6)
                          (i32.const 2))))
                    (local.get 9)))
                (br_if 1 (;@5;)
                  (i32.ge_u
                    (local.get 8)
                    (local.get 6)))
                (br_if 0 (;@6;)
                  (i32.le_u
                    (local.get 8)
                    (local.tee 6
                      (i32.sub
                        (local.get 6)
                        (i32.lt_u
                          (local.get 8)
                          (local.get 6))))))))
            (br_if 0 (;@4;)
              (i32.eqz
                (local.get 9)))
            (local.set 6
              (i32.add
                (i32.add
                  (i32.add
                    (local.get 5)
                    (i32.const 480))
                  (i32.shl
                    (local.get 21)
                    (i32.const 2)))
                (i32.const -4)))
            (loop ;; label = @5
              (local.set 21
                (i32.add
                  (local.get 21)
                  (i32.const -1)))
              (local.set 18
                (i32.add
                  (local.get 18)
                  (i32.const -24)))
              (local.set 7
                (i32.load
                  (local.get 6)))
              (local.set 6
                (i32.add
                  (local.get 6)
                  (i32.const -4)))
              (br_if 0 (;@5;)
                (i32.eqz
                  (local.get 7)))
              (br 4 (;@1;))))
          (local.set 9
            (i32.const 0))
          (local.set 6
            (local.get 14))
          (loop ;; label = @4
            (local.set 9
              (i32.add
                (local.get 9)
                (i32.const 1)))
            (local.set 10
              (i32.load
                (local.get 6)))
            (local.set 6
              (i32.add
                (local.get 6)
                (i32.const -4)))
            (br_if 0 (;@4;)
              (i32.eqz
                (local.get 10))))
          (br_if 1 (;@2;)
            (i32.ge_u
              (local.get 21)
              (local.tee 1
                (i32.add
                  (local.get 9)
                  (local.get 21)))))
          (local.set 10
            (i32.add
              (local.get 21)
              (i32.const 1)))
          (loop ;; label = @4
            (f64.store
              (i32.add
                (local.get 5)
                (i32.shl
                  (local.tee 9
                    (i32.add
                      (local.get 10)
                      (local.get 7)))
                  (i32.const 3)))
              (f64.convert_i32_s
                (i32.load offset=1052904
                  (i32.shl
                    (i32.add
                      (local.get 10)
                      (local.get 11))
                    (i32.const 2)))))
            (local.set 6
              (i32.const 0))
            (local.set 12
              (f64.const 0x0p+0 (;=0;)))
            (block ;; label = @5
              (loop ;; label = @6
                (local.set 12
                  (f64.add
                    (local.get 12)
                    (f64.mul
                      (f64.load
                        (i32.add
                          (local.get 0)
                          (i32.shl
                            (local.get 6)
                            (i32.const 3))))
                      (f64.load
                        (i32.add
                          (local.get 5)
                          (i32.shl
                            (i32.sub
                              (local.get 9)
                              (local.get 6))
                            (i32.const 3)))))))
                (br_if 1 (;@5;)
                  (i32.ge_u
                    (local.get 6)
                    (local.get 7)))
                (br_if 0 (;@6;)
                  (i32.le_u
                    (local.tee 6
                      (i32.add
                        (local.get 6)
                        (i32.lt_u
                          (local.get 6)
                          (local.get 7))))
                    (local.get 7)))))
            (f64.store
              (i32.add
                (i32.add
                  (local.get 5)
                  (i32.const 320))
                (i32.shl
                  (local.get 10)
                  (i32.const 3)))
              (local.get 12))
            (local.set 6
              (i32.add
                (local.get 10)
                (i32.lt_u
                  (local.get 10)
                  (local.get 1))))
            (br_if 2 (;@2;)
              (i32.ge_u
                (local.get 10)
                (local.get 1)))
            (local.set 10
              (local.get 6))
            (br_if 0 (;@4;)
              (i32.le_u
                (local.get 6)
                (local.get 1)))
            (br 2 (;@2;)))))
      (block ;; label = @2
        (block ;; label = @3
          (br_if 0 (;@3;)
            (f64.ge
              (local.tee 12
                (call $_ZN4libm4math6scalbn6scalbn17hd5ee51b98c77623bE
                  (local.get 12)
                  (i32.sub
                    (i32.const 0)
                    (local.get 18))))
              (f64.const 0x1p+24 (;=16777216;))))
          (local.set 22
            (local.get 12))
          (br 1 (;@2;)))
        (i32.store
          (i32.add
            (i32.add
              (local.get 5)
              (i32.const 480))
            (i32.shl
              (local.get 21)
              (i32.const 2)))
          (i32.trunc_sat_f64_s
            (f64.add
              (local.get 12)
              (f64.mul
                (local.tee 22
                  (f64.convert_i32_s
                    (i32.trunc_sat_f64_s
                      (f64.mul
                        (local.get 12)
                        (f64.const 0x1p-24 (;=0.00000005960464477539063;))))))
                (f64.const -0x1p+24 (;=-16777216;))))))
        (local.set 21
          (i32.add
            (local.get 21)
            (i32.const 1)))
        (local.set 18
          (local.get 15)))
      (i32.store
        (i32.add
          (i32.add
            (local.get 5)
            (i32.const 480))
          (i32.shl
            (local.get 21)
            (i32.const 2)))
        (i32.trunc_sat_f64_s
          (local.get 22))))
    (local.set 6
      (i32.add
        (i32.add
          (local.get 5)
          (i32.const 320))
        (i32.shl
          (local.get 21)
          (i32.const 3))))
    (local.set 7
      (i32.add
        (i32.add
          (local.get 5)
          (i32.const 480))
        (i32.shl
          (local.get 21)
          (i32.const 2))))
    (local.set 12
      (call $_ZN4libm4math6scalbn6scalbn17hd5ee51b98c77623bE
        (f64.const 0x1p+0 (;=1;))
        (local.get 18)))
    (local.set 0
      (local.get 21))
    (loop ;; label = @1
      (f64.store
        (local.get 6)
        (f64.mul
          (local.get 12)
          (f64.convert_i32_s
            (i32.load
              (local.get 7)))))
      (local.set 6
        (i32.add
          (local.get 6)
          (i32.const -8)))
      (local.set 7
        (i32.add
          (local.get 7)
          (i32.const -4)))
      (local.set 12
        (f64.mul
          (local.get 12)
          (f64.const 0x1p-24 (;=0.00000005960464477539063;))))
      (br_if 0 (;@1;)
        (i32.ne
          (local.tee 0
            (i32.add
              (local.get 0)
              (i32.const -1)))
          (i32.const -1))))
    (local.set 0
      (i32.add
        (i32.add
          (local.get 5)
          (i32.const 320))
        (i32.shl
          (local.get 21)
          (i32.const 3))))
    (local.set 6
      (local.get 21))
    (loop ;; label = @1
      (local.set 9
        (i32.add
          (select
            (local.get 8)
            (local.tee 1
              (i32.sub
                (local.get 21)
                (local.tee 10
                  (local.get 6))))
            (i32.lt_u
              (local.get 8)
              (local.get 1)))
          (i32.const 1)))
      (local.set 12
        (f64.const 0x0p+0 (;=0;)))
      (local.set 6
        (i32.const 0))
      (local.set 7
        (i32.const 0))
      (loop ;; label = @2
        (local.set 12
          (f64.add
            (local.get 12)
            (f64.mul
              (f64.load
                (i32.add
                  (local.get 6)
                  (i32.const 1053168)))
              (f64.load
                (i32.add
                  (local.get 0)
                  (local.get 6))))))
        (local.set 6
          (i32.add
            (local.get 6)
            (i32.const 8)))
        (br_if 0 (;@2;)
          (i32.ne
            (local.get 9)
            (local.tee 7
              (i32.add
                (local.get 7)
                (i32.const 1))))))
      (f64.store
        (i32.add
          (i32.add
            (local.get 5)
            (i32.const 160))
          (i32.shl
            (local.get 1)
            (i32.const 3)))
        (local.get 12))
      (local.set 0
        (i32.add
          (local.get 0)
          (i32.const -8)))
      (local.set 6
        (i32.add
          (local.get 10)
          (i32.const -1)))
      (br_if 0 (;@1;)
        (local.get 10)))
    (block ;; label = @1
      (block ;; label = @2
        (br_if 0 (;@2;)
          (i32.eqz
            (local.get 4)))
        (local.set 6
          (i32.add
            (i32.add
              (local.get 5)
              (i32.const 160))
            (i32.shl
              (local.get 21)
              (i32.const 3))))
        (local.set 12
          (f64.const 0x0p+0 (;=0;)))
        (local.set 7
          (local.get 21))
        (loop ;; label = @3
          (local.set 12
            (f64.add
              (local.get 12)
              (f64.load
                (local.get 6))))
          (local.set 6
            (i32.add
              (local.get 6)
              (i32.const -8)))
          (br_if 0 (;@3;)
            (i32.ne
              (local.tee 7
                (i32.add
                  (local.get 7)
                  (i32.const -1)))
              (i32.const -1))))
        (f64.store
          (local.get 2)
          (select
            (f64.neg
              (local.get 12))
            (local.get 12)
            (local.get 29)))
        (local.set 12
          (f64.sub
            (f64.load offset=160
              (local.get 5))
            (local.get 12)))
        (block ;; label = @3
          (br_if 0 (;@3;)
            (i32.eqz
              (local.get 21)))
          (local.set 6
            (i32.const 1))
          (loop ;; label = @4
            (local.set 12
              (f64.add
                (local.get 12)
                (f64.load
                  (i32.add
                    (i32.add
                      (local.get 5)
                      (i32.const 160))
                    (i32.shl
                      (local.get 6)
                      (i32.const 3))))))
            (br_if 1 (;@3;)
              (i32.ge_u
                (local.get 6)
                (local.get 21)))
            (br_if 0 (;@4;)
              (i32.le_u
                (local.tee 6
                  (i32.add
                    (local.get 6)
                    (i32.lt_u
                      (local.get 6)
                      (local.get 21))))
                (local.get 21)))))
        (f64.store offset=8
          (local.get 2)
          (select
            (f64.neg
              (local.get 12))
            (local.get 12)
            (local.get 29)))
        (br 1 (;@1;)))
      (local.set 6
        (i32.add
          (i32.add
            (local.get 5)
            (i32.const 160))
          (i32.shl
            (local.get 21)
            (i32.const 3))))
      (local.set 12
        (f64.const 0x0p+0 (;=0;)))
      (loop ;; label = @2
        (local.set 12
          (f64.add
            (local.get 12)
            (f64.load
              (local.get 6))))
        (local.set 6
          (i32.add
            (local.get 6)
            (i32.const -8)))
        (br_if 0 (;@2;)
          (i32.ne
            (local.tee 21
              (i32.add
                (local.get 21)
                (i32.const -1)))
            (i32.const -1))))
      (f64.store
        (local.get 2)
        (select
          (f64.neg
            (local.get 12))
          (local.get 12)
          (local.get 29))))
    (global.set $__stack_pointer
      (i32.add
        (local.get 5)
        (i32.const 560)))
    (i32.and
      (local.get 27)
      (i32.const 7))
  )
  (func $_ZN4libm4math8rem_pio28rem_pio26medium17h661801483d1aa963E (;71;) (type 13) (param i32 f64 i32)
    (local f64 f64 f64 f64)
    (block ;; label = @1
      (br_if 0 (;@1;)
        (i32.lt_s
          (i32.sub
            (local.tee 2
              (i32.shr_u
                (local.get 2)
                (i32.const 20)))
            (i32.and
              (i32.wrap_i64
                (i64.shr_u
                  (i64.reinterpret_f64
                    (local.tee 5
                      (f64.sub
                        (local.tee 1
                          (f64.add
                            (local.get 1)
                            (f64.mul
                              (local.tee 3
                                (f64.add
                                  (f64.add
                                    (f64.mul
                                      (local.get 1)
                                      (f64.const 0x1.45f306dc9c883p-1 (;=0.6366197723675814;)))
                                    (f64.const 0x1.8p+52 (;=6755399441055744;)))
                                  (f64.const -0x1.8p+52 (;=-6755399441055744;))))
                              (f64.const -0x1.921fb544p+0 (;=-1.5707963267341256;)))))
                        (local.tee 4
                          (f64.mul
                            (local.get 3)
                            (f64.const 0x1.0b4611a626331p-34 (;=0.00000000006077100506506192;)))))))
                  (i64.const 52)))
              (i32.const 2047)))
          (i32.const 17)))
      (block ;; label = @2
        (br_if 0 (;@2;)
          (i32.gt_s
            (i32.sub
              (local.get 2)
              (i32.and
                (i32.wrap_i64
                  (i64.shr_u
                    (i64.reinterpret_f64
                      (local.tee 5
                        (f64.sub
                          (local.tee 6
                            (f64.sub
                              (local.get 1)
                              (local.tee 5
                                (f64.mul
                                  (local.get 3)
                                  (f64.const 0x1.0b4611a6p-34 (;=0.00000000006077100506303966;))))))
                          (local.tee 4
                            (f64.sub
                              (f64.mul
                                (local.get 3)
                                (f64.const 0x1.3198a2e037073p-69 (;=0.0000000000000000000020222662487959506;)))
                              (f64.sub
                                (f64.sub
                                  (local.get 1)
                                  (local.get 6))
                                (local.get 5)))))))
                    (i64.const 52)))
                (i32.const 2047)))
            (i32.const 49)))
        (local.set 1
          (local.get 6))
        (br 1 (;@1;)))
      (local.set 5
        (f64.sub
          (local.tee 1
            (f64.sub
              (local.get 6)
              (local.tee 5
                (f64.mul
                  (local.get 3)
                  (f64.const 0x1.3198a2ep-69 (;=0.0000000000000000000020222662487111665;))))))
          (local.tee 4
            (f64.sub
              (f64.mul
                (local.get 3)
                (f64.const 0x1.b839a252049c1p-104 (;=0.000000000000000000000000000000084784276603689;)))
              (f64.sub
                (f64.sub
                  (local.get 6)
                  (local.get 1))
                (local.get 5)))))))
    (f64.store
      (local.get 0)
      (local.get 5))
    (i32.store offset=8
      (local.get 0)
      (i32.trunc_sat_f64_s
        (local.get 3)))
    (f64.store offset=16
      (local.get 0)
      (f64.sub
        (f64.sub
          (local.get 1)
          (local.get 5))
        (local.get 4)))
  )
  (func $__ashlti3 (;72;) (type 14) (param i32 i64 i64 i32)
    (local i64)
    (block ;; label = @1
      (block ;; label = @2
        (br_if 0 (;@2;)
          (i32.and
            (local.get 3)
            (i32.const 64)))
        (br_if 1 (;@1;)
          (i32.eqz
            (local.get 3)))
        (local.set 2
          (i64.or
            (i64.shl
              (local.get 2)
              (local.tee 4
                (i64.extend_i32_u
                  (i32.and
                    (local.get 3)
                    (i32.const 63)))))
            (i64.shr_u
              (local.get 1)
              (i64.extend_i32_u
                (i32.and
                  (i32.sub
                    (i32.const 0)
                    (local.get 3))
                  (i32.const 63))))))
        (local.set 1
          (i64.shl
            (local.get 1)
            (local.get 4)))
        (br 1 (;@1;)))
      (local.set 2
        (i64.shl
          (local.get 1)
          (i64.extend_i32_u
            (i32.and
              (local.get 3)
              (i32.const 63)))))
      (local.set 1
        (i64.const 0)))
    (i64.store
      (local.get 0)
      (local.get 1))
    (i64.store offset=8
      (local.get 0)
      (local.get 2))
  )
  (func $__multi3 (;73;) (type 15) (param i32 i64 i64 i64 i64)
    (local i64 i64 i64 i64 i64 i64)
    (i64.store
      (local.get 0)
      (local.tee 10
        (i64.add
          (local.tee 7
            (i64.mul
              (local.tee 5
                (i64.and
                  (local.get 3)
                  (i64.const 4294967295)))
              (local.tee 6
                (i64.and
                  (local.get 1)
                  (i64.const 4294967295)))))
          (i64.shl
            (local.tee 5
              (i64.add
                (local.tee 6
                  (i64.mul
                    (local.tee 8
                      (i64.shr_u
                        (local.get 3)
                        (i64.const 32)))
                    (local.get 6)))
                (i64.mul
                  (local.get 5)
                  (local.tee 9
                    (i64.shr_u
                      (local.get 1)
                      (i64.const 32))))))
            (i64.const 32)))))
    (i64.store offset=8
      (local.get 0)
      (i64.add
        (i64.add
          (i64.add
            (i64.mul
              (local.get 8)
              (local.get 9))
            (i64.or
              (i64.shl
                (i64.extend_i32_u
                  (i64.lt_u
                    (local.get 5)
                    (local.get 6)))
                (i64.const 32))
              (i64.shr_u
                (local.get 5)
                (i64.const 32))))
          (i64.extend_i32_u
            (i64.lt_u
              (local.get 10)
              (local.get 7))))
        (i64.add
          (i64.mul
            (local.get 4)
            (local.get 1))
          (i64.mul
            (local.get 3)
            (local.get 2)))))
  )
  (func $_ZN17compiler_builtins3int19specialized_div_rem12u128_div_rem17h8d9ba2662c058edeE (;74;) (type 15) (param i32 i64 i64 i64 i64)
    (local i32 i64 i32 i32 i32 i64 i64 i64 i64)
    (global.set $__stack_pointer
      (local.tee 5
        (i32.sub
          (global.get $__stack_pointer)
          (i32.const 176))))
    (local.set 6
      (i64.const 0))
    (block ;; label = @1
      (block ;; label = @2
        (block ;; label = @3
          (block ;; label = @4
            (br_if 0 (;@4;)
              (i32.le_u
                (local.tee 7
                  (i32.wrap_i64
                    (select
                      (i64.clz
                        (local.get 4))
                      (i64.add
                        (i64.clz
                          (local.get 3))
                        (i64.const 64))
                      (i64.ne
                        (local.get 4)
                        (i64.const 0)))))
                (local.tee 8
                  (i32.wrap_i64
                    (select
                      (i64.clz
                        (local.get 2))
                      (i64.add
                        (i64.clz
                          (local.get 1))
                        (i64.const 64))
                      (i64.ne
                        (local.get 2)
                        (i64.const 0)))))))
            (br_if 1 (;@3;)
              (i32.gt_u
                (local.get 8)
                (i32.const 63)))
            (br_if 2 (;@2;)
              (i32.gt_u
                (local.get 7)
                (i32.const 95)))
            (block ;; label = @5
              (block ;; label = @6
                (block ;; label = @7
                  (br_if 0 (;@7;)
                    (i32.lt_u
                      (i32.sub
                        (local.get 7)
                        (local.get 8))
                      (i32.const 32)))
                  (call $__lshrti3
                    (i32.add
                      (local.get 5)
                      (i32.const 160))
                    (local.get 3)
                    (local.get 4)
                    (local.tee 9
                      (i32.sub
                        (i32.const 96)
                        (local.get 7))))
                  (local.set 10
                    (i64.add
                      (i64.load32_u offset=160
                        (local.get 5))
                      (i64.const 1)))
                  (local.set 11
                    (i64.const 0))
                  (local.set 6
                    (i64.const 0))
                  (br 1 (;@6;)))
                (call $__lshrti3
                  (i32.add
                    (local.get 5)
                    (i32.const 48))
                  (local.get 1)
                  (local.get 2)
                  (local.tee 8
                    (i32.sub
                      (i32.const 64)
                      (local.get 8))))
                (call $__lshrti3
                  (i32.add
                    (local.get 5)
                    (i32.const 32))
                  (local.get 3)
                  (local.get 4)
                  (local.get 8))
                (local.set 6
                  (i64.const 0))
                (call $__multi3
                  (local.get 5)
                  (local.get 3)
                  (i64.const 0)
                  (local.tee 12
                    (i64.div_u
                      (i64.load offset=48
                        (local.get 5))
                      (i64.load offset=32
                        (local.get 5))))
                  (i64.const 0))
                (call $__multi3
                  (i32.add
                    (local.get 5)
                    (i32.const 16))
                  (local.get 4)
                  (i64.const 0)
                  (local.get 12)
                  (i64.const 0))
                (local.set 10
                  (i64.load
                    (local.get 5)))
                (block ;; label = @7
                  (br_if 0 (;@7;)
                    (i64.ne
                      (i64.add
                        (i64.load offset=24
                          (local.get 5))
                        (i64.extend_i32_u
                          (i64.lt_u
                            (local.tee 11
                              (i64.add
                                (local.tee 13
                                  (i64.load offset=8
                                    (local.get 5)))
                                (i64.load offset=16
                                  (local.get 5))))
                            (local.get 13))))
                      (i64.const 0)))
                  (br_if 2 (;@5;)
                    (i32.eqz
                      (select
                        (local.tee 8
                          (i64.lt_u
                            (local.get 1)
                            (local.get 10)))
                        (i64.lt_u
                          (local.get 2)
                          (local.get 11))
                        (i64.eq
                          (local.get 2)
                          (local.get 11))))))
                (local.set 2
                  (i64.sub
                    (i64.sub
                      (i64.add
                        (i64.add
                          (local.get 4)
                          (local.get 2))
                        (i64.extend_i32_u
                          (i64.lt_u
                            (local.tee 1
                              (i64.add
                                (local.get 3)
                                (local.get 1)))
                            (local.get 3))))
                      (local.get 11))
                    (i64.extend_i32_u
                      (i64.lt_u
                        (local.get 1)
                        (local.get 10)))))
                (local.set 12
                  (i64.add
                    (local.get 12)
                    (i64.const -1)))
                (local.set 1
                  (i64.sub
                    (local.get 1)
                    (local.get 10)))
                (br 5 (;@1;)))
              (block ;; label = @6
                (block ;; label = @7
                  (loop ;; label = @8
                    (call $__lshrti3
                      (i32.add
                        (local.get 5)
                        (i32.const 144))
                      (local.get 1)
                      (local.get 2)
                      (local.tee 8
                        (i32.sub
                          (i32.const 64)
                          (local.get 8))))
                    (local.set 12
                      (i64.load offset=144
                        (local.get 5)))
                    (block ;; label = @9
                      (br_if 0 (;@9;)
                        (i32.ge_u
                          (local.get 8)
                          (local.get 9)))
                      (call $__lshrti3
                        (i32.add
                          (local.get 5)
                          (i32.const 80))
                        (local.get 3)
                        (local.get 4)
                        (local.get 8))
                      (call $__multi3
                        (i32.add
                          (local.get 5)
                          (i32.const 64))
                        (local.get 3)
                        (local.get 4)
                        (local.tee 13
                          (i64.div_u
                            (local.get 12)
                            (i64.load offset=80
                              (local.get 5))))
                        (i64.const 0))
                      (block ;; label = @10
                        (br_if 0 (;@10;)
                          (select
                            (local.tee 8
                              (i64.lt_u
                                (local.get 1)
                                (local.tee 10
                                  (i64.load offset=64
                                    (local.get 5)))))
                            (i64.lt_u
                              (local.get 2)
                              (local.tee 12
                                (i64.load offset=72
                                  (local.get 5))))
                            (i64.eq
                              (local.get 2)
                              (local.get 12))))
                        (local.set 2
                          (i64.sub
                            (i64.sub
                              (local.get 2)
                              (local.get 12))
                            (i64.extend_i32_u
                              (local.get 8))))
                        (local.set 1
                          (i64.sub
                            (local.get 1)
                            (local.get 10)))
                        (local.set 6
                          (i64.add
                            (local.get 6)
                            (i64.extend_i32_u
                              (i64.lt_u
                                (local.tee 12
                                  (i64.add
                                    (local.get 11)
                                    (local.get 13)))
                                (local.get 11)))))
                        (br 9 (;@1;)))
                      (local.set 2
                        (i64.sub
                          (i64.sub
                            (i64.add
                              (i64.add
                                (local.get 2)
                                (local.get 4))
                              (i64.extend_i32_u
                                (i64.lt_u
                                  (local.tee 4
                                    (i64.add
                                      (local.get 1)
                                      (local.get 3)))
                                  (local.get 1))))
                            (local.get 12))
                          (i64.extend_i32_u
                            (i64.lt_u
                              (local.get 4)
                              (local.get 10)))))
                      (local.set 1
                        (i64.sub
                          (local.get 4)
                          (local.get 10)))
                      (local.set 6
                        (i64.add
                          (local.get 6)
                          (i64.extend_i32_u
                            (i64.lt_u
                              (local.tee 12
                                (i64.add
                                  (i64.add
                                    (local.get 13)
                                    (local.get 11))
                                  (i64.const -1)))
                              (local.get 11)))))
                      (br 8 (;@1;)))
                    (call $__ashlti3
                      (i32.add
                        (local.get 5)
                        (i32.const 128))
                      (local.tee 12
                        (i64.div_u
                          (local.get 12)
                          (local.get 10)))
                      (i64.const 0)
                      (local.tee 8
                        (i32.sub
                          (local.get 8)
                          (local.get 9))))
                    (call $__multi3
                      (i32.add
                        (local.get 5)
                        (i32.const 112))
                      (local.get 3)
                      (local.get 4)
                      (local.get 12)
                      (i64.const 0))
                    (call $__ashlti3
                      (i32.add
                        (local.get 5)
                        (i32.const 96))
                      (i64.load offset=112
                        (local.get 5))
                      (i64.load offset=120
                        (local.get 5))
                      (local.get 8))
                    (local.set 6
                      (i64.add
                        (i64.add
                          (i64.load offset=136
                            (local.get 5))
                          (local.get 6))
                        (i64.extend_i32_u
                          (i64.lt_u
                            (local.tee 11
                              (i64.add
                                (local.tee 6
                                  (i64.load offset=128
                                    (local.get 5)))
                                (local.get 11)))
                            (local.get 6)))))
                    (block ;; label = @9
                      (br_if 0 (;@9;)
                        (i32.le_u
                          (local.get 7)
                          (local.tee 8
                            (i32.wrap_i64
                              (select
                                (i64.clz
                                  (local.tee 2
                                    (i64.sub
                                      (i64.sub
                                        (local.get 2)
                                        (i64.load offset=104
                                          (local.get 5)))
                                      (i64.extend_i32_u
                                        (i64.lt_u
                                          (local.get 1)
                                          (local.tee 12
                                            (i64.load offset=96
                                              (local.get 5))))))))
                                (i64.add
                                  (i64.clz
                                    (local.tee 1
                                      (i64.sub
                                        (local.get 1)
                                        (local.get 12))))
                                  (i64.const 64))
                                (i64.ne
                                  (local.get 2)
                                  (i64.const 0)))))))
                      (br_if 2 (;@7;)
                        (i32.gt_u
                          (local.get 8)
                          (i32.const 63)))
                      (br 1 (;@8;))))
                  (br_if 1 (;@6;)
                    (i32.eqz
                      (select
                        (local.tee 8
                          (i64.lt_u
                            (local.get 1)
                            (local.get 3)))
                        (i64.lt_u
                          (local.get 2)
                          (local.get 4))
                        (i64.eq
                          (local.get 2)
                          (local.get 4)))))
                  (local.set 12
                    (local.get 11))
                  (br 6 (;@1;)))
                (local.set 1
                  (i64.sub
                    (local.get 1)
                    (i64.mul
                      (local.tee 2
                        (i64.div_u
                          (local.get 1)
                          (local.get 3)))
                      (local.get 3))))
                (local.set 6
                  (i64.add
                    (local.get 6)
                    (i64.extend_i32_u
                      (i64.lt_u
                        (local.tee 12
                          (i64.add
                            (local.get 11)
                            (local.get 2)))
                        (local.get 11)))))
                (local.set 2
                  (i64.const 0))
                (br 5 (;@1;)))
              (local.set 2
                (i64.sub
                  (i64.sub
                    (local.get 2)
                    (local.get 4))
                  (i64.extend_i32_u
                    (local.get 8))))
              (local.set 1
                (i64.sub
                  (local.get 1)
                  (local.get 3)))
              (local.set 6
                (i64.add
                  (local.get 6)
                  (i64.extend_i32_u
                    (i64.eqz
                      (local.tee 12
                        (i64.add
                          (local.get 11)
                          (i64.const 1)))))))
              (br 4 (;@1;)))
            (local.set 2
              (i64.sub
                (i64.sub
                  (local.get 2)
                  (local.get 11))
                (i64.extend_i32_u
                  (local.get 8))))
            (local.set 1
              (i64.sub
                (local.get 1)
                (local.get 10)))
            (local.set 6
              (i64.const 0))
            (br 3 (;@1;)))
          (local.set 2
            (i64.sub
              (i64.sub
                (local.get 2)
                (select
                  (local.get 4)
                  (i64.const 0)
                  (local.tee 8
                    (select
                      (i64.ge_u
                        (local.get 1)
                        (local.get 3))
                      (i64.ge_u
                        (local.get 2)
                        (local.get 4))
                      (i64.eq
                        (local.get 2)
                        (local.get 4))))))
              (i64.extend_i32_u
                (i64.lt_u
                  (local.get 1)
                  (local.tee 4
                    (select
                      (local.get 3)
                      (i64.const 0)
                      (local.get 8)))))))
          (local.set 1
            (i64.sub
              (local.get 1)
              (local.get 4)))
          (local.set 12
            (i64.extend_i32_u
              (local.get 8)))
          (br 2 (;@1;)))
        (local.set 1
          (i64.sub
            (local.get 1)
            (i64.mul
              (local.tee 12
                (i64.div_u
                  (local.get 1)
                  (local.get 3)))
              (local.get 3))))
        (local.set 6
          (i64.const 0))
        (local.set 2
          (i64.const 0))
        (br 1 (;@1;)))
      (local.set 12
        (i64.or
          (i64.shl
            (local.tee 2
              (i64.div_u
                (i64.or
                  (i64.shl
                    (i64.sub
                      (local.get 2)
                      (i64.mul
                        (local.tee 6
                          (i64.div_u
                            (local.get 2)
                            (local.tee 4
                              (i64.and
                                (local.get 3)
                                (i64.const 4294967295)))))
                        (local.get 3)))
                    (i64.const 32))
                  (local.tee 12
                    (i64.shr_u
                      (local.get 1)
                      (i64.const 32))))
                (local.get 4)))
            (i64.const 32))
          (local.tee 3
            (i64.div_u
              (local.tee 1
                (i64.or
                  (i64.shl
                    (i64.sub
                      (local.get 12)
                      (i64.mul
                        (local.get 2)
                        (local.get 3)))
                    (i64.const 32))
                  (i64.and
                    (local.get 1)
                    (i64.const 4294967295))))
              (local.get 4)))))
      (local.set 1
        (i64.sub
          (local.get 1)
          (i64.mul
            (local.get 3)
            (local.get 4))))
      (local.set 6
        (i64.or
          (i64.shr_u
            (local.get 2)
            (i64.const 32))
          (local.get 6)))
      (local.set 2
        (i64.const 0)))
    (i64.store offset=16
      (local.get 0)
      (local.get 1))
    (i64.store
      (local.get 0)
      (local.get 12))
    (i64.store offset=24
      (local.get 0)
      (local.get 2))
    (i64.store offset=8
      (local.get 0)
      (local.get 6))
    (global.set $__stack_pointer
      (i32.add
        (local.get 5)
        (i32.const 176)))
  )
  (func $__udivti3 (;75;) (type 15) (param i32 i64 i64 i64 i64)
    (local i32)
    (global.set $__stack_pointer
      (local.tee 5
        (i32.sub
          (global.get $__stack_pointer)
          (i32.const 32))))
    (call $_ZN17compiler_builtins3int19specialized_div_rem12u128_div_rem17h8d9ba2662c058edeE
      (local.get 5)
      (local.get 1)
      (local.get 2)
      (local.get 3)
      (local.get 4))
    (local.set 4
      (i64.load
        (local.get 5)))
    (i64.store offset=8
      (local.get 0)
      (i64.load offset=8
        (local.get 5)))
    (i64.store
      (local.get 0)
      (local.get 4))
    (global.set $__stack_pointer
      (i32.add
        (local.get 5)
        (i32.const 32)))
  )
  (func $__lshrti3 (;76;) (type 14) (param i32 i64 i64 i32)
    (local i64)
    (block ;; label = @1
      (block ;; label = @2
        (br_if 0 (;@2;)
          (i32.and
            (local.get 3)
            (i32.const 64)))
        (br_if 1 (;@1;)
          (i32.eqz
            (local.get 3)))
        (local.set 1
          (i64.or
            (i64.shl
              (local.get 2)
              (i64.extend_i32_u
                (i32.and
                  (i32.sub
                    (i32.const 0)
                    (local.get 3))
                  (i32.const 63))))
            (i64.shr_u
              (local.get 1)
              (local.tee 4
                (i64.extend_i32_u
                  (i32.and
                    (local.get 3)
                    (i32.const 63)))))))
        (local.set 2
          (i64.shr_u
            (local.get 2)
            (local.get 4)))
        (br 1 (;@1;)))
      (local.set 1
        (i64.shr_u
          (local.get 2)
          (i64.extend_i32_u
            (i32.and
              (local.get 3)
              (i32.const 63)))))
      (local.set 2
        (i64.const 0)))
    (i64.store
      (local.get 0)
      (local.get 1))
    (i64.store offset=8
      (local.get 0)
      (local.get 2))
  )
  (func $__umodti3 (;77;) (type 15) (param i32 i64 i64 i64 i64)
    (local i32)
    (global.set $__stack_pointer
      (local.tee 5
        (i32.sub
          (global.get $__stack_pointer)
          (i32.const 32))))
    (call $_ZN17compiler_builtins3int19specialized_div_rem12u128_div_rem17h8d9ba2662c058edeE
      (local.get 5)
      (local.get 1)
      (local.get 2)
      (local.get 3)
      (local.get 4))
    (local.set 4
      (i64.load offset=16
        (local.get 5)))
    (i64.store offset=8
      (local.get 0)
      (i64.load offset=24
        (local.get 5)))
    (i64.store
      (local.get 0)
      (local.get 4))
    (global.set $__stack_pointer
      (i32.add
        (local.get 5)
        (i32.const 32)))
  )
  (data $.rodata (;0;) (i32.const 1048576) "\00\00\80?\00\00\c0?\00\00\00\00\dc\cf\d15\00\00\00\00\00\c0\15?8c\ed>\da\0fI?^\98{?\da\0f\c9?i7\ac1h!\223\b4\0f\143h!\a23]=\7ff\9e\a0\e6?\00\00\00\00\00\889=D\17u\faR\b0\e6?\00\00\00\00\00\00\d8<\fe\d9\0bu\12\c0\e6?\00\00\00\00\00x(\bd\bfv\d4\dd\dc\cf\e6?\00\00\00\00\00\c0\1e=)\1ae<\b2\df\e6?\00\00\00\00\00\00\d8\bc\e3:Y\98\92\ef\e6?\00\00\00\00\00\00\bc\bc\86\93Q\f9}\ff\e6?\00\00\00\00\00\d8/\bd\a3-\f4ft\0f\e7?\00\00\00\00\00\88,\bd\c3_\ec\e8u\1f\e7?\00\00\00\00\00\c0\13=\05\cf\ea\86\82/\e7?\00\00\00\00\0008\bdR\81\a5H\9a?\e7?\00\00\00\00\00\c0\00\bd\fc\cc\d75\bdO\e7?\00\00\00\00\00\88/=\f1gBV\eb_\e7?\00\00\00\00\00\e0\03=Hm\ab\b1$p\e7?\00\00\00\00\00\d0'\bd8]\deOi\80\e7?\00\00\00\00\00\00\dd\bc\00\1d\ac8\b9\90\e7?\00\00\00\00\00\00\e3<x\01\ebs\14\a1\e7?\00\00\00\00\00\00\ed\bc`\d0v\09{\b1\e7?\00\00\00\00\00@ =3\c10\01\ed\c1\e7?\00\00\00\00\00\00\a0<6\86\ffbj\d2\e7?\00\00\00\00\00\90&\bd;N\cf6\f3\e2\e7?\00\00\00\00\00\e0\02\bd\e8\c3\91\84\87\f3\e7?\00\00\00\00\00X$\bdN\1b>T'\04\e8?\00\00\00\00\00\003=\1a\07\d1\ad\d2\14\e8?\00\00\00\00\00\00\0f=~\cdL\99\89%\e8?\00\00\00\00\00\c0!\bd\d0B\b9\1eL6\e8?\00\00\00\00\00\d0)=\b5\ca#F\1aG\e8?\00\00\00\00\00\10G=\bc[\9f\17\f4W\e8?\00\00\00\00\00`\22=\af\91D\9b\d9h\e8?\00\00\00\00\00\c42\bd\95\a31\d9\cay\e8?\00\00\00\00\00\00#\bd\b8e\8a\d9\c7\8a\e8?\00\00\00\00\00\80*\bd\00Xx\a4\d0\9b\e8?\00\00\00\00\00\00\ed\bc#\a2*B\e5\ac\e8?\00\00\00\00\00(3=\fa\19\d6\ba\05\be\e8?\00\00\00\00\00\b4B=\83C\b5\162\cf\e8?\00\00\00\00\00\d0.\bdLf\08^j\e0\e8?\00\00\00\00\00P \bd\07x\15\99\ae\f1\e8?\00\00\00\00\00((=\0e,(\d0\fe\02\e9?\00\00\00\00\00\b0\1c\bd\96\ff\91\0b[\14\e9?\00\00\00\00\00\e0\05\bd\f9/\aaS\c3%\e9?\00\00\00\00\00@\f5<J\c6\cd\b077\e9?\00\00\00\00\00 \17=\ae\98_+\b8H\e9?\00\00\00\00\00\00\09\bd\cbR\c8\cbDZ\e9?\00\00\00\00\00h%=!ov\9a\ddk\e9?\00\00\00\00\00\d06\bd*N\de\9f\82}\e9?\00\00\00\00\00\00\01\bd\a3#z\e43\8f\e9?\00\00\00\00\00\00-=\04\06\cap\f1\a0\e9?\00\00\00\00\00\a48\bd\89\ffSM\bb\b2\e9?\00\00\00\00\00\5c5=[\f1\a3\82\91\c4\e9?\00\00\00\00\00\b8&=\c5\b8K\19t\d6\e9?\00\00\00\00\00\00\ec\bc\8e#\e3\19c\e8\e9?\00\00\00\00\00\d0\17=\02\f3\07\8d^\fa\e9?\00\00\00\00\00@\16=M\e5]{f\0c\ea?\00\00\00\00\00\00\f5\bc\f6\b8\8e\edz\1e\ea?\00\00\00\00\00\e0\09='.J\ec\9b0\ea?\00\00\00\00\00\d8*=]\0aF\80\c9B\ea?\00\00\00\00\00\f0\1a\bd\9b%>\b2\03U\ea?\00\00\00\00\00`\0b=\13b\f4\8aJg\ea?\00\00\00\00\00\888=\a7\b30\13\9ey\ea?\00\00\00\00\00 \11=\8d.\c1S\fe\8b\ea?\00\00\00\00\00\c0\06=\d2\fcyUk\9e\ea?\00\00\00\00\00\b8)\bd\b8o5!\e5\b0\ea?\00\00\00\00\00p+=\81\f3\d3\bfk\c3\ea?\00\00\00\00\00\00\d9<\80'<:\ff\d5\ea?\00\00\00\00\00\00\e4<\a3\d2Z\99\9f\e8\ea?\00\00\00\00\00\90,\bdg\f3\22\e6L\fb\ea?\00\00\00\00\00P\16=\90\b7\8d)\07\0e\eb?\00\00\00\00\00\d4/=\a9\89\9al\ce \eb?\00\00\00\00\00p\12=K\1aO\b8\a23\eb?\00\00\00\00\00GM=\e7G\b7\15\84F\eb?\00\00\00\00\0088\bd:Y\e5\8drY\eb?\00\00\00\00\00\00\98<j\c5\f1)nl\eb?\00\00\00\00\00\d0\0a=P^\fb\f2v\7f\eb?\00\00\00\00\00\80\de<\b2I'\f2\8c\92\eb?\00\00\00\00\00\c0\04\bd\03\06\a10\b0\a5\eb?\00\00\00\00\00p\0d\bdfo\9a\b7\e0\b8\eb?\00\00\00\00\00\90\0d=\ff\c1K\90\1e\cc\eb?\00\00\00\00\00\a0\02=o\a1\f3\c3i\df\eb?\00\00\00\00\00x\1f\bd\b8\1d\d7[\c2\f2\eb?\00\00\00\00\00\a0\10\bd\e9\b2Aa(\06\ec?\00\00\00\00\00@\11\bd\e0R\85\dd\9b\19\ec?\00\00\00\00\00\e0\0b=\eed\fa\d9\1c-\ec?\00\00\00\00\00@\09\bd/\d0\ff_\ab@\ec?\00\00\00\00\00\d0\0e\bd\15\fd\faxGT\ec?\00\00\00\00\00f9=\cb\d0W.\f1g\ec?\00\00\00\00\00\10\1a\bd\b6\c1\88\89\a8{\ec?\00\00\00\00\80EX\bd3\e7\06\94m\8f\ec?\00\00\00\00\00H\1a\bd\df\c4QW@\a3\ec?\00\00\00\00\00\00\cb<\94\90\ef\dc \b7\ec?\00\00\00\00\00@\01=\89\16m.\0f\cb\ec?\00\00\00\00\00 \f0<\12\c4]U\0b\df\ec?\00\00\00\00\00`\f3<;\ab[[\15\f3\ec?\00\00\00\00\00\90\06\bd\bc\89\07J-\07\ed?\00\00\00\00\00\a0\09=\fa\c8\08+S\1b\ed?\00\00\00\00\00\e0\15\bd\85\8a\0d\08\87/\ed?\00\00\00\00\00(\1d=\03\a2\ca\ea\c8C\ed?\00\00\00\00\00\a0\01=\91\a4\fb\dc\18X\ed?\00\00\00\00\00\00\df<\a1\e6b\e8vl\ed?\00\00\00\00\00\a0\03\bdN\83\c9\16\e3\80\ed?\00\00\00\00\00\d8\0c\bd\90`\ffq]\95\ed?\00\00\00\00\00\c0\f4<\ae2\db\03\e6\a9\ed?\00\00\00\00\00\90\ff<%\83:\d6|\be\ed?\00\00\00\00\00\80\e9<E\b4\01\f3!\d3\ed?\00\00\00\00\00 \f5\bc\bf\05\1cd\d5\e7\ed?\00\00\00\00\00p\1d\bd\ec\9a{3\97\fc\ed?\00\00\00\00\00\14\16\bd^}\19kg\11\ee?\00\00\00\00\00H\0b=\e7\a3\f5\14F&\ee?\00\00\00\00\00\ce@=\5c\ee\16;3;\ee?\00\00\00\00\00h\0c=\b4?\8b\e7.P\ee?\00\00\00\00\000\09\bdhmg$9e\ee?\00\00\00\00\00\00\e5\bcDL\c7\fbQz\ee?\00\00\00\00\00\f8\07\bd&\b7\cdwy\8f\ee?\00\00\00\00\00p\f3\bc\e8\90\a4\a2\af\a4\ee?\00\00\00\00\00\d0\e5<\e4\ca|\86\f4\b9\ee?\00\00\00\00\00\1a\16=\0dh\8e-H\cf\ee?\00\00\00\00\00P\f5<\14\85\18\a2\aa\e4\ee?\00\00\00\00\00@\c6<\13Za\ee\1b\fa\ee?\00\00\00\00\00\80\ee\bc\06A\b6\1c\9c\0f\ef?\00\00\00\00\00\88\fa\bcc\b9k7+%\ef?\00\00\00\00\00\90,\bdur\ddH\c9:\ef?\00\00\00\00\00\00\aa<$En[vP\ef?\00\00\00\00\00\f0\f4\bc\fdD\88y2f\ef?\00\00\00\00\00\80\ca<8\be\9c\ad\fd{\ef?\00\00\00\00\00\bc\fa<\82<$\02\d8\91\ef?\00\00\00\00\00`\d4\bc\8e\90\9e\81\c1\a7\ef?\00\00\00\00\00\0c\0b\bd\11\d5\926\ba\bd\ef?\00\00\00\00\00\e0\c0\bc\94q\8f+\c2\d3\ef?\00\00\00\00\80\de\10\bd\ee#*k\d9\e9\ef?\00\00\00\00\00C\ee<\00\00\00\00\00\00\f0?\00\00\00\00\00\00\00\00\be\bcZ\fa\1a\0b\f0?\00\00\00\00\00@\b3\bc\033\fb\a9=\16\f0?\00\00\00\00\00\17\12\bd\82\02;\14h!\f0?\00\00\00\00\00@\ba<l\80w>\9a,\f0?\00\00\00\00\00\98\ef<\ca\bb\11.\d47\f0?\00\00\00\00\00@\c7\bc\89\7fn\e8\15C\f0?\00\00\00\00\000\d8<gT\f6r_N\f0?\00\00\00\00\00?\1a\bdZ\85\15\d3\b0Y\f0?\00\00\00\00\00\84\02\bd\95\1f<\0e\0ae\f0?\00\00\00\00\00`\f1<\1a\f7\dd)kp\f0?\00\00\00\00\00$\15=-\a8r+\d4{\f0?\00\00\00\00\00\a0\e9\bc\d0\9bu\18E\87\f0?\00\00\00\00\00@\e6<\c8\07f\f6\bd\92\f0?\00\00\00\00\00x\00\bd\83\f3\c6\ca>\9e\f0?\00\00\00\00\00\00\98\bc09\1f\9b\c7\a9\f0?\00\00\00\00\00\a0\ff<\fc\88\f9lX\b5\f0?\00\00\00\00\00\c8\fa\bc\8al\e4E\f1\c0\f0?\00\00\00\00\00\c0\d9<\16Hr+\92\cc\f0?\00\00\00\00\00 \05=\d8]9#;\d8\f0?\00\00\00\00\00\d0\fa\bc\f3\d1\d32\ec\e3\f0?\00\00\00\00\00\ac\1b=\a6\a9\df_\a5\ef\f0?\00\00\00\00\00\e8\04\bd\f0\d2\fe\aff\fb\f0?\00\00\00\00\000\0d\bdK#\d7(0\07\f1?\00\00\00\00\00P\f1<[[\12\d0\01\13\f1?\00\00\00\00\00\00\ec<\f9*^\ab\db\1e\f1?\00\00\00\00\00\bc\16=\d51l\c0\bd*\f1?\00\00\00\00\00@\e8<}\04\f2\14\a86\f1?\00\00\00\00\00\d0\0e\bd\e9-\a9\ae\9aB\f1?\00\00\00\00\00\e0\e8<81O\93\95N\f1?\00\00\00\00\00@\eb<q\8e\a5\c8\98Z\f1?\00\00\00\00\000\05=\df\c3qT\a4f\f1?\00\00\00\00\008\03=\11R}<\b8r\f1?\00\00\00\00\00\d4(=\9f\bb\95\86\d4~\f1?\00\00\00\00\00\d0\05\bd\93\8d\8c8\f9\8a\f1?\00\00\00\00\00\88\1c\bdf]7X&\97\f1?\00\00\00\00\00\f0\11=\a7\cbo\eb[\a3\f1?\00\00\00\00\00H\10=\e3\87\13\f8\99\af\f1?\00\00\00\00\009G\bdT]\04\84\e0\bb\f1?\00\00\00\00\00\e4$=C\1c(\95/\c8\f1?\00\00\00\00\00 \0a\bd\b2\b9h1\87\d4\f1?\00\00\00\00\00\80\e3<1@\b4^\e7\e0\f1?\00\00\00\00\00\c0\ea<8\d9\fc\22P\ed\f1?\00\00\00\00\00\90\01=\f7\cd8\84\c1\f9\f1?\00\00\00\00\00x\1b\bd\8f\8db\88;\06\f2?\00\00\00\00\00\94-=\1e\a8x5\be\12\f2?\00\00\00\00\00\00\d8<A\dd}\91I\1f\f2?\00\00\00\00\004+=#\13y\a2\dd+\f2?\00\00\00\00\00\f8\19=\e7aunz8\f2?\00\00\00\00\00\c8\19\bd'\14\82\fb\1fE\f2?\00\00\00\00\000\02=\02\a6\b2O\ceQ\f2?\00\00\00\00\00H\13\bd\b0\ce\1eq\85^\f2?\00\00\00\00\00p\12=\16}\e2eEk\f2?\00\00\00\00\00\d0\11=\0f\e0\1d4\0ex\f2?\00\00\00\00\00\ee1=>c\f5\e1\df\84\f2?\00\00\00\00\00\c0\14\bd0\bb\91u\ba\91\f2?\00\00\00\00\00\d8\13\bd\09\df\1f\f5\9d\9e\f2?\00\00\00\00\00\b0\08=\9b\0e\d1f\8a\ab\f2?\00\00\00\00\00|\22\bd:\da\da\d0\7f\b8\f2?\00\00\00\00\004*=\f9\1aw9~\c5\f2?\00\00\00\00\00\80\10\bd\d9\02\e4\a6\85\d2\f2?\00\00\00\00\00\d0\0e\bdy\15d\1f\96\df\f2?\00\00\00\00\00 \f4\bc\cf.>\a9\af\ec\f2?\00\00\00\00\00\98$\bd\22\88\bdJ\d2\f9\f2?\00\00\00\00\000\16\bd%\b61\0a\fe\06\f3?\00\00\00\00\0062\bd\0b\a5\ee\ed2\14\f3?\00\00\00\00\80\dfp\bd\b8\d7L\fcp!\f3?\00\00\00\00\00H\22\bd\a2\e9\a8;\b8.\f3?\00\00\00\00\00\98%\bdf\17d\b2\08<\f3?\00\00\00\00\00\d0\1e='\fa\e3fbI\f3?\00\00\00\00\00\00\dc\bc\0f\9f\92_\c5V\f3?\00\00\00\00\00\d80\bd\b9\88\de\a21d\f3?\00\00\00\00\00\c8\22=9\aa:7\a7q\f3?\00\00\00\00\00` =\fet\1e#&\7f\f3?\00\00\00\00\00`\16\bd8\d8\05m\ae\8c\f3?\00\00\00\00\00\e0\0a\bd\c3>q\1b@\9a\f3?\00\00\00\00\00rD\bd \a0\e54\db\a7\f3?\00\00\00\00\00 \08=\95n\ec\bf\7f\b5\f3?\00\00\00\00\00\80>=\f2\a8\13\c3-\c3\f3?\00\00\00\00\00\80\ef<\22\e1\edD\e5\d0\f3?\00\00\00\00\00\a0\17\bd\bb4\12L\a6\de\f3?\00\00\00\00\000&=\ccN\1c\dfp\ec\f3?\00\00\00\00\00\a6H\bd\8c~\ac\04E\fa\f3?\00\00\00\00\00\dc<\bd\bb\a0g\c3\22\08\f4?\00\00\00\00\00\b8%=\95.\f7!\0a\16\f4?\00\00\00\00\00\c0\1e=FF\09'\fb#\f4?\00\00\00\00\00`\13\bd \a9P\d9\f51\f4?\00\00\00\00\00\98#=\eb\b9\84?\fa?\f4?\00\00\00\00\00\00\fa<\19\89a`\08N\f4?\00\00\00\00\00\c0\f6\bc\01\d2\a7B \5c\f4?\00\00\00\00\00\c0\0b\bd\16\00\1d\edAj\f4?\00\00\00\00\00\80\12\bd&3\8bfmx\f4?\00\00\00\00\00\e00=\00<\c1\b5\a2\86\f4?\00\00\00\00\00@-\bd\04\af\92\e1\e1\94\f4?\00\00\00\00\00 \0c=r\d3\d7\f0*\a3\f4?\00\00\00\00\00P\1e\bd\01\b8m\ea}\b1\f4?\00\00\00\00\00\80\07=\e1)6\d5\da\bf\f4?\00\00\00\00\00\80\13\bd2\c1\17\b8A\ce\f4?\00\00\00\00\00\80\00=\db\dd\fd\99\b2\dc\f4?\00\00\00\00\00p,=\96\ab\d8\81-\eb\f4?\00\00\00\00\00\e0\1c\bd\02-\9dv\b2\f9\f4?\00\00\00\00\00 \19=\c11E\7fA\08\f5?\00\00\00\00\00\c0\08\bd*f\cf\a2\da\16\f5?\00\00\00\00\00\00\fa\bc\eaQ?\e8}%\f5?\00\00\00\00\00\08J=\daN\9dV+4\f5?\00\00\00\00\00\d8&\bd\1a\ac\f6\f4\e2B\f5?\00\00\00\00\00D2\bd\db\94]\ca\a4Q\f5?\00\00\00\00\00<H=k\11\e9\ddp`\f5?\00\00\00\00\00\b0$=\de)\b56Go\f5?\00\00\00\00\00ZA=\0e\c4\e2\db'~\f5?\00\00\00\00\00\e0)\bdo\c7\97\d4\12\8d\f5?\00\00\00\00\00\08#\bdL\0b\ff'\08\9c\f5?\00\00\00\00\00\ecM='TH\dd\07\ab\f5?\00\00\00\00\00\00\c4\bc\f4z\a8\fb\11\ba\f5?\00\00\00\00\00\080=\0bFY\8a&\c9\f5?\00\00\00\00\00\c8&\bd?\8e\99\90E\d8\f5?\00\00\00\00\00\9aF=\e1 \ad\15o\e7\f5?\00\00\00\00\00@\1b\bd\ca\eb\dc \a3\f6\f5?\00\00\00\00\00p\17=\b8\dcv\b9\e1\05\f6?\00\00\00\00\00\f8&=\15\f7\cd\e6*\15\f6?\00\00\00\00\00\00\01=1U:\b0~$\f6?\00\00\00\00\00\d0\15\bd\b5)\19\1d\dd3\f6?\00\00\00\00\00\d0\12\bd\13\c3\cc4FC\f6?\00\00\00\00\00\80\ea\bc\fa\8e\bc\fe\b9R\f6?\00\00\00\00\00`(\bd\973U\828b\f6?\00\00\00\00\00\feq=\8e2\08\c7\c1q\f6?\00\00\00\00\00 7\bd~\a9L\d4U\81\f6?\00\00\00\00\00\80\e6<q\94\9e\b1\f4\90\f6?\00\00\00\00\00x)\bd\00\00\00?\00\00\00\bf\cd;\7ff\9e\a0\e6?\87\01\ebs\14\a1\e7?\db\a0*B\e5\ac\e8?\90\f0\a3\82\91\c4\e9?\ad\d3Z\99\9f\e8\ea?\9cR\85\dd\9b\19\ec?\87\a4\fb\dc\18X\ed?\da\90\a4\a2\af\a4\ee?\00\00\00\00\00\00\f0?\0f\89\f9lX\b5\f0?{Q}<\b8r\f1?8bunz8\f2?\15\b71\0a\fe\06\f3?\224\12L\a6\de\f3?'*6\d5\da\bf\f4?)TH\dd\07\ab\f5?\00\00\00\00\00\00\f0?\8br\8d\f9\a2(\f4?=n=\a5\fee\f9?\03\00\00\00\04\00\00\00\04\00\00\00\06\00\00\00\83\f9\a2\00DNn\00\fc)\15\00\d1W'\00\dd4\f5\00b\db\c0\00<\99\95\00A\90C\00cQ\fe\00\bb\de\ab\00\b7a\c5\00:n$\00\d2MB\00I\06\e0\00\09\ea.\00\1c\92\d1\00\eb\1d\fe\00)\b1\1c\00\e8>\a7\00\f55\82\00D\bb.\00\9c\e9\84\00\b4&p\00A~_\00\d6\919\00S\839\00\9c\f49\00\8b_\84\00(\f9\bd\00\f8\1f;\00\de\ff\97\00\0f\98\05\00\11/\ef\00\0aZ\8b\00m\1fm\00\cf~6\00\09\cb'\00FO\b7\00\9ef?\00-\ea_\00\ba'u\00\e5\eb\c7\00={\f1\00\f79\07\00\92R\8a\00\fbk\ea\00\1f\b1_\00\08]\8d\000\03V\00{\fcF\00\f0\abk\00 \bc\cf\006\f4\9a\00\e3\a9\1d\00^a\91\00\08\1b\e6\00\85\99e\00\a0\14_\00\8d@h\00\80\d8\ff\00'sM\00\06\061\00\caV\15\00\c9\a8s\00{\e2`\00k\8c\c0\00\00\00\00@\fb!\f9?\00\00\00\00-Dt>\00\00\00\80\98F\f8<\00\00\00`Q\ccx;\00\00\00\80\83\1b\f09\00\00\00@ %z8\00\00\00\80\22\82\e36\00\00\00\00\1d\f3i5Q\b4\f0\b2\96\b1D\b0\f9\ae\b6\ady\acC\ab\14\aa\eb\a8\c8\a7\aa\a6\92\a5\80\a4s\a3k\a2h\a1j\a0p\9f{\9e\8a\9d\9d\9c\b5\9b\d1\9a\f0\99\13\99:\98e\97\93\96\c4\95\f8\940\94k\93\a9\92\ea\91.\91u\90\be\8f\0a\8fY\8e\aa\8d\fe\8cT\8c\ac\8b\07\8bd\8a\c4\89%\89\89\88\ee\87V\87\c0\86+\86\99\85\08\85y\84\ec\83a\83\d8\82P\82\c9\81E\81\c2\80@\80\02\ff\0e\fd%\fbG\f9s\f7\aa\f5\ea\f34\f2\87\f0\e3\eeG\ed\b3\eb'\ea\a3\e8'\e7\b2\e5C\e4\dc\e2z\e1 \e0\cb\de}\dd4\dc\f1\da\b3\d9{\d8H\d7\1a\d6\f1\d4\cd\d3\ad\d2\92\d1{\d0i\cf[\ceQ\cdJ\ccH\cbJ\caO\c9X\c8d\c7t\c6\87\c5\9d\c4\b7\c3\d4\c2\f4\c1\16\c1<\c0e\bf\90\be\be\bd\ef\bc#\bcY\bb\91\ba\cc\b9\0a\b9J\b8\8c\b7\d0\b6\17\b6`\b5\00\00\00\00\00\00\f0?\00\00\00\00\00\00\f8?\00\00\00\00\00\00\00\00\06\d0\cfC\eb\fdL>\00\00\00\00\00\00\00\00\00\00\00@\03\b8\e2?O\bba\05g\ac\dd?\18-DT\fb!\e9?\9b\f6\81\d2\0bs\ef?\18-DT\fb!\f9?\e2e/\22\7f+z<\07\5c\143&\a6\81<\bd\cb\f0z\88\07p<\07\5c\143&\a6\91<\00\00\00\00\00\00\e0?\00\00\00\00\00\00\e0\bf\18-DT\fb!\e9?\18-DT\fb!\e9\bf\d2!3\7f|\d9\02@\d2!3\7f|\d9\02\c0\00\00\00\00\00\00\00\00\00\00\00\00\00\00\00\80\18-DT\fb!\09@\18-DT\fb!\09\c0\db\0fI?\db\0fI\bf\e4\cb\16@\e4\cb\16\c0\00\00\00\00\00\00\00\80\db\0fI@\db\0fI\c0")
  (@producers
    (language "Rust" "")
    (processed-by "rustc" "1.94.1 (e408947bf 2026-03-25)")
  )
  (@custom "target_features" (after data) "\08+\0bbulk-memory+\0fbulk-memory-opt+\16call-indirect-overlong+\0amultivalue+\0fmutable-globals+\13nontrapping-fptoint+\0freference-types+\08sign-ext")
)
