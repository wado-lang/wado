//! Fine-grained codegen feature flags.
//!
//! These toggle individual codegen strategies that we want to be able to
//! switch on and off without rebuilding the toolchain — primarily so we can
//! A/B them under the benchmark suite. The CLI exposes them through the
//! generic `-f <flag>` option (see `wado-cli`), which forwards the raw flag
//! strings to [`CompilerOptions::codegen_flags`](crate::CompilerOptions); the
//! compiler then parses them into this typed struct via [`CodegenFlags::parse`].
//!
//! Each flag is a plain boolean. A leading `no-` on the flag name inverts it,
//! so a flag that is on by default can be turned off with `-f no-<flag>`.

/// Codegen feature flags toggled from the CLI via `-f <flag>`.
///
/// Unlike a plain `#[derive(Default)]`, the default here is *not* uniformly
/// `false`: each field's default encodes the compiler's current preferred
/// codegen strategy. `-f <flag>` forces it on and `-f no-<flag>` forces it
/// off, so an empty flag set reproduces [`CodegenFlags::default`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CodegenFlags {
    /// Lower `builtin::array_copy` to the native Wasm `array.copy` instruction
    /// (the default) instead of an open-coded element-wise loop.
    ///
    /// We originally defaulted to the loop because wasmtime's `array.copy`
    /// runtime path was markedly slower than an inlined loop for short copies.
    /// After a wasmtime patch improved that path, benchmarking showed the
    /// native instruction wins big on copy-heavy workloads (zlib decompress
    /// ~+41%, syntax-highlight ~+10%) and is neutral elsewhere (compress,
    /// JSON parsing, float formatting all within noise), so the native
    /// instruction is now the default. Pass `-f no-array-copy` to restore the
    /// open-coded loop. See the `WirInstr::ArrayCopy` emitter in
    /// `codegen/emit.rs`.
    pub array_copy: bool,
}

impl Default for CodegenFlags {
    fn default() -> Self {
        Self { array_copy: true }
    }
}

impl CodegenFlags {
    /// Parse raw `-f` flag strings into a [`CodegenFlags`], starting from the
    /// defaults and applying each flag in order.
    ///
    /// Flags follow the clang-style convention: `name` enables a flag and
    /// `no-name` disables it (so `-f no-array-copy` overrides the on-by-default
    /// `array_copy`, and a later flag wins over an earlier one). An
    /// unrecognized flag yields `Err(flag)`, carrying the offending string so
    /// the caller can surface a diagnostic.
    pub fn parse<I, S>(flags: I) -> Result<Self, String>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut result = Self::default();
        for flag in flags {
            let flag = flag.as_ref();
            let (name, enabled) = match flag.strip_prefix("no-") {
                Some(rest) => (rest, false),
                None => (flag, true),
            };
            match name {
                "array-copy" => result.array_copy = enabled,
                _ => return Err(flag.to_string()),
            }
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_flags_reproduce_the_defaults() {
        assert_eq!(
            CodegenFlags::parse(std::iter::empty::<&str>()),
            Ok(CodegenFlags::default())
        );
        // array.copy is on by default.
        assert!(CodegenFlags::default().array_copy);
    }

    #[test]
    fn no_prefix_disables_an_on_by_default_flag() {
        let flags = CodegenFlags::parse(["no-array-copy"]).unwrap();
        assert!(!flags.array_copy);
    }

    #[test]
    fn explicit_enable_still_works_and_last_wins() {
        // `-f array-copy` is redundant with the default but remains valid.
        assert!(CodegenFlags::parse(["array-copy"]).unwrap().array_copy);
        // The last flag wins when both spellings appear.
        assert!(
            !CodegenFlags::parse(["array-copy", "no-array-copy"])
                .unwrap()
                .array_copy
        );
        assert!(
            CodegenFlags::parse(["no-array-copy", "array-copy"])
                .unwrap()
                .array_copy
        );
    }

    #[test]
    fn unknown_flag_is_reported_verbatim() {
        assert_eq!(CodegenFlags::parse(["bogus"]), Err("bogus".to_string()));
        // The `no-` prefix is stripped for matching but the error echoes the
        // original spelling so the user sees exactly what they typed.
        assert_eq!(
            CodegenFlags::parse(["no-bogus"]),
            Err("no-bogus".to_string())
        );
    }
}
