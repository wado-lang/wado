# The Wasm Loader (Security Model for external plugins)

Effect declaration = Wasm import = WASI capability:

```wado
use {load_wasm} from "core:wasm";

// Restrict plugin capabilities with world and effect declarations
world Transform {
    // ...

    export fn execute(input: String) with FileSystem -> String;
}

let plugin = load_wasm<Transform>("./transform.wasm");

let output = plugin.execute(input);
```

## Requirements

This feature requires:

1. **WASI interface for component instantiation** - A standardized interface (e.g., `wasi:component/runtime`) that allows a component to instantiate another component at runtime. This does not exist in WASI yet.

2. **Host runtime support** - Wasmtime (or other runtimes) must expose dynamic component instantiation to guest components.

3. **Import/capability specification** - A mechanism to specify which imports to provide to the loaded component, enabling capability restriction.

Currently, component composition in the Component Model is **static** (build-time linking via tools like `wasm-compose`). Dynamic loading from within a component is a future capability dependent on WASI evolution.
