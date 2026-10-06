//! The native host pieces `wado-cli`, `wado-dev-tools`, and `wado-compiler`'s
//! tests share. A crate of its own because the compiler's tests cannot depend
//! on `wado-cli`, which depends on the compiler.

pub mod fixture;
pub mod stub_host;
pub mod timezone;
pub mod tls_trust;

pub use stub_host::{HostStubs, StubHost};
