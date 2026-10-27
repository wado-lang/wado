;; A component whose functions read lists, a string and a record holding a list,
;; so a caller has to lower each argument into the callee's memory, flat and
;; through a parameter buffer alike, through an interface and as world functions.
(component
  (core module $m
    ;; Room for 4 MiB of arguments past the scratch address.
    (memory (export "memory") 65)
    (global $top (mut i32) (i32.const 1024))
    ;; Bump allocation, reset as each call ends: a call reads its arguments
    ;; once and is done with them before the next call lifts its own.
    (func (export "realloc") (param i32 i32 i32 i32) (result i32)
      (local $at i32)
      global.get $top
      local.get 2
      i32.const 1
      i32.sub
      i32.add
      i32.const 0
      local.get 2
      i32.sub
      i32.and
      local.tee $at
      local.get 3
      i32.add
      global.set $top
      local.get $at)
    (func $reset
      i32.const 1024
      global.set $top)
    (func $sum_bytes (param $ptr i32) (param $len i32) (result i32)
      (local $total i32)
      block $done
        loop $next
          local.get $len
          i32.eqz
          br_if $done
          local.get $total
          local.get $ptr
          i32.load8_u
          i32.add
          local.set $total
          local.get $ptr
          i32.const 1
          i32.add
          local.set $ptr
          local.get $len
          i32.const 1
          i32.sub
          local.set $len
          br $next
        end
      end
      local.get $total)
    (func (export "sum") (param i32 i32) (result i32)
      local.get 0
      local.get 1
      call $sum_bytes
      call $reset)
    (func (export "length") (param i32 i32) (result i32)
      local.get 1
      call $reset)
    ;; tuple<list<u8>, u32> flattens to (ptr, len, tag).
    (func (export "sum-blob") (param i32 i32 i32) (result i32)
      local.get 0
      local.get 1
      call $sum_bytes
      local.get 2
      i32.add
      call $reset)
    ;; Sixteen u32s and a list are past the sixteen flat parameters a lift
    ;; passes directly, so they arrive as one pointer to their tuple; the list's
    ;; (ptr, len) sits at offset 64.
    (func (export "wide") (param $at i32) (result i32)
      local.get $at
      i32.load offset=64
      local.get $at
      i32.load offset=68
      call $sum_bytes
      local.get $at
      i32.load
      i32.add
      call $reset)
    (func (export "sum-f32") (param $ptr i32) (param $len i32) (result f32)
      (local $total f32)
      block $done
        loop $next
          local.get $len
          i32.eqz
          br_if $done
          local.get $total
          local.get $ptr
          f32.load
          f32.add
          local.set $total
          local.get $ptr
          i32.const 4
          i32.add
          local.set $ptr
          local.get $len
          i32.const 1
          i32.sub
          local.set $len
          br $next
        end
      end
      local.get $total
      call $reset)
  )
  (core instance $i (instantiate $m))
  (alias core export $i "memory" (core memory $mem))
  (alias core export $i "realloc" (core func $realloc))
  (func $sum (param "data" (list u8)) (result u32)
    (canon lift (core func $i "sum") (memory $mem) (realloc $realloc)))
  (func $length (param "text" string) (result u32)
    (canon lift (core func $i "length") (memory $mem) (realloc $realloc)))
  (func $sum_blob (param "blob" (tuple (list u8) u32)) (result u32)
    (canon lift (core func $i "sum-blob") (memory $mem) (realloc $realloc)))
  (func $wide
    (param "a" u32) (param "b" u32) (param "c" u32) (param "d" u32)
    (param "e" u32) (param "f" u32) (param "g" u32) (param "h" u32)
    (param "i" u32) (param "j" u32) (param "k" u32) (param "l" u32)
    (param "m" u32) (param "n" u32) (param "o" u32) (param "p" u32)
    (param "data" (list u8))
    (result u32)
    (canon lift (core func $i "wide") (memory $mem) (realloc $realloc)))
  (func $sum_f32 (param "data" (list f32)) (result f32)
    (canon lift (core func $i "sum-f32") (memory $mem) (realloc $realloc)))
  (instance $iface
    (export "sum" (func $sum))
    (export "length" (func $length))
    (export "sum-blob" (func $sum_blob))
    (export "wide" (func $wide))
    (export "sum-f32" (func $sum_f32)))
  (export "wado:test/lists@0.1.0" (instance $iface))
  ;; The same functions as world functions, which no handler serves.
  (export "sum-bytes" (func $sum))
  (export "sum-floats" (func $sum_f32))
)
