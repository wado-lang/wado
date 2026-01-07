(module
  ;; WASI imports
  (import "wasi_snapshot_preview1" "fd_write"
    (func $fd_write (param i32 i32 i32 i32) (result i32)))
  
  ;; Memory (1 page = 64KB)
  (memory (export "memory") 1)
  
  ;; String data
  (data (i32.const 100) "{message}\\n") ;; string_0, len=11
  
  ;; function: println
  (func (export "println")
    ;; TODO: unimplemented statement
    ;; TODO: unimplemented statement
  )
  
  ;; function: eprintln
  (func (export "eprintln")
    ;; TODO: unimplemented statement
    ;; TODO: unimplemented statement
  )
  
  ;; function: print
  (func (export "print")
    ;; TODO: unimplemented statement
    ;; TODO: unimplemented statement
  )
  
  ;; function: eprint
  (func (export "eprint")
    ;; TODO: unimplemented statement
    ;; TODO: unimplemented statement
  )
  
  ;; function: env
  (func (export "env")
    ;; TODO: unimplemented statement
    ;; TODO: unimplemented statement
    ;; TODO: unimplemented statement
  )
  
  ;; function: args
  (func (export "args")
    ;; TODO: unimplemented statement
  )
  
  ;; function: cwd
  (func (export "cwd")
    ;; TODO: unimplemented statement
  )
  
  ;; function: exit_success
  (func (export "exit_success")
    ;; TODO: unimplemented expression
  )
  
  ;; function: exit_error
  (func (export "exit_error")
    ;; TODO: unimplemented expression
  )
  
  ;; function: exit_with_code
  (func (export "exit_with_code")
    ;; TODO: unimplemented expression
  )
  
  ;; function: message_to_stream
  (func (export "message_to_stream")
  )
)
