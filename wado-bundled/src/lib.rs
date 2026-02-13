//! Bundled utilities for Wado runtime.
//!
//! This crate compiles to Wasm P1 format for static linking with Wado-generated code.
//!
//! Provides:
//! - Float-to-string conversion using fpfmt
//! - Math functions (transcendentals) using libm

#![cfg_attr(target_arch = "wasm32", no_std)]

#[cfg(target_arch = "wasm32")]
use core::panic::PanicInfo;

/// Copy bytes to a destination pointer in linear memory.
///
/// # Safety
/// The destination must have enough space for `src.len()` bytes.
unsafe fn copy_to_ptr(dest_ptr: i32, src: &[u8]) {
    let dest = dest_ptr as *mut u8;
    for (i, &byte) in src.iter().enumerate() {
        // SAFETY: Caller guarantees dest has enough space
        unsafe { dest.add(i).write(byte) };
    }
}

// ============================================================================
// Float-to-string formatting using fpfmt
// ============================================================================

/// Write the decimal digits of `d` into `buf[0..nd]`.
fn write_digits(buf: &mut [u8], mut d: u64, nd: usize) {
    for i in (0..nd).rev() {
        buf[i] = b'0' + (d % 10) as u8;
        d /= 10;
    }
}

/// Format `(d, p)` in exponential notation: `d.ddde[-]ee`
///
/// Returns number of bytes written.
fn fmt_exp(buf: &mut [u8], d: u64, p: i32, nd: i32) -> usize {
    let nd = nd as usize;
    let exp = nd as i32 + p - 1;

    // Write all digits starting at position 0
    write_digits(buf, d, nd);

    let mut pos;
    if nd > 1 {
        // Shift digits right by 1 to make room for decimal point after first digit
        let mut i = nd;
        while i > 1 {
            buf[i] = buf[i - 1];
            i -= 1;
        }
        buf[1] = b'.';
        pos = nd + 1;
    } else {
        pos = nd;
    }

    // Write exponent
    buf[pos] = b'e';
    pos += 1;

    let abs_exp = if exp < 0 {
        buf[pos] = b'-';
        pos += 1;
        (-exp) as usize
    } else {
        exp as usize
    };

    // Write exponent digits (no leading zeros, no '+' sign)
    if abs_exp >= 100 {
        buf[pos] = b'0' + (abs_exp / 100) as u8;
        buf[pos + 1] = b'0' + ((abs_exp / 10) % 10) as u8;
        buf[pos + 2] = b'0' + (abs_exp % 10) as u8;
        pos + 3
    } else if abs_exp >= 10 {
        buf[pos] = b'0' + (abs_exp / 10) as u8;
        buf[pos + 1] = b'0' + (abs_exp % 10) as u8;
        pos + 2
    } else {
        buf[pos] = b'0' + abs_exp as u8;
        pos + 1
    }
}

/// Format `(d, p)` as shortest decimal or exponential string into `buf`.
///
/// Uses decimal notation for exponents in `[-4, 15]`, exponential otherwise.
/// Always includes at least one decimal digit (e.g., `"13.0"` not `"13"`).
///
/// Returns number of bytes written.
fn fmt_shortest(buf: &mut [u8], d: u64, p: i32, nd: i32) -> usize {
    let exp = nd + p - 1; // base-10 exponent of leading digit

    // Use exponential for very large or very small values
    if !(-4..=15).contains(&exp) {
        return fmt_exp(buf, d, p, nd);
    }

    let nd_usize = nd as usize;

    if p >= 0 {
        // Integer: digits + trailing zeros + ".0"
        let p_usize = p as usize;
        write_digits(buf, d, nd_usize);
        let mut i = 0;
        while i < p_usize {
            buf[nd_usize + i] = b'0';
            i += 1;
        }
        buf[nd_usize + p_usize] = b'.';
        buf[nd_usize + p_usize + 1] = b'0';
        nd_usize + p_usize + 2
    } else {
        let neg_p = (-p) as usize;
        if exp >= 0 {
            // Mixed integer and fractional: e.g., (123, -2) → "1.23"
            let int_digits = nd_usize - neg_p;
            write_digits(buf, d, nd_usize);
            // Shift fractional part right by 1 to insert decimal point
            let mut i = nd_usize;
            while i > int_digits {
                buf[i] = buf[i - 1];
                i -= 1;
            }
            buf[int_digits] = b'.';
            nd_usize + 1
        } else {
            // Pure fraction: e.g., (1, -1) → "0.1", (35, -3) → "0.035"
            let leading_zeros = neg_p - nd_usize;
            buf[0] = b'0';
            buf[1] = b'.';
            let mut i = 0;
            while i < leading_zeros {
                buf[2 + i] = b'0';
                i += 1;
            }
            write_digits(&mut buf[2 + leading_zeros..], d, nd_usize);
            2 + leading_zeros + nd_usize
        }
    }
}

/// Find shortest decimal representation `(d, p)` that round-trips through f32.
///
/// Uses fpfmt's f64 `fixed_width` with increasing digit counts, verifying
/// each candidate via `parse` → `as f32` round-trip.
/// f32 needs at most 9 significant digits for unique identification.
#[allow(clippy::cast_possible_truncation)]
fn f32_short(f: f32) -> (u64, i32) {
    let f64_val = f64::from(f);

    // Linear scan: try increasing digit counts until round-trip succeeds
    let mut n = 1;
    while n <= 9 {
        let (d, p) = fpfmt::fixed_width(f64_val, n);
        if fpfmt::parse(d, p) as f32 == f {
            return (d, p);
        }
        n += 1;
    }

    // Fallback: 9 digits always suffices for f32
    fpfmt::fixed_width(f64_val, 9)
}

/// Format an f64 value using shortest representation to the provided buffer.
///
/// # Safety
/// The buffer must be at least 32 bytes.
#[unsafe(no_mangle)]
#[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
pub extern "C" fn f64_to_buffer(value: f64, buffer_ptr: i32) -> i32 {
    let mut buf = [0u8; 32];

    // Handle special cases
    if value != value {
        // NaN
        unsafe { copy_to_ptr(buffer_ptr, b"NaN") };
        return 3;
    }
    if value == f64::INFINITY {
        unsafe { copy_to_ptr(buffer_ptr, b"inf") };
        return 3;
    }
    if value == f64::NEG_INFINITY {
        unsafe { copy_to_ptr(buffer_ptr, b"-inf") };
        return 4;
    }
    if value == 0.0 {
        if value.to_bits() >> 63 != 0 {
            unsafe { copy_to_ptr(buffer_ptr, b"-0.0") };
            return 4;
        }
        unsafe { copy_to_ptr(buffer_ptr, b"0.0") };
        return 3;
    }

    let mut pos = 0;
    let f = if value < 0.0 {
        buf[0] = b'-';
        pos = 1;
        -value
    } else {
        value
    };

    let (d, p) = fpfmt::short(f);
    let nd = fpfmt::digits(d);
    let len = fmt_shortest(&mut buf[pos..], d, p, nd);
    let total = pos + len;

    unsafe { copy_to_ptr(buffer_ptr, &buf[..total]) };
    total as i32
}

/// Format an f64 value with fixed-point precision to the provided buffer.
///
/// # Safety
/// The buffer must be at least 400 bytes.
#[unsafe(no_mangle)]
#[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
pub extern "C" fn f64_to_buffer_fixed(value: f64, precision: i32, buffer_ptr: i32) -> i32 {
    f64_to_buffer_fixed_impl(value, precision, buffer_ptr)
}

/// Format an f32 value using shortest representation to the provided buffer.
///
/// # Safety
/// The buffer must be at least 24 bytes.
#[unsafe(no_mangle)]
#[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
pub extern "C" fn f32_to_buffer(value: f32, buffer_ptr: i32) -> i32 {
    let mut buf = [0u8; 24];

    // Handle special cases
    if value != value {
        // NaN
        unsafe { copy_to_ptr(buffer_ptr, b"NaN") };
        return 3;
    }
    if value == f32::INFINITY {
        unsafe { copy_to_ptr(buffer_ptr, b"inf") };
        return 3;
    }
    if value == f32::NEG_INFINITY {
        unsafe { copy_to_ptr(buffer_ptr, b"-inf") };
        return 4;
    }
    if value == 0.0 {
        if value.to_bits() >> 31 != 0 {
            unsafe { copy_to_ptr(buffer_ptr, b"-0.0") };
            return 4;
        }
        unsafe { copy_to_ptr(buffer_ptr, b"0.0") };
        return 3;
    }

    let mut pos = 0;
    let f = if value < 0.0 {
        buf[0] = b'-';
        pos = 1;
        -value
    } else {
        value
    };

    let (d, p) = f32_short(f);
    let nd = fpfmt::digits(d);
    let len = fmt_shortest(&mut buf[pos..], d, p, nd);
    let total = pos + len;

    unsafe { copy_to_ptr(buffer_ptr, &buf[..total]) };
    total as i32
}

/// Format an f32 value with fixed-point precision to the provided buffer.
///
/// # Safety
/// The buffer must be at least 64 bytes.
#[unsafe(no_mangle)]
#[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
pub extern "C" fn f32_to_buffer_fixed(value: f32, precision: i32, buffer_ptr: i32) -> i32 {
    // Promote to f64 and format (exact conversion)
    f64_to_buffer_fixed_impl(f64::from(value), precision, buffer_ptr)
}

// ============================================================================
// Fixed-point formatting (for Display with precision specifier)
// ============================================================================

/// Format `(d, p, nd)` with exactly `precision` decimal places into `buf`.
///
/// Returns number of bytes written.
fn fmt_fixed(buf: &mut [u8], d: u64, p: i32, nd: i32, precision: i32) -> usize {
    let nd_usize = nd as usize;
    let prec = precision as usize;

    // Determine integer part length: nd + p digits before decimal point
    let int_len = nd + p; // can be negative (pure fraction) or very large

    if int_len <= 0 {
        // Pure fraction: "0." + leading_zeros + digits + trailing_zeros
        buf[0] = b'0';
        buf[1] = b'.';
        let leading_zeros = (-int_len) as usize;

        if leading_zeros >= prec {
            // All precision digits are zeros
            let mut i = 0;
            while i < prec {
                buf[2 + i] = b'0';
                i += 1;
            }
            return 2 + prec;
        }

        // Fill leading zeros
        let mut i = 0;
        while i < leading_zeros {
            buf[2 + i] = b'0';
            i += 1;
        }

        // Fill significant digits (as many as fit in precision)
        let avail = prec - leading_zeros;
        let digits_to_write = if nd_usize < avail { nd_usize } else { avail };
        write_digits(&mut buf[2 + leading_zeros..], d, digits_to_write);

        // If we used fewer digits than d has, that's fine (truncated; rounding handled by caller)
        // Fill remaining with zeros
        let mut i = leading_zeros + digits_to_write;
        while i < prec {
            buf[2 + i] = b'0';
            i += 1;
        }
        2 + prec
    } else {
        // Has integer part
        let int_usize = int_len as usize;

        if int_len >= nd {
            // All significant digits are in integer part, plus trailing zeros
            write_digits(buf, d, nd_usize);
            let trailing = int_usize - nd_usize;
            let mut i = 0;
            while i < trailing {
                buf[nd_usize + i] = b'0';
                i += 1;
            }
            // Decimal point and precision zeros
            buf[int_usize] = b'.';
            let mut i = 0;
            while i < prec {
                buf[int_usize + 1 + i] = b'0';
                i += 1;
            }
            int_usize + 1 + prec
        } else {
            // Mixed: some digits before and after decimal point
            let frac_digits_from_d = nd_usize - int_usize; // digits of d after decimal
            write_digits(buf, d, nd_usize);
            // Shift fractional part right to insert decimal point
            let mut i = nd_usize;
            while i > int_usize {
                buf[i] = buf[i - 1];
                i -= 1;
            }
            buf[int_usize] = b'.';

            if frac_digits_from_d >= prec {
                // We have enough fractional digits; truncate
                int_usize + 1 + prec
            } else {
                // Need more fractional digits: pad with zeros
                let pos = int_usize + 1 + frac_digits_from_d;
                let mut i = 0;
                while i < prec - frac_digits_from_d {
                    buf[pos + i] = b'0';
                    i += 1;
                }
                int_usize + 1 + prec
            }
        }
    }
}

/// Format an f64 value with exactly `precision` decimal places.
///
/// # Safety
/// The buffer must be large enough (recommend 400 bytes for arbitrary f64 + precision).
#[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
fn f64_to_buffer_fixed_impl(value: f64, precision: i32, buffer_ptr: i32) -> i32 {
    let mut buf = [0u8; 400];

    // Handle special cases
    if value != value {
        unsafe { copy_to_ptr(buffer_ptr, b"NaN") };
        return 3;
    }
    if value == f64::INFINITY {
        unsafe { copy_to_ptr(buffer_ptr, b"inf") };
        return 3;
    }
    if value == f64::NEG_INFINITY {
        unsafe { copy_to_ptr(buffer_ptr, b"-inf") };
        return 4;
    }
    if value == 0.0 {
        // "0.000..." with precision zeros
        let mut pos = 0;
        if value.to_bits() >> 63 != 0 {
            buf[0] = b'-';
            pos = 1;
        }
        buf[pos] = b'0';
        buf[pos + 1] = b'.';
        let mut i = 0;
        let prec = precision as usize;
        while i < prec {
            buf[pos + 2 + i] = b'0';
            i += 1;
        }
        let total = pos + 2 + prec;
        unsafe { copy_to_ptr(buffer_ptr, &buf[..total]) };
        return total as i32;
    }

    let mut pos = 0;
    let f = if value < 0.0 {
        buf[0] = b'-';
        pos = 1;
        -value
    } else {
        value
    };

    // Get the shortest representation to determine magnitude
    let (d_short, p_short) = fpfmt::short(f);
    let nd_short = fpfmt::digits(d_short);
    let int_len = nd_short + p_short; // number of integer digits

    // Compute how many significant digits we need
    let nd_needed = if int_len <= 0 {
        // Pure fraction: we need enough digits to cover the precision
        // after leading zeros. We need digits from position (|int_len|) onward.
        let needed = precision + (-int_len);
        // But we can only get at most 18 from fixed_width
        if needed <= 0 { 1 } else { needed.min(18) }
    } else {
        // Has integer part
        let needed = int_len + precision;
        needed.min(18)
    };

    let nd_needed = nd_needed.max(1);

    let (d, p) = if nd_needed <= nd_short {
        // We already have enough digits from short; re-round with fixed_width
        fpfmt::fixed_width(f, nd_needed)
    } else if nd_needed > 17 {
        // For very large integers, use 17 digits (max meaningful for f64)
        fpfmt::fixed_width(f, 17.min(nd_needed))
    } else {
        fpfmt::fixed_width(f, nd_needed)
    };
    let nd = fpfmt::digits(d);

    let len = fmt_fixed(&mut buf[pos..], d, p, nd, precision);
    let total = pos + len;

    unsafe { copy_to_ptr(buffer_ptr, &buf[..total]) };
    total as i32
}

// ============================================================================
// Exponential formatting (for LowerExp/UpperExp traits)
// ============================================================================

/// Format an f64 value in exponential notation.
///
/// If `precision < 0`, use shortest representation.
/// If `precision >= 0`, use `precision + 1` significant digits.
///
/// # Safety
/// The buffer must be at least 32 bytes.
#[unsafe(no_mangle)]
#[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
pub extern "C" fn f64_to_buffer_exp(value: f64, precision: i32, buffer_ptr: i32) -> i32 {
    let mut buf = [0u8; 32];

    // Handle special cases
    if value != value {
        unsafe { copy_to_ptr(buffer_ptr, b"NaN") };
        return 3;
    }
    if value == f64::INFINITY {
        unsafe { copy_to_ptr(buffer_ptr, b"inf") };
        return 3;
    }
    if value == f64::NEG_INFINITY {
        unsafe { copy_to_ptr(buffer_ptr, b"-inf") };
        return 4;
    }
    if value == 0.0 {
        let mut pos = 0;
        if value.to_bits() >> 63 != 0 {
            buf[0] = b'-';
            pos = 1;
        }
        if precision < 0 {
            // "0e0"
            buf[pos] = b'0';
            buf[pos + 1] = b'e';
            buf[pos + 2] = b'0';
            let total = pos + 3;
            unsafe { copy_to_ptr(buffer_ptr, &buf[..total]) };
            return total as i32;
        }
        // "0.000...e0"
        buf[pos] = b'0';
        if precision > 0 {
            buf[pos + 1] = b'.';
            let prec = precision as usize;
            let mut i = 0;
            while i < prec {
                buf[pos + 2 + i] = b'0';
                i += 1;
            }
            buf[pos + 2 + prec] = b'e';
            buf[pos + 3 + prec] = b'0';
            let total = pos + 4 + prec;
            unsafe { copy_to_ptr(buffer_ptr, &buf[..total]) };
            return total as i32;
        }
        // precision == 0: "0e0"
        buf[pos] = b'0';
        buf[pos + 1] = b'e';
        buf[pos + 2] = b'0';
        let total = pos + 3;
        unsafe { copy_to_ptr(buffer_ptr, &buf[..total]) };
        return total as i32;
    }

    let mut pos = 0;
    let f = if value < 0.0 {
        buf[0] = b'-';
        pos = 1;
        -value
    } else {
        value
    };

    let (d, p, nd) = if precision < 0 {
        // Shortest
        let (d, p) = fpfmt::short(f);
        let nd = fpfmt::digits(d);
        (d, p, nd)
    } else {
        // precision + 1 significant digits (precision is # of digits after decimal)
        let n = (precision + 1).min(18).max(1);
        let (d, p) = fpfmt::fixed_width(f, n);
        let nd = fpfmt::digits(d);
        (d, p, nd)
    };

    let len = fmt_exp(&mut buf[pos..], d, p, nd);
    let total = pos + len;

    unsafe { copy_to_ptr(buffer_ptr, &buf[..total]) };
    total as i32
}

/// Format an f32 value in exponential notation.
///
/// # Safety
/// The buffer must be at least 24 bytes.
#[unsafe(no_mangle)]
#[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
pub extern "C" fn f32_to_buffer_exp(value: f32, precision: i32, buffer_ptr: i32) -> i32 {
    // Promote to f64 and format
    f64_to_buffer_exp(f64::from(value), precision, buffer_ptr)
}

#[cfg(target_arch = "wasm32")]
#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    core::arch::wasm32::unreachable();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Format an f64 using the shortest representation (same logic as `f64_to_buffer`).
    fn format_f64(value: f64) -> String {
        if value != value {
            return "NaN".into();
        }
        if value == f64::INFINITY {
            return "inf".into();
        }
        if value == f64::NEG_INFINITY {
            return "-inf".into();
        }
        if value == 0.0 {
            return if value.to_bits() >> 63 != 0 {
                "-0.0"
            } else {
                "0.0"
            }
            .into();
        }
        let mut buf = [0u8; 32];
        let mut pos = 0;
        let f = if value < 0.0 {
            buf[0] = b'-';
            pos = 1;
            -value
        } else {
            value
        };
        let (d, p) = fpfmt::short(f);
        let nd = fpfmt::digits(d);
        let len = fmt_shortest(&mut buf[pos..], d, p, nd);
        String::from_utf8(buf[..pos + len].to_vec()).unwrap()
    }

    /// Format an f32 using the shortest representation (same logic as `f32_to_buffer`).
    fn format_f32(value: f32) -> String {
        if value != value {
            return "NaN".into();
        }
        if value == f32::INFINITY {
            return "inf".into();
        }
        if value == f32::NEG_INFINITY {
            return "-inf".into();
        }
        if value == 0.0 {
            return if value.to_bits() >> 31 != 0 {
                "-0.0"
            } else {
                "0.0"
            }
            .into();
        }
        let mut buf = [0u8; 24];
        let mut pos = 0;
        let f = if value < 0.0 {
            buf[0] = b'-';
            pos = 1;
            -value
        } else {
            value
        };
        let (d, p) = f32_short(f);
        let nd = fpfmt::digits(d);
        let len = fmt_shortest(&mut buf[pos..], d, p, nd);
        String::from_utf8(buf[..pos + len].to_vec()).unwrap()
    }

    #[test]
    fn test_f64_display_matches_expected() {
        let cases: &[(f64, &str)] = &[
            (1.23, "1.23"),
            (3.14159, "3.14159"),
            (13.0, "13.0"),
            (0.035, "0.035"),
            (6.022e23, "6.022e23"),
            (1.0, "1.0"),
            (42.0, "42.0"),
            (100.0, "100.0"),
            (255.0, "255.0"),
            (10.0, "10.0"),
            (63.0, "63.0"),
            (-2.5, "-2.5"),
            (0.1, "0.1"),
            (3.141592653589793, "3.141592653589793"),
            (0.6931471805599453, "0.6931471805599453"),
            (2.302585092994046, "2.302585092994046"),
            (1.4426950408889634, "1.4426950408889634"),
            (0.4342944819032518, "0.4342944819032518"),
            (1.4142135623730951, "1.4142135623730951"),
            (0.7071067811865476, "0.7071067811865476"),
            (1.5707963267948966, "1.5707963267948966"),
            (0.7853981633974483, "0.7853981633974483"),
            (f64::INFINITY, "inf"),
            (f64::NEG_INFINITY, "-inf"),
            (0.0, "0.0"),
            (-6.0, "-6.0"),
            (7.5, "7.5"),
            (10.5, "10.5"),
            (2.5, "2.5"),
            (1024.0, "1024.0"),
            (27.0, "27.0"),
            (31.4159, "31.4159"),
            (25.0, "25.0"),
            (11.0, "11.0"),
            (6.283185307179586, "6.283185307179586"),
            (2.718281828459045, "2.718281828459045"),
        ];
        for &(val, expected) in cases {
            assert_eq!(
                format_f64(val),
                expected,
                "f64 {:?} should format as {:?}",
                val,
                expected
            );
        }
    }

    #[test]
    fn test_f32_display_matches_expected() {
        let cases: &[(f32, &str)] = &[
            (3.14_f32, "3.14"),
            (2.718_f32, "2.718"),
            (core::f32::consts::PI, "3.1415927"),
            (core::f32::consts::TAU, "6.2831855"),
            (core::f32::consts::E, "2.7182817"),
            (1.5_f32, "1.5"),
            (2.5_f32, "2.5"),
            (3.5_f32, "3.5"),
            (1.0_f32, "1.0"),
            (2.0_f32, "2.0"),
            (3.0_f32, "3.0"),
            (100.0_f32, "100.0"),
            (f32::INFINITY, "inf"),
            (f32::NEG_INFINITY, "-inf"),
            (0.0_f32, "0.0"),
        ];
        for &(val, expected) in cases {
            assert_eq!(
                format_f32(val),
                expected,
                "f32 {:?} should format as {:?}",
                val,
                expected
            );
        }
    }
}

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

// Exponential and logarithmic functions (f64)

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

// Power functions (f64)

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

// Exponential and logarithmic functions (f32)

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

// Power functions (f32)

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
