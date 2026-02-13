//! Bundled math functions (libm) for Wado runtime.
//!
//! This crate compiles to Wasm P1 format for static linking with Wado-generated code.
//! Uses libm for deterministic transcendental math functions.

#![cfg_attr(target_arch = "wasm32", no_std)]

#[cfg(target_arch = "wasm32")]
use core::panic::PanicInfo;

// ============================================================================
// Math functions (libm)
// ============================================================================
// Note: sqrt, abs, ceil, floor, trunc, nearest, min, max, copysign are already
// provided as builtin functions (direct Wasm instructions) in builtin.wado.

// Trigonometric functions (f64)

#[unsafe(no_mangle)]
pub extern "C" fn libm_sin(x: f64) -> f64 {
    libm::sin(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn libm_cos(x: f64) -> f64 {
    libm::cos(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn libm_tan(x: f64) -> f64 {
    libm::tan(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn libm_asin(x: f64) -> f64 {
    libm::asin(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn libm_acos(x: f64) -> f64 {
    libm::acos(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn libm_atan(x: f64) -> f64 {
    libm::atan(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn libm_atan2(y: f64, x: f64) -> f64 {
    libm::atan2(y, x)
}

// Hyperbolic functions (f64)

#[unsafe(no_mangle)]
pub extern "C" fn libm_sinh(x: f64) -> f64 {
    libm::sinh(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn libm_cosh(x: f64) -> f64 {
    libm::cosh(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn libm_tanh(x: f64) -> f64 {
    libm::tanh(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn libm_asinh(x: f64) -> f64 {
    libm::asinh(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn libm_acosh(x: f64) -> f64 {
    libm::acosh(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn libm_atanh(x: f64) -> f64 {
    libm::atanh(x)
}

// Exponential/Logarithmic functions (f64)

#[unsafe(no_mangle)]
pub extern "C" fn libm_exp(x: f64) -> f64 {
    libm::exp(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn libm_exp2(x: f64) -> f64 {
    libm::exp2(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn libm_expm1(x: f64) -> f64 {
    libm::expm1(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn libm_log(x: f64) -> f64 {
    libm::log(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn libm_log2(x: f64) -> f64 {
    libm::log2(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn libm_log10(x: f64) -> f64 {
    libm::log10(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn libm_log1p(x: f64) -> f64 {
    libm::log1p(x)
}

// Power/Root functions (f64)

#[unsafe(no_mangle)]
pub extern "C" fn libm_pow(x: f64, y: f64) -> f64 {
    libm::pow(x, y)
}

#[unsafe(no_mangle)]
pub extern "C" fn libm_cbrt(x: f64) -> f64 {
    libm::cbrt(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn libm_hypot(x: f64, y: f64) -> f64 {
    libm::hypot(x, y)
}

// Remainder function (f64)

#[unsafe(no_mangle)]
pub extern "C" fn libm_fmod(x: f64, y: f64) -> f64 {
    libm::fmod(x, y)
}

// Trigonometric functions (f32)

#[unsafe(no_mangle)]
pub extern "C" fn libm_sinf(x: f32) -> f32 {
    libm::sinf(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn libm_cosf(x: f32) -> f32 {
    libm::cosf(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn libm_tanf(x: f32) -> f32 {
    libm::tanf(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn libm_asinf(x: f32) -> f32 {
    libm::asinf(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn libm_acosf(x: f32) -> f32 {
    libm::acosf(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn libm_atanf(x: f32) -> f32 {
    libm::atanf(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn libm_atan2f(y: f32, x: f32) -> f32 {
    libm::atan2f(y, x)
}

// Hyperbolic functions (f32)

#[unsafe(no_mangle)]
pub extern "C" fn libm_sinhf(x: f32) -> f32 {
    libm::sinhf(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn libm_coshf(x: f32) -> f32 {
    libm::coshf(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn libm_tanhf(x: f32) -> f32 {
    libm::tanhf(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn libm_asinhf(x: f32) -> f32 {
    libm::asinhf(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn libm_acoshf(x: f32) -> f32 {
    libm::acoshf(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn libm_atanhf(x: f32) -> f32 {
    libm::atanhf(x)
}

// Exponential/Logarithmic functions (f32)

#[unsafe(no_mangle)]
pub extern "C" fn libm_expf(x: f32) -> f32 {
    libm::expf(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn libm_exp2f(x: f32) -> f32 {
    libm::exp2f(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn libm_expm1f(x: f32) -> f32 {
    libm::expm1f(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn libm_logf(x: f32) -> f32 {
    libm::logf(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn libm_log2f(x: f32) -> f32 {
    libm::log2f(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn libm_log10f(x: f32) -> f32 {
    libm::log10f(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn libm_log1pf(x: f32) -> f32 {
    libm::log1pf(x)
}

// Power/Root functions (f32)

#[unsafe(no_mangle)]
pub extern "C" fn libm_powf(x: f32, y: f32) -> f32 {
    libm::powf(x, y)
}

#[unsafe(no_mangle)]
pub extern "C" fn libm_cbrtf(x: f32) -> f32 {
    libm::cbrtf(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn libm_hypotf(x: f32, y: f32) -> f32 {
    libm::hypotf(x, y)
}

// Remainder function (f32)

#[unsafe(no_mangle)]
pub extern "C" fn libm_fmodf(x: f32, y: f32) -> f32 {
    libm::fmodf(x, y)
}

#[cfg(target_arch = "wasm32")]
#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    core::arch::wasm32::unreachable();
}
