//! Optimization pass for Wado TIR
//!
//! The optimize phase performs IR-to-IR optimizations:
//! - Dead code elimination
//! - Constant folding and propagation
//! - Inlining (within optimization budget)
//! - Link-time optimization across modules
//!
//! Currently this is a pass-through stub. Optimizations will be added
//! incrementally.

use crate::tir::TirModule;

/// Optimization level
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OptLevel {
    /// No optimizations (fastest compilation)
    #[default]
    None,
    /// Basic optimizations (constant folding, simple DCE)
    Basic,
    /// Full optimizations (inlining, LTO)
    Full,
}

/// Optimize a TIR module (currently pass-through)
pub fn optimize(module: TirModule, _level: OptLevel) -> TirModule {
    // Pass-through for now
    // Future: Add optimizations based on level
    module
}

/// Optimize with default level (None)
pub fn optimize_default(module: TirModule) -> TirModule {
    optimize(module, OptLevel::None)
}

/// Optimize multiple modules with LTO (link-time optimization)
pub fn optimize_with_lto(modules: Vec<TirModule>, _level: OptLevel) -> Vec<TirModule> {
    // Pass-through for now
    // Future: Cross-module optimizations
    modules
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_optimize_passthrough() {
        let module = TirModule::new(vec!["test".to_string()]);
        let optimized = optimize(module, OptLevel::None);
        assert_eq!(optimized.path, vec!["test".to_string()]);
    }
}
