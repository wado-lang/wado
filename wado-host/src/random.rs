//! The `wasi:random` a host gives a program whose result must be a function of
//! its input: a Kiln generator, and a program `core:eval` runs.

use rand::SeedableRng;
use rand::rngs::ChaCha12Rng;
use wasmtime_wasi::WasiCtxBuilder;

/// Answers `wasi:random/insecure` and `insecure-seed` the same on every run, as
/// `insecure-seed` permits. The seeds are arbitrary; only their being fixed
/// matters. `ChaCha12` is named rather than `StdRng`, whose output rand does not
/// promise to keep across versions.
pub fn fixed_insecure_random(builder: &mut WasiCtxBuilder) -> &mut WasiCtxBuilder {
    builder
        .insecure_random(ChaCha12Rng::from_seed([1; 32]))
        .insecure_random_seed(2)
}
