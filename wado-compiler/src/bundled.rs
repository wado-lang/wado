//! Bundled Wasm modules
//!
//! This module provides access to pre-compiled Wasm modules that are bundled
//! into the compiler. These modules provide runtime functions for Wado programs.
//!
//! The modules are split into two separate crates to allow independent DCE:
//! - **fts** (float-to-string): provides `f64_to_buffer`, `f32_to_buffer`, etc.
//! - **libm**: provides deterministic math functions (`libm_sin`, `libm_cos`, etc.)

use std::sync::OnceLock;

/// WAT source for the FTS (float-to-string) module
const WADO_BUNDLED_FTS_WAT: &str = include_str!("../lib/builtins/wado-bundled-fts.wat");

/// WAT source for the libm (math) module
const WADO_BUNDLED_LIBM_WAT: &str = include_str!("../lib/builtins/wado-bundled-libm.wat");

/// Lazily parsed Wasm bytes for the FTS module
static WADO_BUNDLED_FTS_WASM: OnceLock<Vec<u8>> = OnceLock::new();

/// Lazily parsed Wasm bytes for the libm module
static WADO_BUNDLED_LIBM_WASM: OnceLock<Vec<u8>> = OnceLock::new();

/// Get the bundled FTS (float-to-string) Wasm module bytes
pub fn wado_bundled_fts_wasm() -> &'static [u8] {
    WADO_BUNDLED_FTS_WASM.get_or_init(|| {
        wat::parse_str(WADO_BUNDLED_FTS_WAT).expect("Failed to parse bundled FTS WAT module")
    })
}

/// Get the bundled libm (math) Wasm module bytes
pub fn wado_bundled_libm_wasm() -> &'static [u8] {
    WADO_BUNDLED_LIBM_WASM.get_or_init(|| {
        wat::parse_str(WADO_BUNDLED_LIBM_WAT).expect("Failed to parse bundled libm WAT module")
    })
}

/// Returns true if the given bundled function name belongs to the FTS module
pub fn is_fts_function(name: &str) -> bool {
    name.starts_with("f32_to_buffer") || name.starts_with("f64_to_buffer")
}

/// Constants for the float-to-string module exports
pub mod float_to_string {
    /// Function name for f64 to buffer conversion (shortest representation)
    pub const F64_TO_BUFFER: &str = "f64_to_buffer";

    /// Function name for f64 to buffer conversion (fixed-point with precision)
    pub const F64_TO_BUFFER_FIXED: &str = "f64_to_buffer_fixed";

    /// Function name for f32 to buffer conversion (shortest representation)
    pub const F32_TO_BUFFER: &str = "f32_to_buffer";

    /// Function name for f32 to buffer conversion (fixed-point with precision)
    pub const F32_TO_BUFFER_FIXED: &str = "f32_to_buffer_fixed";

    /// Function name for f64 to buffer in exponential notation
    pub const F64_TO_BUFFER_EXP: &str = "f64_to_buffer_exp";

    /// Function name for f32 to buffer in exponential notation
    pub const F32_TO_BUFFER_EXP: &str = "f32_to_buffer_exp";
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
    fn test_fts_module_valid() {
        let exports = collect_exports(wado_bundled_fts_wasm());
        assert!(
            exports.contains(&float_to_string::F64_TO_BUFFER.to_string()),
            "Missing f64_to_buffer export"
        );
        assert!(
            exports.contains(&float_to_string::F32_TO_BUFFER.to_string()),
            "Missing f32_to_buffer export"
        );
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

    #[test]
    fn test_is_fts_function() {
        assert!(is_fts_function("f64_to_buffer"));
        assert!(is_fts_function("f64_to_buffer_fixed"));
        assert!(is_fts_function("f32_to_buffer_exp"));
        assert!(!is_fts_function("libm_sin"));
        assert!(!is_fts_function("libm_cosf"));
    }
}
