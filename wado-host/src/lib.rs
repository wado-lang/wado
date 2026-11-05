//! The native host pieces `wado-cli`, `wado-dev-tools`, the runners, and
//! `wado-compiler-tests` share. A crate of its own so that the compiler's tests
//! need not depend on `wado-cli`.

pub mod atomic_write;
pub mod engine;
#[cfg(feature = "toolchain")]
pub mod fixture;
pub mod fuel;
pub mod guest;
pub mod http_hooks;
#[cfg(feature = "toolchain")]
pub mod kiln;
pub mod options;
pub mod path_search;
pub mod process_host;
pub mod random;
pub mod runner;
#[cfg(unix)]
pub mod sandbox;
#[cfg(feature = "toolchain")]
pub mod stub_host;
pub mod sync;
pub mod ticker;
pub mod timezone;
pub mod tls_trust;
pub mod user_config;

#[cfg(feature = "toolchain")]
pub use stub_host::{HostStubs, StubHost};
