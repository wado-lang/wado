//! The native host pieces `wado-cli`, `wado-dev-tools`, the runners, and
//! `wado-compiler-tests` share. A crate of its own so that the compiler's tests
//! need not depend on `wado-cli`.

#[cfg(feature = "toolchain")]
pub mod atomic_write;
pub mod engine;
#[cfg(feature = "toolchain")]
pub mod fixture;
pub mod fuel;
#[cfg(feature = "toolchain")]
pub mod kiln;
#[cfg(feature = "toolchain")]
pub mod random;
pub mod runner;
#[cfg(all(unix, feature = "toolchain"))]
pub mod sandbox;
#[cfg(feature = "toolchain")]
pub mod stub_host;
#[cfg(feature = "toolchain")]
pub mod sync;
#[cfg(feature = "toolchain")]
pub mod timezone;
#[cfg(feature = "toolchain")]
pub mod tls_trust;

#[cfg(feature = "toolchain")]
pub use stub_host::{HostStubs, StubHost};
