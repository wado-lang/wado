;; A component whose guest effect (`test:lvl/tag`) takes a type `use`d from a
;; types-only interface it also imports (`test:lvl/types`), alongside the WASI
;; types-only `wasi:clocks/types`, as wit-component emits them.
(component
  (import "wasi:clocks/types@0.3.0" (instance
    (type $duration u64)
    (export "duration" (type (eq $duration)))))
  (import "test:lvl/types@0.1.0" (instance $types
    (type $level (enum "low" "high"))
    (export "level" (type (eq $level)))
    (type $count u32)
    (export "count" (type (eq $count)))
    (type $pair (record (field "a" u32) (field "b" u32)))
    (export "pair" (type (eq $pair)))))
  (alias export $types "level" (type $level_t))
  (import "test:lvl/tag@0.1.0" (instance $tag
    (alias outer 1 $level_t (type $lv))
    (export "level" (type (eq $lv)))
    (export "tag" (func (param "l" 1) (result u32)))))
  (alias export $tag "tag" (func $tag_fn))
  (core func $tag_lowered (canon lower (func $tag_fn)))
  (core instance $imports (export "tag" (func $tag_lowered)))
  (core module $m
    (import "host" "tag" (func $tag (param i32) (result i32)))
    ;; tag(high) + 1
    (func (export "stamp") (result i32)
      i32.const 1
      call $tag
      i32.const 1
      i32.add)
  )
  (core instance $i (instantiate $m (with "host" (instance $imports))))
  (func $stamp (result u32) (canon lift (core func $i "stamp")))
  (instance $lvl (export "stamp" (func $stamp)))
  (export "test:lvl/lvl@0.1.0" (instance $lvl))
)
