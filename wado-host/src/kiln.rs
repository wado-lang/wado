//! Running Kiln generators: the pipeline that plans, runs, and caches the
//! invocations a program declares, and the wasmtime runtime it runs them on.
//! What resolves a generator to its component is the caller's, through
//! [`driver::GeneratorProvider`].

pub mod component_cache;
pub mod driver;
pub mod local;
pub mod metadata;
pub mod runtime;
