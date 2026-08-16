//! Fine-grained codegen feature flags.
//!
//! These toggle individual codegen strategies without rebuilding the toolchain,
//! primarily to A/B them under the benchmark suite. The CLI's `-f <flag>`
//! forwards raw strings to [`CompilerOptions::codegen_flags`](crate::CompilerOptions),
//! which [`CodegenFlags::parse`] reads into this struct. Each flag is a boolean,
//! and a leading `no-` inverts it.

/// Codegen feature flags toggled from the CLI via `-f <flag>`.
///
/// Unlike a plain `#[derive(Default)]`, the default here is *not* uniformly
/// `false`: each field's default encodes the compiler's current preferred
/// codegen strategy. `-f <flag>` forces it on and `-f no-<flag>` forces it
/// off, so an empty flag set reproduces [`CodegenFlags::default`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CodegenFlags {
    /// Lower `builtin::array_copy` to the native Wasm `array.copy` instruction
    /// (the default) instead of an open-coded element-wise loop. The loop was
    /// once faster for short copies; since a wasmtime patch, the instruction
    /// wins big on copy-heavy workloads (zlib decompress ~+41%,
    /// syntax-highlight ~+10%) and is neutral elsewhere.
    pub array_copy: bool,

    /// Emit `metadata.code.branch_hint` entries (the default);
    /// `-f no-branch-hinting` benchmarks without them, lowering
    /// `builtin::cold_path()` to a no-op and skipping trap-based inference. The
    /// markers are dropped at WIR build, not NIR, so the inliner's cold-path
    /// cost exclusion is unchanged and the A/B isolates the hints themselves.
    pub branch_hinting: bool,

    /// Lower an assertion failure to a bare `unreachable` trap instead of the
    /// power-assert diagnostic. The check and trap always stay; only the
    /// *message* goes, taking with it the `Formatter` / `Inspect` / `String`
    /// stack that even a `list[i]` drags in. Off at `-O0`…`-O3`, **on at `-Os`**
    /// (see [`CodegenFlags::for_opt_level`]).
    pub bare_asserts: bool,

    /// Emit native Wasm wide-arithmetic (`i64.mul_wide_u/s`, `i64.add128`,
    /// `i64.sub128`) — the default, best on wasmtime. `-f no-wide-arithmetic`
    /// open-codes them as 32-bit-limb i64 sequences
    /// (`codegen/emit/wide_arith_downlevel.rs`) for V8, which lacks the proposal.
    pub wide_arithmetic: bool,
}

impl Default for CodegenFlags {
    fn default() -> Self {
        Self {
            array_copy: true,
            branch_hinting: true,
            bare_asserts: false,
            wide_arithmetic: true,
        }
    }
}

impl CodegenFlags {
    /// The opt-level-dependent defaults, before any `-f` flag is applied.
    ///
    /// Identical to [`CodegenFlags::default`] except `-Os` flips
    /// [`bare_asserts`](Self::bare_asserts) on: a size-optimized build drops the
    /// power-assert diagnostic by default (an `-f no-bare-asserts` overrides it).
    #[must_use]
    pub fn for_opt_level(opt_level: crate::OptLevel) -> Self {
        Self {
            bare_asserts: matches!(opt_level, crate::OptLevel::Os),
            ..Self::default()
        }
    }

    /// Parse raw `-f` flag strings into a [`CodegenFlags`], starting from the
    /// [`for_opt_level`](Self::for_opt_level) defaults and applying each flag in
    /// order.
    ///
    /// Flags follow the clang-style convention: `name` enables a flag and
    /// `no-name` disables it (so `-f no-array-copy` overrides the on-by-default
    /// `array_copy`, and a later flag wins over an earlier one). An
    /// unrecognized flag yields `Err(flag)`, carrying the offending string so
    /// the caller can surface a diagnostic.
    /// Every flag [`Self::parse`] accepts, in help-text order. The single
    /// source of truth for the CLI's `-f` help and [`Self::unknown_flag_message`],
    /// so a new flag cannot be added and left undiscoverable.
    pub const SUPPORTED: &'static [&'static str] = &[
        "array-copy",
        "branch-hinting",
        "bare-asserts",
        "wide-arithmetic",
    ];

    /// The diagnostic for a flag [`Self::parse`] rejected.
    #[must_use]
    pub fn unknown_flag_message(flag: &str) -> String {
        let supported = Self::SUPPORTED
            .iter()
            .map(|f| format!("`{f}`"))
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "unknown codegen flag: `-f {flag}` (supported: {supported}, \
             optionally prefixed with `no-`)"
        )
    }

    pub fn parse<I, S>(flags: I, opt_level: crate::OptLevel) -> Result<Self, String>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut result = Self::for_opt_level(opt_level);
        for flag in flags {
            let flag = flag.as_ref();
            let (name, enabled) = match flag.strip_prefix("no-") {
                Some(rest) => (rest, false),
                None => (flag, true),
            };
            match name {
                "array-copy" => result.array_copy = enabled,
                "branch-hinting" => result.branch_hinting = enabled,
                "bare-asserts" => result.bare_asserts = enabled,
                "wide-arithmetic" => result.wide_arithmetic = enabled,
                _ => return Err(flag.to_string()),
            }
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::OptLevel;

    /// Parse at `-O2` (the level whose opt-level defaults equal
    /// [`CodegenFlags::default`]), so these cases isolate flag handling.
    fn parse<'a, I: IntoIterator<Item = &'a str>>(flags: I) -> Result<CodegenFlags, String> {
        CodegenFlags::parse(flags, OptLevel::O2)
    }

    #[test]
    fn every_advertised_flag_parses_both_ways() {
        for name in CodegenFlags::SUPPORTED {
            assert!(parse([*name]).is_ok(), "`-f {name}` was rejected");
            assert!(
                parse([format!("no-{name}").as_str()]).is_ok(),
                "`-f no-{name}` was rejected"
            );
        }
    }

    #[test]
    fn an_unknown_flag_names_every_supported_one() {
        let flag = parse(["nope"]).unwrap_err();
        let message = CodegenFlags::unknown_flag_message(&flag);
        for name in CodegenFlags::SUPPORTED {
            assert!(message.contains(name), "{message} omits `{name}`");
        }
    }

    #[test]
    fn empty_flags_reproduce_the_defaults() {
        assert_eq!(parse(std::iter::empty()), Ok(CodegenFlags::default()));
        // array.copy and branch hinting are on by default; bare-asserts off.
        assert!(CodegenFlags::default().array_copy);
        assert!(CodegenFlags::default().branch_hinting);
        assert!(!CodegenFlags::default().bare_asserts);
    }

    #[test]
    fn os_enables_bare_asserts_by_default() {
        // `-Os` flips bare-asserts on without an explicit flag; other levels
        // leave it off.
        assert!(CodegenFlags::for_opt_level(OptLevel::Os).bare_asserts);
        assert!(!CodegenFlags::for_opt_level(OptLevel::O2).bare_asserts);
        assert!(!CodegenFlags::for_opt_level(OptLevel::O0).bare_asserts);
        // The opt-level default still folds the array.copy / branch-hinting ons.
        assert!(CodegenFlags::for_opt_level(OptLevel::Os).array_copy);
    }

    #[test]
    fn no_bare_asserts_overrides_the_os_default() {
        let flags = CodegenFlags::parse(["no-bare-asserts"], OptLevel::Os).unwrap();
        assert!(!flags.bare_asserts);
    }

    #[test]
    fn bare_asserts_forces_it_on_below_os() {
        let flags = CodegenFlags::parse(["bare-asserts"], OptLevel::O2).unwrap();
        assert!(flags.bare_asserts);
    }

    #[test]
    fn no_branch_hinting_disables_the_default() {
        let flags = parse(["no-branch-hinting"]).unwrap();
        assert!(!flags.branch_hinting);
        // Other flags keep their defaults.
        assert!(flags.array_copy);
    }

    #[test]
    fn no_prefix_disables_an_on_by_default_flag() {
        let flags = parse(["no-array-copy"]).unwrap();
        assert!(!flags.array_copy);
    }

    #[test]
    fn explicit_enable_still_works_and_last_wins() {
        // `-f array-copy` is redundant with the default but remains valid.
        assert!(parse(["array-copy"]).unwrap().array_copy);
        // The last flag wins when both spellings appear.
        assert!(!parse(["array-copy", "no-array-copy"]).unwrap().array_copy);
        assert!(parse(["no-array-copy", "array-copy"]).unwrap().array_copy);
    }

    #[test]
    fn unknown_flag_is_reported_verbatim() {
        assert_eq!(parse(["bogus"]), Err("bogus".to_string()));
        // The `no-` prefix is stripped for matching but the error echoes the
        // original spelling so the user sees exactly what they typed.
        assert_eq!(parse(["no-bogus"]), Err("no-bogus".to_string()));
    }
}
