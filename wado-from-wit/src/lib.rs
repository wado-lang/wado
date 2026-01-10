//! wado-from-wit: Generate Wado standard library from WIT files

pub mod codegen;
pub mod ir;
pub mod naming;
pub mod transform;

pub use codegen::WadoCodeGenerator;
pub use ir::WadoModule;
pub use transform::Transformer;
