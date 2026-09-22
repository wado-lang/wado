//! The primitive types, named the same way at every layer of the compiler.

/// A type no module declares, whose representation the compiler knows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PrimitiveType {
    I8,
    I16,
    I32,
    I64,
    U8,
    U16,
    U32,
    U64,
    F32,
    F64,
    /// IEEE 754 binary16. Bits only: Wasm has no half precision instruction.
    F16,
    /// The top 16 bits of an `f32`. Bits only, as [`Self::F16`] is.
    Bf16,
    Bool,
    Char,
    V128,
}

impl PrimitiveType {
    /// Every variant. `i128` and `u128` are absent: they are prelude struct
    /// declarations, which write their own operator impls.
    pub const ALL: &'static [Self] = &[
        Self::I8,
        Self::I16,
        Self::I32,
        Self::I64,
        Self::U8,
        Self::U16,
        Self::U32,
        Self::U64,
        Self::F32,
        Self::F64,
        Self::F16,
        Self::Bf16,
        Self::Bool,
        Self::Char,
        Self::V128,
    ];

    /// The name this type is spelled with, such as `i32` or `bf16`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::I8 => "i8",
            Self::I16 => "i16",
            Self::I32 => "i32",
            Self::I64 => "i64",
            Self::U8 => "u8",
            Self::U16 => "u16",
            Self::U32 => "u32",
            Self::U64 => "u64",
            Self::F32 => "f32",
            Self::F64 => "f64",
            Self::F16 => "f16",
            Self::Bf16 => "bf16",
            Self::Bool => "bool",
            Self::Char => "char",
            Self::V128 => "v128",
        }
    }

    /// True for the half precision types, `f16` and `bf16`.
    #[must_use]
    pub fn is_half(self) -> bool {
        matches!(self, Self::F16 | Self::Bf16)
    }

    /// The largest value a scalar integer holds; `None` for every other
    /// primitive.
    #[must_use]
    pub fn int_max(self) -> Option<u128> {
        Some(match self {
            Self::I8 => i8::MAX as u128,
            Self::I16 => i16::MAX as u128,
            Self::I32 => i32::MAX as u128,
            Self::I64 => i64::MAX as u128,
            Self::U8 => u128::from(u8::MAX),
            Self::U16 => u128::from(u16::MAX),
            Self::U32 => u128::from(u32::MAX),
            Self::U64 => u128::from(u64::MAX),
            Self::F32
            | Self::F64
            | Self::F16
            | Self::Bf16
            | Self::Bool
            | Self::Char
            | Self::V128 => return None,
        })
    }

    /// The primitive `name` spells, `None` for every other name.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|p| p.as_str() == name)
    }

    /// True where `name` spells one.
    #[must_use]
    pub fn is_primitive_name(name: &str) -> bool {
        Self::from_name(name).is_some()
    }

    /// Every name this enum spells.
    #[must_use]
    pub fn all_primitive_names() -> Vec<&'static str> {
        Self::ALL.iter().copied().map(Self::as_str).collect()
    }
}
