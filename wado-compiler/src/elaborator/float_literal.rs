//! A float literal rounded once, from its source text into its target format.

use std::cmp::Ordering;

use crate::elaborator::util::normalize_numeric_literal;
use crate::primitive::PrimitiveType;

/// A binary interchange format a float literal is written into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FloatFormat {
    exponent_bits: u32,
    mantissa_bits: u32,
    name: &'static str,
}

impl FloatFormat {
    pub(crate) const F64: Self = Self::new(11, 52, "f64");
    pub(crate) const F32: Self = Self::new(8, 23, "f32");
    pub(crate) const F16: Self = Self::new(5, 10, "f16");
    pub(crate) const BF16: Self = Self::new(8, 7, "bf16");

    const fn new(exponent_bits: u32, mantissa_bits: u32, name: &'static str) -> Self {
        Self {
            exponent_bits,
            mantissa_bits,
            name,
        }
    }

    /// The format `prim` stores, `None` for a primitive that is not a float.
    pub(crate) fn of(prim: PrimitiveType) -> Option<Self> {
        match prim {
            PrimitiveType::F64 => Some(Self::F64),
            PrimitiveType::F32 => Some(Self::F32),
            PrimitiveType::F16 => Some(Self::F16),
            PrimitiveType::Bf16 => Some(Self::BF16),
            PrimitiveType::I8
            | PrimitiveType::I16
            | PrimitiveType::I32
            | PrimitiveType::I64
            | PrimitiveType::U8
            | PrimitiveType::U16
            | PrimitiveType::U32
            | PrimitiveType::U64
            | PrimitiveType::Bool
            | PrimitiveType::Char
            | PrimitiveType::V128 => None,
        }
    }

    /// The sign bit, which a negated literal sets.
    pub(crate) fn sign_bit(self) -> u64 {
        1 << (self.exponent_bits + self.mantissa_bits)
    }

    fn bias(self) -> i64 {
        (1 << (self.exponent_bits - 1)) - 1
    }

    /// The value `bits` hold in `f32` or `f64`, widened to `f64`.
    pub(crate) fn value(self, bits: u64) -> f64 {
        match self {
            Self::F64 => f64::from_bits(bits),
            Self::F32 => f64::from(f32::from_bits(
                u32::try_from(bits).expect("f32 bits fit u32"),
            )),
            _ => unreachable!("`{}` has no float value of its own", self.name),
        }
    }
}

/// Why a literal has no bits in a format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FloatLiteralError {
    Invalid,
    OutOfRange(FloatFormat),
}

impl FloatLiteralError {
    /// The report for `literal`, spelled as the source writes it, sign included.
    pub(crate) fn message(self, literal: &str) -> String {
        match self {
            Self::Invalid => format!("invalid float literal: {literal}"),
            Self::OutOfRange(format) => {
                format!("literal out of range for `{}`: {literal}", format.name)
            }
        }
    }
}

/// `repr` in `format`'s bits, rounded to nearest even from the literal's exact
/// value, which a radix prefix spells as an integer of any width.
pub(crate) fn float_literal_bits(
    repr: &str,
    format: FloatFormat,
) -> Result<u64, FloatLiteralError> {
    let clean = normalize_numeric_literal(repr);
    let radix = [("0x", 16), ("0b", 2), ("0o", 8)]
        .into_iter()
        .find_map(|(prefix, radix)| clean.strip_prefix(prefix).map(|digits| (digits, radix)));
    let exact = match radix {
        Some((digits, radix)) => {
            radix_to_decimal(digits, radix).ok_or(FloatLiteralError::Invalid)?
        }
        None => clean,
    };
    let approx: f64 = exact.parse().map_err(|_| FloatLiteralError::Invalid)?;
    if approx.is_infinite() {
        return Err(FloatLiteralError::OutOfRange(format));
    }
    if format == FloatFormat::F64 {
        return Ok(approx.to_bits());
    }
    narrow(approx, format, || compare_decimal(&exact, approx))
        .ok_or(FloatLiteralError::OutOfRange(format))
}

/// The decimal spelling of the unsigned integer `digits` spell in `radix`.
fn radix_to_decimal(digits: &str, radix: u32) -> Option<String> {
    const BASE: u64 = 1_000_000_000;
    if digits.is_empty() {
        return None;
    }
    // Little-endian limbs of nine decimal digits each.
    let mut limbs: Vec<u64> = vec![0];
    for c in digits.chars() {
        let mut carry = u64::from(c.to_digit(radix)?);
        for limb in &mut limbs {
            let v = *limb * u64::from(radix) + carry;
            *limb = v % BASE;
            carry = v / BASE;
        }
        if carry != 0 {
            limbs.push(carry);
        }
    }
    let mut out = limbs.pop().expect("one limb at least").to_string();
    for limb in limbs.iter().rev() {
        out.push_str(&format!("{limb:09}"));
    }
    Some(out)
}

/// `value` rounded to nearest even in `format`, `None` at an infinity. `tie`
/// settles an exact midpoint, since `value` may itself round the literal.
fn narrow(value: f64, format: FloatFormat, tie: impl FnOnce() -> Ordering) -> Option<u64> {
    assert!(value >= 0.0, "a literal is unsigned until negated");
    let bits = value.to_bits();
    let field = (bits >> 52) as i64;
    let fraction = bits & ((1 << 52) - 1);
    let (m, e) = if field == 0 {
        (fraction, -1074)
    } else {
        (fraction | (1 << 52), field - 1075)
    };
    if m == 0 {
        return Some(0);
    }
    let mb = i64::from(format.mantissa_bits);
    let emin = 1 - format.bias();
    let magnitude = e + i64::from(m.ilog2());
    let lsb = (magnitude - mb).max(emin - mb);
    let shift = lsb - e;
    let mut q = if shift <= 0 {
        m << -shift
    } else if shift > 54 {
        0
    } else {
        let q = m >> shift;
        let rem = m & ((1 << shift) - 1);
        let round_up = match rem.cmp(&(1 << (shift - 1))) {
            Ordering::Greater => true,
            Ordering::Less => false,
            Ordering::Equal => match tie() {
                Ordering::Greater => true,
                Ordering::Less => false,
                Ordering::Equal => q & 1 == 1,
            },
        };
        q + u64::from(round_up)
    };
    let mut lsb = lsb;
    if q == 1 << (mb + 1) {
        q >>= 1;
        lsb += 1;
    }
    if q < 1 << mb {
        return Some(q);
    }
    let biased = lsb + mb + format.bias();
    if biased >= (1 << format.exponent_bits) - 1 {
        return None;
    }
    Some(((biased as u64) << mb) | (q - (1 << mb)))
}

/// The exact decimal `literal` against the exact value of `approx`.
fn compare_decimal(literal: &str, approx: f64) -> Ordering {
    // Precision past the longest f64 expansion (767 digits) prints it exactly.
    let expansion = format!("{approx:.800e}");
    let [a, b] = [literal, &expansion].map(scientific);
    a.1.cmp(&b.1).then_with(|| {
        let len = a.0.len().max(b.0.len());
        let pad = |d: &[u8]| {
            (0..len)
                .map(|i| d.get(i).copied().unwrap_or(b'0'))
                .collect::<Vec<_>>()
        };
        pad(&a.0).cmp(&pad(&b.0))
    })
}

/// A nonzero decimal as its significant digits and the power of ten of the
/// first one.
fn scientific(text: &str) -> (Vec<u8>, i64) {
    let (mantissa, exponent) = text.split_once('e').unwrap_or((text, "0"));
    let exponent: i64 = exponent
        .parse()
        .expect("a parsed literal's exponent is an integer");
    let (int, frac) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    let digits: Vec<u8> = int.bytes().chain(frac.bytes()).collect();
    let leading = digits.iter().take_while(|&&d| d == b'0').count();
    let mut significant = digits[leading..].to_vec();
    while significant.last() == Some(&b'0') {
        significant.pop();
    }
    assert!(!significant.is_empty(), "a tie is never at zero");
    (
        significant,
        int.len() as i64 - leading as i64 - 1 + exponent,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bits(repr: &str, format: FloatFormat) -> u64 {
        float_literal_bits(repr, format).unwrap()
    }

    #[test]
    fn a_literal_rounds_once_from_its_decimal() {
        // f64 rounds this to f32's midpoint between 1 and its successor, and a
        // second rounding would take the even side, 1.
        assert_eq!(
            bits("1.00000005960464477539062500000000001", FloatFormat::F32),
            0x3F80_0001
        );
        assert_eq!(
            bits("1.000000059604644775390625", FloatFormat::F32),
            0x3F80_0000
        );
        assert_eq!(
            bits("1.00000005960464477539062499999999999", FloatFormat::F32),
            0x3F80_0000
        );
        // The same at binary16: the midpoint above 1 is 1 + 2^-11.
        assert_eq!(
            bits("1.00048828125000000000000000000000001", FloatFormat::F16),
            0x3C01
        );
        assert_eq!(bits("1.00048828125", FloatFormat::F16), 0x3C00);
    }

    #[test]
    fn every_class_lands_on_its_reference_bits() {
        assert_eq!(bits("0.0", FloatFormat::F16), 0x0000);
        assert_eq!(bits("1.0", FloatFormat::F16), 0x3C00);
        assert_eq!(bits("65504.0", FloatFormat::F16), 0x7BFF);
        assert_eq!(bits("5.9604645e-8", FloatFormat::F16), 0x0001);
        assert_eq!(bits("6.1035156e-5", FloatFormat::F16), 0x0400);
        assert_eq!(bits("1e-10", FloatFormat::F16), 0x0000);
        assert_eq!(bits("2049", FloatFormat::F16), 0x6800);
        assert_eq!(bits("2051", FloatFormat::F16), 0x6802);
        assert_eq!(bits("0x10", FloatFormat::F16), 0x4C00);
        assert_eq!(bits("3.14159265", FloatFormat::BF16), 0x4049);
        assert_eq!(bits("1e39", FloatFormat::F64), 1e39_f64.to_bits());
        assert_eq!(bits("1e38", FloatFormat::BF16), 0x7E96);
    }

    #[test]
    fn a_radix_literal_is_as_wide_as_its_digits() {
        assert_eq!(radix_to_decimal("ff", 16).as_deref(), Some("255"));
        assert_eq!(
            radix_to_decimal("100000000000000000000000000000000", 16).as_deref(),
            Some("340282366920938463463374607431768211456")
        );
        assert_eq!(radix_to_decimal("12", 2), None);
    }

    #[test]
    fn a_literal_past_the_largest_finite_value_is_refused() {
        assert!(float_literal_bits("65520.0", FloatFormat::F16).is_err());
        assert_eq!(bits("65519.99", FloatFormat::F16), 0x7BFF);
        assert!(float_literal_bits("1e39", FloatFormat::F32).is_err());
        assert!(float_literal_bits("1e400", FloatFormat::F64).is_err());
    }
}
