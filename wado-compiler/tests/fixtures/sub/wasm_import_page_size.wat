;; Wat module whose memory uses the custom-page-sizes proposal. An embedded
;; asset shares the component's memory, which has the default 64 KiB page,
;; so Phase 1 rejects this shape with a diagnostic instead of emitting a
;; component that fails validation.
(module
  (memory 1 1 (pagesize 1))
  (func (export "get") (result i32)
    i32.const 7)
)
