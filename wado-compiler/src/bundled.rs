//! Bundled Wasm modules
//!
//! This module provides access to pre-compiled Wasm modules that are bundled
//! into the compiler. These modules provide runtime functions for Wado programs.

use std::sync::OnceLock;

/// WAT source for the wado-bundled module (float-to-string and other builtins)
const WADO_BUNDLED_WAT: &str = include_str!("../lib/builtins/wado-bundled.wat");

/// Lazily parsed Wasm bytes for the bundled module
static WADO_BUNDLED_WASM: OnceLock<Vec<u8>> = OnceLock::new();

/// Get the bundled Wasm module bytes (float-to-string and other builtins)
///
/// This parses the WAT source on first access and caches the result.
pub fn wado_bundled_wasm() -> &'static [u8] {
    WADO_BUNDLED_WASM.get_or_init(|| {
        wat::parse_str(WADO_BUNDLED_WAT).expect("Failed to parse bundled WAT module")
    })
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

    #[test]
    fn test_wado_bundled_wasm_valid() {
        // Verify the bundled Wasm is valid
        let wasm = wado_bundled_wasm();
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

        // Check expected exports are present
        assert!(
            found_exports.contains(&float_to_string::F64_TO_BUFFER.to_string()),
            "Missing f64_to_buffer export"
        );
        assert!(
            found_exports.contains(&float_to_string::F64_TO_BUFFER_FIXED.to_string()),
            "Missing f64_to_buffer_fixed export"
        );
        assert!(
            found_exports.contains(&float_to_string::F32_TO_BUFFER.to_string()),
            "Missing f32_to_buffer export"
        );
        assert!(
            found_exports.contains(&float_to_string::F32_TO_BUFFER_FIXED.to_string()),
            "Missing f32_to_buffer_fixed export"
        );
        assert!(
            found_exports.contains(&float_to_string::F64_TO_BUFFER_EXP.to_string()),
            "Missing f64_to_buffer_exp export"
        );
        assert!(
            found_exports.contains(&float_to_string::F32_TO_BUFFER_EXP.to_string()),
            "Missing f32_to_buffer_exp export"
        );
    }
}
