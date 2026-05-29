//! Small internal macros shared across the crate.

/// Declare a `pub` enum whose LSP wire form is a small `u32` discriminant.
///
/// Generates the same `serde` plumbing (`#[serde(into = "u32", try_from
/// = "u32")]`), the `From<Self> for u32` cast, and a `TryFrom<u32>`
/// impl whose `Err` is a human-readable diagnostic. The LSP wire types
/// `Severity` (1..=4) and `HighlightKind` (1..=3) used to spell these
/// impls out by hand — identical shape, drifty when one side gets a new
/// variant.
///
/// Usage:
///
/// ```ignore
/// lsp_repr_u32_enum!(Severity {
///     Error = 1,
///     Warning = 2,
/// });
/// ```
macro_rules! lsp_repr_u32_enum {
    (
        $(#[$enum_meta:meta])*
        $vis:vis enum $name:ident {
            $(
                $(#[$variant_meta:meta])*
                $variant:ident = $val:literal
            ),+ $(,)?
        }
    ) => {
        $(#[$enum_meta])*
        #[derive(
            Debug, Clone, Copy, PartialEq, Eq,
            ::serde::Serialize, ::serde::Deserialize,
        )]
        #[serde(into = "u32", try_from = "u32")]
        $vis enum $name {
            $(
                $(#[$variant_meta])*
                $variant = $val,
            )+
        }

        impl ::core::convert::From<$name> for u32 {
            fn from(value: $name) -> Self {
                value as Self
            }
        }

        impl ::core::convert::TryFrom<u32> for $name {
            // Spelled out explicitly because variants named `Error`
            // (e.g. `Severity::Error`) would otherwise make
            // `Self::Error` ambiguous with the associated type.
            type Error = ::std::string::String;

            fn try_from(
                n: u32,
            ) -> ::core::result::Result<
                Self,
                <Self as ::core::convert::TryFrom<u32>>::Error,
            > {
                match n {
                    $($val => ::core::result::Result::Ok(Self::$variant),)+
                    _ => ::core::result::Result::Err(format!(
                        concat!("invalid ", stringify!($name), ": {}"),
                        n,
                    )),
                }
            }
        }
    };
}

pub(crate) use lsp_repr_u32_enum;
