//! wado-from-idl: Generate Wado standard library from IDL files (WIT, `WebIDL`)

pub mod codegen;
pub mod ir;
pub mod naming;
pub mod transform;

pub use codegen::WadoCodeGenerator;
pub use ir::WadoModule;
pub use transform::Transformer;
