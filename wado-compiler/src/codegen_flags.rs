//! Fine-grained codegen feature flags.
//!
//! These toggle individual codegen strategies that we want to be able to
//! switch on and off without rebuilding the toolchain — primarily so we can
//! A/B them under the benchmark suite. The CLI exposes them through the
//! generic `-f <flag>` option (see `wado-cli`), which forwards the raw flag
//! strings to [`CompilerOptions::codegen_flags`](crate::CompilerOptions); the
//! compiler then parses them into this typed struct via [`CodegenFlags::parse`].

/// Codegen feature flags toggled from the CLI via `-f <flag>`.
///
/// Every field defaults to `false`, i.e. the compiler's established codegen
/// behaviour. A flag only ever *opts into* an alternative strategy, so an
/// empty flag set reproduces the default output exactly.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CodegenFlags {
    /// Lower `builtin::array_copy` to the native Wasm `array.copy` instruction
    /// instead of the default open-coded element-wise loop.
    ///
    /// The loop is the default because wasmtime's `array.copy` runtime path
    /// used to be markedly slower than an inlined loop for the short copies
    /// that dominate Wado workloads. A recent wasmtime patch improves that
    /// path, but it is unclear whether it wins for our access patterns, so
    /// this flag (`-f array-copy`) lets us re-measure both strategies under
    /// the benchmark suite. See the `WirInstr::ArrayCopy` emitter in
    /// `codegen/emit.rs`.
    pub array_copy: bool,
}

impl CodegenFlags {
    /// Parse raw `-f` flag strings into a [`CodegenFlags`].
    ///
    /// Flags follow the clang-style convention: `name` enables a flag and
    /// `no-name` disables it (so a later `-f no-array-copy` can override an
    /// earlier `-f array-copy`). An unrecognized flag yields `Err(flag)`,
    /// carrying the offending string so the caller can surface a diagnostic.
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
    fn defaults_to_all_off() {
        assert_eq!(
            CodegenFlags::parse(std::iter::empty::<&str>()),
            Ok(CodegenFlags::default())
        );
        assert!(!CodegenFlags::default().array_copy);
    }

    #[test]
    fn enables_array_copy() {
        let flags = CodegenFlags::parse(["array-copy"]).unwrap();
        assert!(flags.array_copy);
    }

    #[test]
    fn no_prefix_disables_and_last_wins() {
        let flags = CodegenFlags::parse(["array-copy", "no-array-copy"]).unwrap();
        assert!(!flags.array_copy);
        let flags = CodegenFlags::parse(["no-array-copy", "array-copy"]).unwrap();
        assert!(flags.array_copy);
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
