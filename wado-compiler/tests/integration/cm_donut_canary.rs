//! Canary: wasmtime executes the Component Model's donut-wrapping callback
//! shape (Linking.md "Higher-order Shared-Nothing Linking"). Through 48 it
//! trapped every adapter between ancestor-related instances; 49 runs them. See
//! `docs/research-cm-boundary-callbacks.md`.
//!
//! The component below is a valid donut: the parent satisfies the nested
//! child's `cb` import with a lift of its own core trampoline
//! (`shim → lift → child → lower → main`, fixup fills the funcref table), and
//! `go(x)` = child's `run(x)` = parent's `impl(x) + 1`. It validates and
//! instantiates, and `go(5)` = 51.
//!
//! When this test FAILS, wasmtime traps donut adapters again: revisit the
//! research note before building on them.

const DONUT_WAT: &str = r#"
(component
  ;; Trampoline module: forwards to whatever the fixup module puts in slot 0.
  (core module $Shim
    (table (export "tbl") 1 1 funcref)
    (type $ft (func (param i32) (result i32)))
    (func (export "tramp") (param i32) (result i32)
      (call_indirect (type $ft) (local.get 0) (i32.const 0))
    )
  )
  (core instance $shim (instantiate $Shim))
  (func $tramp (param "x" u32) (result u32) (canon lift (core func $shim "tramp")))

  ;; Child: imports the callback, exports run(x) = cb(x) + 1.
  (component $Child
    (import "cb" (func $cb (param "x" u32) (result u32)))
    (core func $cb_low (canon lower (func $cb)))
    (core module $CM
      (import "env" "cb" (func $cb (param i32) (result i32)))
      (func (export "run") (param i32) (result i32)
        (i32.add (call $cb (local.get 0)) (i32.const 1))
      )
    )
    (core instance $cm
      (instantiate $CM (with "env" (instance (export "cb" (func $cb_low)))))
    )
    (func $run (param "x" u32) (result u32) (canon lift (core func $cm "run")))
    (export "run" (func $run))
  )
  (instance $child (instantiate $Child (with "cb" (func $tramp))))
  (core func $run_low (canon lower (func $child "run")))

  ;; Main: the callback implementation impl(x) = x * 10, and go(x) = child.run(x).
  (core module $Main
    (import "child" "run" (func $run (param i32) (result i32)))
    (func (export "impl") (param i32) (result i32)
      (i32.mul (local.get 0) (i32.const 10)))
    (func (export "go") (param i32) (result i32)
      (call $run (local.get 0)))
  )
  (core instance $main
    (instantiate $Main (with "child" (instance (export "run" (func $run_low)))))
  )

  ;; Fixup: write main.impl into the shim table after main exists.
  (alias core export $shim "tbl" (core table $tbl))
  (alias core export $main "impl" (core func $impl))
  (core module $Fix
    (import "env" "tbl" (table 1 1 funcref))
    (import "env" "impl" (func $impl (param i32) (result i32)))
    (elem (table 0) (i32.const 0) func $impl)
  )
  (core instance $fix
    (instantiate $Fix
      (with "env" (instance (export "tbl" (table $tbl)) (export "impl" (func $impl))))
    )
  )

  (func $go (param "x" u32) (result u32) (canon lift (core func $main "go")))
  (export "go" (func $go))
)
"#;

#[test]
fn wasmtime_executes_donut_adapters() {
    let bytes = wat::parse_str(DONUT_WAT).expect("donut WAT parses");

    let mut config = wasmtime::Config::new();
    config.wasm_component_model(true);
    let engine = wasmtime::Engine::new(&config).expect("engine");
    let component =
        wasmtime::component::Component::new(&engine, &bytes).expect("donut component validates");
    let linker = wasmtime::component::Linker::new(&engine);
    let mut store = wasmtime::Store::new(&engine, ());
    let instance = linker
        .instantiate(&mut store, &component)
        .expect("donut component instantiates");

    let go = instance
        .get_typed_func::<(u32,), (u32,)>(&mut store, "go")
        .expect("go export");
    let (r,) = go
        .call(&mut store, (5,))
        .expect("wasmtime runs the donut parent→child call");
    assert_eq!(r, 51, "go(5) = impl(5) + 1");
}
