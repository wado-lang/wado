//! The `wasi:random` a host gives a program whose result must be a function of
//! its input: a Kiln generator, a program `core:eval` runs, and a test.

use std::convert::Infallible;

use rand::TryRng;
use wasmtime::component::Linker;
use wasmtime_wasi::WasiCtxBuilder;
use wasmtime_wasi::p3::bindings::random::{insecure, insecure_seed};
use wasmtime_wasi::random::{WasiRandom, WasiRandomView};

/// The interfaces [`add_to_linker`] links.
pub const INTERFACES: [&str; 2] = [
    "wasi:random/insecure@0.3.0",
    "wasi:random/insecure-seed@0.3.0",
];

/// Links `wasi:random/insecure` and `insecure-seed`. The store's context answers
/// them, so build it through [`zeroed`].
pub fn add_to_linker<T: WasiRandomView + 'static>(linker: &mut Linker<T>) -> wasmtime::Result<()> {
    insecure::add_to_linker::<_, WasiRandom>(linker, T::random)?;
    insecure_seed::add_to_linker::<_, WasiRandom>(linker, T::random)
}

/// Answers `wasi:random/insecure` and `insecure-seed` with zeros, which both
/// permit: `insecure` places no requirement on its values, and `insecure-seed`
/// may be entirely deterministic.
pub fn zeroed(builder: &mut WasiCtxBuilder) -> &mut WasiCtxBuilder {
    builder.insecure_random(Zeros).insecure_random_seed(0)
}

struct Zeros;

impl TryRng for Zeros {
    type Error = Infallible;

    fn try_next_u32(&mut self) -> Result<u32, Infallible> {
        Ok(0)
    }

    fn try_next_u64(&mut self) -> Result<u64, Infallible> {
        Ok(0)
    }

    fn try_fill_bytes(&mut self, dst: &mut [u8]) -> Result<(), Infallible> {
        dst.fill(0);
        Ok(())
    }
}
