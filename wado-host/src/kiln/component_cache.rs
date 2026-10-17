//! AOT-compiled generator components, compiled once per run.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use wasmtime::Engine;
use wasmtime::component::Component;

use wado_compiler::GeneratorRunnerError;

use super::runtime::{compile_component, generator_engine};
use crate::sync::lock;

/// AOT-compiled generator [`Component`]s, keyed by wasm SHA-256.
///
/// AOT dominates generator execution (see [`compile_component`]), so a
/// generator must be compiled once, not once per fixture. A `wado test` run
/// shares one cache across every per-fixture host, and so does an e2e run.
/// In-memory only — a disk `.cwasm` cache would be a trust-the-disk
/// code-injection vector. Every `Component` comes from one
/// [`generator_engine`], which the cache creates on first use.
#[derive(Default)]
pub struct ComponentCache {
    engine: OnceLock<Engine>,
    /// One slot per generator wasm, its mutex held across that component's AOT
    /// compile so concurrent callers wait rather than each compiling. The list
    /// is locked only to hand a slot out, so distinct components never
    /// serialize.
    slots: Mutex<Vec<([u8; 32], Arc<Mutex<Option<Component>>>)>>,
    /// AOT count (misses); asserted by tests instead of wall-clock timing.
    compile_count: AtomicUsize,
}

impl ComponentCache {
    /// Lazily-created shared engine; run components on the engine that
    /// [`Self::get_or_compile`] returns alongside them.
    fn engine(&self) -> Result<Engine, GeneratorRunnerError> {
        if let Some(engine) = self.engine.get() {
            return Ok(engine.clone());
        }
        let engine = generator_engine().map_err(|error| {
            GeneratorRunnerError::Host(format!("failed to create kiln wasmtime engine: {error}"))
        })?;
        // First `set` wins; always clone from the stored value.
        let _ = self.engine.set(engine);
        Ok(self
            .engine
            .get()
            .expect("kiln engine was set above or by a racing caller")
            .clone())
    }

    /// AOT-compiled component for `wasm` (plus its engine), compiling on a
    /// miss. Single-flighted per component: a test run compiles fixtures in
    /// parallel, so the hosts reach a cold cache together.
    ///
    /// # Errors
    ///
    /// Fails when the engine cannot be created or `wasm` is not a component.
    pub fn get_or_compile(&self, wasm: &[u8]) -> Result<(Engine, Component), GeneratorRunnerError> {
        let engine = self.engine()?;
        let slot = self.slot(wado_compiler::kiln::content_hash(wasm));
        let mut cached = lock(&slot);
        if let Some(component) = cached.as_ref() {
            return Ok((engine, component.clone()));
        }
        let component = compile_component(&engine, wasm)?;
        self.compile_count.fetch_add(1, Ordering::SeqCst);
        *cached = Some(component.clone());
        Ok((engine, component))
    }

    /// The slot for one component hash, creating it on first sight.
    fn slot(&self, key: [u8; 32]) -> Arc<Mutex<Option<Component>>> {
        let mut slots = lock(&self.slots);
        if let Some((_, slot)) = slots.iter().find(|(k, _)| k == &key) {
            return slot.clone();
        }
        let slot = Arc::new(Mutex::new(None));
        slots.push((key, slot.clone()));
        slot
    }

    #[must_use]
    pub fn compile_count(&self) -> usize {
        self.compile_count.load(Ordering::SeqCst)
    }
}
