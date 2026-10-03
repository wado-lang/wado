//! Bundled math functions (libm) for Wado runtime.
//!
//! This crate compiles to Wasm P1 format for static linking with Wado-generated code.
//! Uses libm for deterministic transcendental math functions.

#![cfg_attr(target_arch = "wasm32", no_std)]

#[cfg(target_arch = "wasm32")]
use core::panic::PanicInfo;

// Note: sqrt, abs, ceil, floor, trunc, nearest, min, max, copysign are already
// provided as builtin functions (direct Wasm instructions) in builtin.wado.

// Trigonometric functions (f64)

#[unsafe(no_mangle)]
pub extern "C" fn f64_sin(x: f64) -> f64 {
    libm::sin(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn f64_cos(x: f64) -> f64 {
    libm::cos(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn f64_tan(x: f64) -> f64 {
    libm::tan(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn f64_asin(x: f64) -> f64 {
    libm::asin(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn f64_acos(x: f64) -> f64 {
    libm::acos(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn f64_atan(x: f64) -> f64 {
    libm::atan(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn f64_atan2(y: f64, x: f64) -> f64 {
    libm::atan2(y, x)
}

// Hyperbolic functions (f64)

#[unsafe(no_mangle)]
pub extern "C" fn f64_sinh(x: f64) -> f64 {
    libm::sinh(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn f64_cosh(x: f64) -> f64 {
    libm::cosh(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn f64_tanh(x: f64) -> f64 {
    libm::tanh(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn f64_asinh(x: f64) -> f64 {
    libm::asinh(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn f64_acosh(x: f64) -> f64 {
    libm::acosh(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn f64_atanh(x: f64) -> f64 {
    libm::atanh(x)
}

// Exponential/Logarithmic functions (f64)

#[unsafe(no_mangle)]
pub extern "C" fn f64_exp(x: f64) -> f64 {
    libm::exp(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn f64_exp2(x: f64) -> f64 {
    libm::exp2(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn f64_exp10(x: f64) -> f64 {
    libm::exp10(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn f64_expm1(x: f64) -> f64 {
    libm::expm1(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn f64_ln(x: f64) -> f64 {
    libm::log(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn f64_log2(x: f64) -> f64 {
    libm::log2(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn f64_log10(x: f64) -> f64 {
    libm::log10(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn f64_ln1p(x: f64) -> f64 {
    libm::log1p(x)
}

// Power/Root functions (f64)

#[unsafe(no_mangle)]
pub extern "C" fn f64_pow(x: f64, y: f64) -> f64 {
    libm::pow(x, y)
}

#[unsafe(no_mangle)]
pub extern "C" fn f64_cbrt(x: f64) -> f64 {
    libm::cbrt(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn f64_hypot(x: f64, y: f64) -> f64 {
    libm::hypot(x, y)
}

// Remainder function (f64)

#[unsafe(no_mangle)]
pub extern "C" fn f64_fmod(x: f64, y: f64) -> f64 {
    libm::fmod(x, y)
}

// Fused multiply-add (f64)

#[unsafe(no_mangle)]
pub extern "C" fn f64_mul_add(x: f64, y: f64, z: f64) -> f64 {
    libm::fma(x, y, z)
}

// Rounding (f64)

#[unsafe(no_mangle)]
pub extern "C" fn f64_round(x: f64) -> f64 {
    libm::round(x)
}

// Special functions (f64)

#[unsafe(no_mangle)]
pub extern "C" fn f64_erf(x: f64) -> f64 {
    libm::erf(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn f64_erfc(x: f64) -> f64 {
    libm::erfc(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn f64_gamma(x: f64) -> f64 {
    libm::tgamma(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn f64_ln_gamma(x: f64) -> f64 {
    libm::lgamma(x)
}

// Trigonometric functions (f32)

#[unsafe(no_mangle)]
pub extern "C" fn f32_sin(x: f32) -> f32 {
    libm::sinf(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn f32_cos(x: f32) -> f32 {
    libm::cosf(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn f32_tan(x: f32) -> f32 {
    libm::tanf(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn f32_asin(x: f32) -> f32 {
    libm::asinf(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn f32_acos(x: f32) -> f32 {
    libm::acosf(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn f32_atan(x: f32) -> f32 {
    libm::atanf(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn f32_atan2(y: f32, x: f32) -> f32 {
    libm::atan2f(y, x)
}

// Hyperbolic functions (f32)

#[unsafe(no_mangle)]
pub extern "C" fn f32_sinh(x: f32) -> f32 {
    libm::sinhf(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn f32_cosh(x: f32) -> f32 {
    libm::coshf(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn f32_tanh(x: f32) -> f32 {
    libm::tanhf(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn f32_asinh(x: f32) -> f32 {
    libm::asinhf(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn f32_acosh(x: f32) -> f32 {
    libm::acoshf(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn f32_atanh(x: f32) -> f32 {
    libm::atanhf(x)
}

// Exponential/Logarithmic functions (f32)

#[unsafe(no_mangle)]
pub extern "C" fn f32_exp(x: f32) -> f32 {
    libm::expf(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn f32_exp2(x: f32) -> f32 {
    libm::exp2f(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn f32_exp10(x: f32) -> f32 {
    libm::exp10f(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn f32_expm1(x: f32) -> f32 {
    libm::expm1f(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn f32_ln(x: f32) -> f32 {
    libm::logf(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn f32_log2(x: f32) -> f32 {
    libm::log2f(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn f32_log10(x: f32) -> f32 {
    libm::log10f(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn f32_ln1p(x: f32) -> f32 {
    libm::log1pf(x)
}

// Power/Root functions (f32)

#[unsafe(no_mangle)]
pub extern "C" fn f32_pow(x: f32, y: f32) -> f32 {
    libm::powf(x, y)
}

#[unsafe(no_mangle)]
pub extern "C" fn f32_cbrt(x: f32) -> f32 {
    libm::cbrtf(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn f32_hypot(x: f32, y: f32) -> f32 {
    libm::hypotf(x, y)
}

// Remainder function (f32)

#[unsafe(no_mangle)]
pub extern "C" fn f32_fmod(x: f32, y: f32) -> f32 {
    libm::fmodf(x, y)
}

// Fused multiply-add (f32)

#[unsafe(no_mangle)]
pub extern "C" fn f32_mul_add(x: f32, y: f32, z: f32) -> f32 {
    libm::fmaf(x, y, z)
}

// Rounding (f32)

#[unsafe(no_mangle)]
pub extern "C" fn f32_round(x: f32) -> f32 {
    libm::roundf(x)
}

// Special functions (f32)

#[unsafe(no_mangle)]
pub extern "C" fn f32_erf(x: f32) -> f32 {
    libm::erff(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn f32_erfc(x: f32) -> f32 {
    libm::erfcf(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn f32_gamma(x: f32) -> f32 {
    libm::tgammaf(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn f32_ln_gamma(x: f32) -> f32 {
    libm::lgammaf(x)
}

#[cfg(target_arch = "wasm32")]
#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    core::arch::wasm32::unreachable();
}
