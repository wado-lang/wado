//! Bundled float-to-string conversion for Wado runtime.
//!
//! This crate compiles to Wasm P1 format for static linking with Wado-generated code.
//! Uses fpfmt for deterministic float-to-string conversion.

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


/// Write the decimal digits of `d` into `buf[0..nd]`.
fn write_digits(buf: &mut [u8], mut d: u64, nd: usize) {
    for i in (0..nd).rev() {
        buf[i] = b'0' + (d % 10) as u8;
        d /= 10;
    }
}

/// Format `(d, p)` in exponential notation: `d.ddde[-]ee` or `d.dddE[-]ee`
///
/// `exp_char` is the exponent character (b'e' or b'E').
/// Returns number of bytes written.
fn fmt_exp(buf: &mut [u8], d: u64, p: i32, nd: i32, exp_char: u8) -> usize {
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
    buf[pos] = exp_char;
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
        return fmt_exp(buf, d, p, nd, b'e');
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
            // Mixed integer and fractional: e.g., (123, -2) -> "1.23"
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
            // Pure fraction: e.g., (1, -1) -> "0.1", (35, -3) -> "0.035"
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
/// each candidate via `parse` -> `as f32` round-trip.
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


/// Format an f64 using shortest representation into `buf`.
///
/// Returns number of bytes written. Buffer must be at least 32 bytes.
#[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
fn f64_fmt_shortest(value: f64, buf: &mut [u8]) -> usize {
    if value != value {
        buf[..3].copy_from_slice(b"NaN");
        return 3;
    }
    if value == f64::INFINITY {
        buf[..3].copy_from_slice(b"inf");
        return 3;
    }
    if value == f64::NEG_INFINITY {
        buf[..4].copy_from_slice(b"-inf");
        return 4;
    }
    if value == 0.0 {
        if value.to_bits() >> 63 != 0 {
            buf[..4].copy_from_slice(b"-0.0");
            return 4;
        }
        buf[..3].copy_from_slice(b"0.0");
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
    pos + len
}

/// Format an f32 using shortest representation into `buf`.
///
/// Returns number of bytes written. Buffer must be at least 24 bytes.
#[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
fn f32_fmt_shortest(value: f32, buf: &mut [u8]) -> usize {
    if value != value {
        buf[..3].copy_from_slice(b"NaN");
        return 3;
    }
    if value == f32::INFINITY {
        buf[..3].copy_from_slice(b"inf");
        return 3;
    }
    if value == f32::NEG_INFINITY {
        buf[..4].copy_from_slice(b"-inf");
        return 4;
    }
    if value == 0.0 {
        if value.to_bits() >> 31 != 0 {
            buf[..4].copy_from_slice(b"-0.0");
            return 4;
        }
        buf[..3].copy_from_slice(b"0.0");
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
    pos + len
}

/// Format an f64 with exactly `precision` decimal places into `buf`.
///
/// Returns number of bytes written. Buffer must be at least 400 bytes.
#[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
fn f64_fmt_fixed(value: f64, precision: i32, buf: &mut [u8]) -> usize {
    if value != value {
        buf[..3].copy_from_slice(b"NaN");
        return 3;
    }
    if value == f64::INFINITY {
        buf[..3].copy_from_slice(b"inf");
        return 3;
    }
    if value == f64::NEG_INFINITY {
        buf[..4].copy_from_slice(b"-inf");
        return 4;
    }
    if value == 0.0 {
        let mut pos = 0;
        if value.to_bits() >> 63 != 0 {
            buf[0] = b'-';
            pos = 1;
        }
        buf[pos] = b'0';
        buf[pos + 1] = b'.';
        let prec = precision as usize;
        let mut i = 0;
        while i < prec {
            buf[pos + 2 + i] = b'0';
            i += 1;
        }
        return pos + 2 + prec;
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
        let needed = precision + (-int_len);
        if needed <= 0 { 1 } else { needed.min(18) }
    } else {
        let needed = int_len + precision;
        needed.min(18)
    };

    let nd_needed = nd_needed.max(1);

    let (d, p) = if nd_needed <= nd_short {
        fpfmt::fixed_width(f, nd_needed)
    } else if nd_needed > 17 {
        fpfmt::fixed_width(f, 17.min(nd_needed))
    } else {
        fpfmt::fixed_width(f, nd_needed)
    };
    let nd = fpfmt::digits(d);

    let len = fmt_fixed(&mut buf[pos..], d, p, nd, precision);
    pos + len
}

/// Format an f64 in exponential notation into `buf`.
///
/// If `precision < 0`, use shortest representation.
/// If `precision >= 0`, use `precision + 1` significant digits.
/// If `upper` is nonzero, use 'E' instead of 'e'.
///
/// Returns number of bytes written. Buffer must be at least 32 bytes.
#[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
fn f64_fmt_exp(value: f64, precision: i32, upper: bool, buf: &mut [u8]) -> usize {
    if value != value {
        buf[..3].copy_from_slice(b"NaN");
        return 3;
    }
    if value == f64::INFINITY {
        buf[..3].copy_from_slice(b"inf");
        return 3;
    }
    if value == f64::NEG_INFINITY {
        buf[..4].copy_from_slice(b"-inf");
        return 4;
    }
    let ec = if upper { b'E' } else { b'e' };

    if value == 0.0 {
        let mut pos = 0;
        if value.to_bits() >> 63 != 0 {
            buf[0] = b'-';
            pos = 1;
        }
        if precision < 0 {
            buf[pos] = b'0';
            buf[pos + 1] = ec;
            buf[pos + 2] = b'0';
            return pos + 3;
        }
        buf[pos] = b'0';
        if precision > 0 {
            buf[pos + 1] = b'.';
            let prec = precision as usize;
            let mut i = 0;
            while i < prec {
                buf[pos + 2 + i] = b'0';
                i += 1;
            }
            buf[pos + 2 + prec] = ec;
            buf[pos + 3 + prec] = b'0';
            return pos + 4 + prec;
        }
        // precision == 0: "0e0"
        buf[pos] = b'0';
        buf[pos + 1] = ec;
        buf[pos + 2] = b'0';
        return pos + 3;
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
        let (d, p) = fpfmt::short(f);
        let nd = fpfmt::digits(d);
        (d, p, nd)
    } else {
        let n = (precision + 1).min(18).max(1);
        let (d, p) = fpfmt::fixed_width(f, n);
        let nd = fpfmt::digits(d);
        (d, p, nd)
    };

    let len = fmt_exp(&mut buf[pos..], d, p, nd, ec);
    pos + len
}


/// Format an f64 value using shortest representation to the provided buffer.
///
/// # Safety
/// The buffer must be at least 32 bytes.
#[unsafe(no_mangle)]
pub extern "C" fn f64_to_buffer(value: f64, buffer_ptr: i32) -> i32 {
    let mut buf = [0u8; 32];
    let len = f64_fmt_shortest(value, &mut buf);
    unsafe { copy_to_ptr(buffer_ptr, &buf[..len]) };
    len as i32
}

/// Format an f32 value using shortest representation to the provided buffer.
///
/// # Safety
/// The buffer must be at least 24 bytes.
#[unsafe(no_mangle)]
pub extern "C" fn f32_to_buffer(value: f32, buffer_ptr: i32) -> i32 {
    let mut buf = [0u8; 24];
    let len = f32_fmt_shortest(value, &mut buf);
    unsafe { copy_to_ptr(buffer_ptr, &buf[..len]) };
    len as i32
}

/// Format an f64 value with fixed-point precision to the provided buffer.
///
/// # Safety
/// The buffer must be at least 400 bytes.
#[unsafe(no_mangle)]
#[allow(clippy::cast_possible_wrap)]
pub extern "C" fn f64_to_buffer_fixed(value: f64, precision: i32, buffer_ptr: i32) -> i32 {
    let mut buf = [0u8; 400];
    let len = f64_fmt_fixed(value, precision, &mut buf);
    unsafe { copy_to_ptr(buffer_ptr, &buf[..len]) };
    len as i32
}

/// Format an f32 value with fixed-point precision to the provided buffer.
///
/// # Safety
/// The buffer must be at least 64 bytes.
#[unsafe(no_mangle)]
#[allow(clippy::cast_possible_wrap)]
pub extern "C" fn f32_to_buffer_fixed(value: f32, precision: i32, buffer_ptr: i32) -> i32 {
    let mut buf = [0u8; 400];
    let len = f64_fmt_fixed(f64::from(value), precision, &mut buf);
    unsafe { copy_to_ptr(buffer_ptr, &buf[..len]) };
    len as i32
}

/// Format an f64 value in exponential notation to the provided buffer.
///
/// If `upper` is nonzero, use 'E' instead of 'e'.
///
/// # Safety
/// The buffer must be at least 32 bytes.
#[unsafe(no_mangle)]
#[allow(clippy::cast_possible_wrap)]
pub extern "C" fn f64_to_buffer_exp(
    value: f64,
    precision: i32,
    upper: i32,
    buffer_ptr: i32,
) -> i32 {
    let mut buf = [0u8; 32];
    let len = f64_fmt_exp(value, precision, upper != 0, &mut buf);
    unsafe { copy_to_ptr(buffer_ptr, &buf[..len]) };
    len as i32
}

/// Format an f32 value in exponential notation to the provided buffer.
///
/// If `upper` is nonzero, use 'E' instead of 'e'.
///
/// # Safety
/// The buffer must be at least 24 bytes.
#[unsafe(no_mangle)]
#[allow(clippy::cast_possible_wrap)]
pub extern "C" fn f32_to_buffer_exp(
    value: f32,
    precision: i32,
    upper: i32,
    buffer_ptr: i32,
) -> i32 {
    let mut buf = [0u8; 32];
    let len = f64_fmt_exp(f64::from(value), precision, upper != 0, &mut buf);
    unsafe { copy_to_ptr(buffer_ptr, &buf[..len]) };
    len as i32
}


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

#[cfg(target_arch = "wasm32")]
#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    core::arch::wasm32::unreachable();
}
