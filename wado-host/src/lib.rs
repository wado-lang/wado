//! The native host pieces `wado-cli`, `wado-dev-tools`, and
//! `wado-compiler-tests` share. A crate of its own so that the compiler's tests
//! need not depend on `wado-cli`.

pub mod atomic_write;
pub mod engine;
pub mod fixture;
pub mod fuel;
pub mod kiln;
pub mod random;
#[cfg(unix)]
pub mod sandbox;
pub mod stub_host;
pub mod sync;
pub mod timezone;
pub mod tls_trust;

pub use stub_host::{HostStubs, StubHost};
