//! Bundled Wasm modules
//!
//! This module provides access to pre-compiled Wasm modules that are bundled
//! into the compiler. These modules provide runtime functions for Wado programs.
//!
//! - **libm**: provides deterministic math functions (`libm_sin`, `libm_cos`, etc.)

use std::sync::OnceLock;

/// WAT source for the libm (math) module
const WADO_BUNDLED_LIBM_WAT: &str = include_str!("../lib/builtins/wado-bundled-libm.wat");

/// Lazily parsed Wasm bytes for the libm module
static WADO_BUNDLED_LIBM_WASM: OnceLock<Vec<u8>> = OnceLock::new();

/// Get the bundled libm (math) Wasm module bytes
pub fn wado_bundled_libm_wasm() -> &'static [u8] {
    WADO_BUNDLED_LIBM_WASM.get_or_init(|| {
        wat::parse_str(WADO_BUNDLED_LIBM_WAT).expect("Failed to parse bundled libm WAT module")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use wasmparser::Parser;
    use wasmparser::Payload;

    fn collect_exports(wasm: &[u8]) -> Vec<String> {
        let parser = Parser::new(0);
        let mut found_exports = Vec::new();
        for payload in parser.parse_all(wasm) {
            if let Ok(Payload::ExportSection(exports)) = payload {
                for export in exports {
                    if let Ok(exp) = export {
                        found_exports.push(exp.name.to_string());
                    }
                }
            }
        }
        found_exports
    }

    #[test]
    fn test_libm_module_valid() {
        let exports = collect_exports(wado_bundled_libm_wasm());
        assert!(
            exports.contains(&"libm_sin".to_string()),
            "Missing libm_sin export"
        );
        assert!(
            exports.contains(&"libm_cos".to_string()),
            "Missing libm_cos export"
        );
    }
}
