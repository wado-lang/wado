//! Host implementation for `wasi:clocks/timezone@0.3.0-rc-2026-03-15`.
//!
//! Wasmtime ships the binding for this interface gated behind the unstable
//! `clocks-timezone` feature, but provides no host implementation, so we
//! supply our own. The implementation is intentionally minimal — it asks
//! the operating system about the current machine's local zone and offset:
//!
//! - `iana-id` / `to-debug-string`: `iana_time_zone::get_timezone()`.
//! - `utc-offset`: `localtime_r(3)` and `tm_gmtoff`.
//!
//! On non-Unix targets `utc-offset` returns `None` (the spec allows that).

use anyhow::Result;
use wasmtime::component::{HasData, Linker};
use wasmtime_wasi::p3::bindings::clocks::timezone::{self, Host, Instant, LinkOptions};

pub struct WadoTimezone;

impl HasData for WadoTimezone {
    type Data<'a> = TimezoneCtx;
}

pub struct TimezoneCtx;

impl Host for TimezoneCtx {
    fn iana_id(&mut self) -> wasmtime::Result<Option<String>> {
        Ok(iana_time_zone::get_timezone().ok())
    }

    fn utc_offset(&mut self, when: Instant) -> wasmtime::Result<Option<i64>> {
        Ok(local_offset_nanos(when.seconds))
    }

    fn to_debug_string(&mut self) -> wasmtime::Result<String> {
        Ok(match iana_time_zone::get_timezone() {
            Ok(timezone) => timezone,
            Err(err) => format!("timezone unavailable: {err}"),
        })
    }
}

/// Wire the timezone interface onto the linker.
///
/// # Errors
///
/// Returns an error if the linker rejects the import (e.g., a duplicate
/// definition for `wasi:clocks/timezone`).
pub fn add_to_linker<T: 'static>(linker: &mut Linker<T>) -> Result<()> {
    let mut options = LinkOptions::default();
    options.clocks_timezone(true);
    timezone::add_to_linker::<T, WadoTimezone>(linker, &options, |_| TimezoneCtx)?;
    Ok(())
}

#[cfg(unix)]
fn local_offset_nanos(secs: i64) -> Option<i64> {
    use std::mem::MaybeUninit;
    // `time_t` is `i64` on glibc/x86_64 (the conversion is a no-op there) but
    // `i32` on 32-bit Unix targets without `_TIME_BITS=64`. Keep `try_into`
    // for portability and silence the lint on the platforms where it's a no-op.
    #[allow(clippy::useless_conversion)]
    let t: libc::time_t = secs.try_into().ok()?;
    let mut tm: MaybeUninit<libc::tm> = MaybeUninit::uninit();
    let ret = unsafe { libc::localtime_r(&raw const t, tm.as_mut_ptr()) };
    if ret.is_null() {
        return None;
    }
    let tm = unsafe { tm.assume_init() };
    Some(tm.tm_gmtoff * 1_000_000_000)
}

#[cfg(not(unix))]
fn local_offset_nanos(_secs: i64) -> Option<i64> {
    None
}
