;; Wat module whose memory carries a maximum and spells out the default page
;; size. Both belong to the memory it defines; the embedding rewrites them into
;; a plain request for the component's memory, which has neither.
(module
  (memory 1 1 (pagesize 65536))
  (func (export "peek") (result i32)
    i32.const 0
    i32.load)
  (func (export "poke") (param i32)
    i32.const 0
    local.get 0
    i32.store)
)
