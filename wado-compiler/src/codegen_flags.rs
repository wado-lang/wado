//! Fine-grained codegen feature flags.
//!
//! These toggle individual codegen strategies without rebuilding the toolchain,
//! primarily to A/B them under the benchmark suite. The CLI's `-f <flag>`
//! forwards raw strings to [`CompilerOptions::codegen_flags`](crate::CompilerOptions),
//! which [`CodegenFlags::parse`] reads into this struct. Each flag is a boolean,
//! and a leading `no-` inverts it.

use crate::OptLevel;

/// Codegen feature flags toggled from the CLI via `-f <flag>`.
///
/// Unlike a plain `#[derive(Default)]`, the default here is *not* uniformly
/// `false`: each field's default encodes the compiler's current preferred
/// codegen strategy. `-f <flag>` forces it on and `-f no-<flag>` forces it
/// off, so an empty flag set reproduces [`CodegenFlags::for_build`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CodegenFlags {
    /// Emit `metadata.code.branch_hint` entries (the default);
    /// `-f no-branch-hinting` benchmarks without them. Only the emitter reads
    /// it: every pass sees the same markers and hints either way, so the A/B
    /// isolates what the hints are worth to the engine.
    pub branch_hinting: bool,

    /// Keep the cold markers (the default); `-f no-cold-paths` drops every one
    /// before any pass reads it, the compiler's included, to measure what they
    /// are worth to the whole pipeline. The hints `infer_branch_hints` derives
    /// from traps stay.
    pub cold_paths: bool,

    /// Lower an assertion failure to a bare `unreachable` trap instead of the
    /// power-assert diagnostic. The check and trap always stay; only the
    /// *message* goes, taking with it the `Formatter` / `Inspect` / `String`
    /// stack that even a `list[i]` drags in. Off at `-O0`…`-O3`, **on at `-Os`**
    /// (see [`CodegenFlags::for_build`]).
    pub bare_asserts: bool,

    /// Emit native Wasm wide-arithmetic (`i64.mul_wide_u/s`, `i64.add128`,
    /// `i64.sub128`) — the default, best on wasmtime. `-f no-wide-arithmetic`
    /// calls their `core:rt` software forms instead (`lower::wide_arith`), for
    /// V8, which lacks the proposal.
    pub wide_arithmetic: bool,

    /// Check the contract of a call marked `unchecked`: `lower::contract_checks`
    /// keeps or deletes the checks of each `unchecked` twin. An unmarked call
    /// checks in every build. On in the test world and at `-O0` and `-O1`, off
    /// otherwise (see [`CodegenFlags::for_build`]).
    pub contract_checks: bool,
}

impl Default for CodegenFlags {
    fn default() -> Self {
        Self {
            branch_hinting: true,
            cold_paths: true,
            bare_asserts: false,
            wide_arithmetic: true,
            contract_checks: false,
        }
    }
}

impl CodegenFlags {
    /// The defaults for a build, before any `-f` flag is applied.
    ///
    /// Identical to [`CodegenFlags::default`] except that `-Os` turns
    /// [`bare_asserts`](Self::bare_asserts) on, and the test world, `-O0` or
    /// `-O1` turns [`contract_checks`](Self::contract_checks) on.
    #[must_use]
    pub fn for_build(opt_level: OptLevel, test_world: bool) -> Self {
        Self {
            bare_asserts: matches!(opt_level, OptLevel::Os),
            contract_checks: test_world || matches!(opt_level, OptLevel::O0 | OptLevel::O1),
            ..Self::default()
        }
    }

    /// Every flag [`Self::parse`] accepts, in help-text order. The CLI's `-f`
    /// help is tested against it and [`Self::unknown_flag_message`] reads it,
    /// so a new flag cannot be added and left undiscoverable.
    pub const SUPPORTED: &'static [&'static str] = &[
        "branch-hinting",
        "cold-paths",
        "bare-asserts",
        "wide-arithmetic",
        "contract-checks",
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

    /// Parse raw `-f` flag strings into a [`CodegenFlags`], starting from the
    /// [`for_build`](Self::for_build) defaults and applying each flag in order.
    ///
    /// Flags follow the clang-style convention: `name` enables a flag and
    /// `no-name` disables it, and a later flag wins over an earlier one. An
    /// unrecognized flag yields `Err(flag)`, carrying the offending string so
    /// the caller can surface a diagnostic.
    pub fn parse<I, S>(flags: I, opt_level: OptLevel, test_world: bool) -> Result<Self, String>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut result = Self::for_build(opt_level, test_world);
        for flag in flags {
            let flag = flag.as_ref();
            let (name, enabled) = match flag.strip_prefix("no-") {
                Some(rest) => (rest, false),
                None => (flag, true),
            };
            match name {
                "branch-hinting" => result.branch_hinting = enabled,
                "cold-paths" => result.cold_paths = enabled,
                "bare-asserts" => result.bare_asserts = enabled,
                "wide-arithmetic" => result.wide_arithmetic = enabled,
                "contract-checks" => result.contract_checks = enabled,
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

    /// Parse at `-O2` outside the test world (the build whose defaults equal
    /// [`CodegenFlags::default`]), so these cases isolate flag handling.
    fn parse<'a, I: IntoIterator<Item = &'a str>>(flags: I) -> Result<CodegenFlags, String> {
        CodegenFlags::parse(flags, OptLevel::O2, false)
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
        // Branch hinting and wide arithmetic are on by default; bare-asserts off.
        assert!(CodegenFlags::default().branch_hinting);
        assert!(CodegenFlags::default().wide_arithmetic);
        assert!(!CodegenFlags::default().bare_asserts);
    }

    #[test]
    fn os_enables_bare_asserts_by_default() {
        // `-Os` flips bare-asserts on without an explicit flag; other levels
        // leave it off.
        assert!(CodegenFlags::for_build(OptLevel::Os, false).bare_asserts);
        assert!(!CodegenFlags::for_build(OptLevel::O2, false).bare_asserts);
        assert!(!CodegenFlags::for_build(OptLevel::O0, false).bare_asserts);
        // The opt-level default still folds the branch-hinting on.
        assert!(CodegenFlags::for_build(OptLevel::Os, false).branch_hinting);
    }

    #[test]
    fn no_bare_asserts_overrides_the_os_default() {
        let flags = CodegenFlags::parse(["no-bare-asserts"], OptLevel::Os, false).unwrap();
        assert!(!flags.bare_asserts);
    }

    #[test]
    fn bare_asserts_forces_it_on_below_os() {
        let flags = CodegenFlags::parse(["bare-asserts"], OptLevel::O2, false).unwrap();
        assert!(flags.bare_asserts);
    }

    #[test]
    fn contract_checks_default_on_in_the_test_world_and_below_o2() {
        for (opt_level, checks) in [
            (OptLevel::O0, true),
            (OptLevel::O1, true),
            (OptLevel::O2, false),
            (OptLevel::O3, false),
            (OptLevel::Os, false),
        ] {
            assert!(CodegenFlags::for_build(opt_level, true).contract_checks);
            assert_eq!(
                CodegenFlags::for_build(opt_level, false).contract_checks,
                checks
            );
        }
    }

    #[test]
    fn contract_checks_flags_override_the_defaults() {
        let off = CodegenFlags::parse(["no-contract-checks"], OptLevel::O2, true).unwrap();
        assert!(!off.contract_checks);
        let on = CodegenFlags::parse(["contract-checks"], OptLevel::O2, false).unwrap();
        assert!(on.contract_checks);
    }

    #[test]
    fn no_branch_hinting_disables_the_default() {
        let flags = parse(["no-branch-hinting"]).unwrap();
        assert!(!flags.branch_hinting);
        // Other flags keep their defaults.
        assert!(flags.wide_arithmetic);
    }

    #[test]
    fn explicit_enable_still_works_and_last_wins() {
        // `-f branch-hinting` is redundant with the default but remains valid.
        assert!(parse(["branch-hinting"]).unwrap().branch_hinting);
        // The last flag wins when both spellings appear.
        assert!(
            !parse(["branch-hinting", "no-branch-hinting"])
                .unwrap()
                .branch_hinting
        );
        assert!(
            parse(["no-branch-hinting", "branch-hinting"])
                .unwrap()
                .branch_hinting
        );
    }

    #[test]
    fn unknown_flag_is_reported_verbatim() {
        assert_eq!(parse(["bogus"]), Err("bogus".to_string()));
        // The `no-` prefix is stripped for matching but the error echoes the
        // original spelling so the user sees exactly what they typed.
        assert_eq!(parse(["no-bogus"]), Err("no-bogus".to_string()));
    }
}
