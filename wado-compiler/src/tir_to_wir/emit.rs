//! WIR emission — converts a `WirModule` into Wasm binary bytes.
//!
//! Currently contains only the stub component builder for Phase 2.
//! Phase 3 will implement the full `WirModule` → Wasm translation.

use wasm_encoder::{
    CodeSection, ComponentBuilder, ComponentExportKind, ComponentValType, ExportKind,
    ExportSection, Function, FunctionSection, Instruction, Module, TypeSection, ValType,
};

/// Build a minimal valid Wasm component for the Phase 2 stub.
///
/// The component exports a synchronous "run" function that returns `result<_, _>`
/// (Ok with unit payload). It has no WASI imports, no stdout, no side effects.
///
/// This produces a component that wasmtime can load and execute, allowing
/// the E2E test infrastructure to run the full pipeline. All tests will fail
/// because the stub produces no output, but they fail at the assertion level
/// (output mismatch) rather than the infrastructure level (invalid Wasm).
pub fn build_stub_component() -> Vec<u8> {
    let mut component = ComponentBuilder::default();

    // 1. Build a minimal core module that exports "run" returning i32
    let core_module = build_stub_core_module();
    component.core_module_raw(Some("m"), &core_module);

    // 2. Instantiate the core module (no imports needed)
    component.core_instantiate(
        Some("i"),
        0, // core module index
        Vec::<(&str, wasm_encoder::ModuleArg)>::new(),
    );

    // 3. Alias the core "run" function out of the instance
    component.core_alias_export(
        Some("run-core"),
        0, // core instance index
        "run",
        ExportKind::Func,
    );

    // 4. Define the result type: result<_, _> (both payloads are unit)
    let (result_type_idx, enc) = component.ty(Some("result-ty"));
    enc.defined_type().result(None, None);

    // 5. Define the function type: () -> result<_, _>
    let (_func_type_idx, enc) = component.ty(Some("run-ty"));
    enc.function()
        .params::<[(&str, ComponentValType); 0], ComponentValType>([])
        .result(Some(ComponentValType::Type(result_type_idx)));

    // 6. Canon lift: wrap core function as component function
    //    Synchronous lifting: core function returns i32 (0=Ok, 1=Err for result)
    //    call_async works fine with synchronously-lifted functions.
    component.lift_func(
        Some("run"),
        0, // core func index (from alias)
        1, // component type index (run-ty)
        [],
    );

    // 7. Export the component function as "run"
    component.export(
        "run",
        ComponentExportKind::Func,
        0, // component func index
        None,
    );

    component.finish()
}

/// Build a minimal core Wasm module with a single "run" function.
///
/// The function signature is `() -> i32`:
/// - Returns 0 for Ok(()) in the result discriminant
fn build_stub_core_module() -> Vec<u8> {
    let mut module = Module::new();

    // Type section: () -> i32
    let mut types = TypeSection::new();
    types.ty().function([], [ValType::I32]);
    module.section(&types);

    // Function section: 1 function of type 0
    let mut funcs = FunctionSection::new();
    funcs.function(0);
    module.section(&funcs);

    // Export section: export "run"
    let mut exports = ExportSection::new();
    exports.export("run", ExportKind::Func, 0);
    module.section(&exports);

    // Code section: return 0 (Ok)
    let mut code = CodeSection::new();
    let mut f = Function::new([]);
    f.instruction(&Instruction::I32Const(0)); // result: Ok(())
    f.instruction(&Instruction::End);
    code.function(&f);
    module.section(&code);

    module.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stub_component_is_valid_wasm() {
        let wasm = build_stub_component();
        // Verify it's a valid Wasm component by checking the magic bytes
        assert!(wasm.len() > 8, "component should have content");
        // Wasm component magic: \0asm followed by layer 0x0a (component)
        assert_eq!(&wasm[0..4], b"\0asm", "should start with Wasm magic");

        // Validate with wasmparser
        let mut validator =
            wasmparser::Validator::new_with_features(wasmparser::WasmFeatures::all());
        validator
            .validate_all(&wasm)
            .expect("stub component should be valid Wasm");
    }
}
