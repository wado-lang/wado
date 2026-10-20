//! The `wasi:random` a host gives a program whose result must be a function of
//! its input: a Kiln generator, and a program `core:eval` runs.

use rand::SeedableRng;
use rand::rngs::ChaCha12Rng;
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
/// them, so build it through [`fixed_seed`].
pub fn add_to_linker<T: WasiRandomView + 'static>(linker: &mut Linker<T>) -> wasmtime::Result<()> {
    insecure::add_to_linker::<_, WasiRandom>(linker, T::random)?;
    insecure_seed::add_to_linker::<_, WasiRandom>(linker, T::random)
}

/// Answers `wasi:random/insecure` and `insecure-seed` the same on every run, as
/// `insecure-seed` permits. The seeds are arbitrary; only their being fixed
/// matters. `ChaCha12` is named rather than `StdRng`, whose output rand does not
/// promise to keep across versions.
pub fn fixed_seed(builder: &mut WasiCtxBuilder) -> &mut WasiCtxBuilder {
    builder
        .insecure_random(ChaCha12Rng::from_seed([1; 32]))
        .insecure_random_seed(2)
}
