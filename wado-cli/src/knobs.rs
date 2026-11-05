//! The knobs shared across subcommands, in `wado`'s own terms: [`CompileKnobs`]
//! for what the compiler is told and [`HostKnobs`] for how the `wado` process
//! itself runs. `wado_host::options` declares each option, and the runtime
//! knobs along with them.

use lexopt::Parser;
use wado_compiler::LogLevel;
use wado_compiler::param_resolution::ParamInputs;
use wado_compiler::pgo::{Cutoff, Percent};

use crate::args::{self, CliExit, OptSpec};
use crate::host_memory::{FreedMemory, set_freed_memory};

pub use wado_host::options::{
    CompileKnobOpt, HostKnobOpt, OptLevel, RuntimeKnobOpt, RuntimeKnobs, parse_opt_level_arg,
};

/// Every knob a compiling subcommand parses and forwards to the compile core.
///
/// `allocator` stays `Option` so the target world picks the default
/// (`test` → debug, every other → freelist) while `--allocator` overrides.
#[derive(Clone, Debug)]
pub struct CompileKnobs {
    pub opt_level: OptLevel,
    pub log_level: LogLevel,
    pub skip_validation: bool,
    /// Ignore all build caches: every Kiln invocation re-runs its generator,
    /// the generator wasm itself is recompiled from source instead of reused
    /// from `build/kiln/generators/`, every `core:eval` call evaluates
    /// instead of reading `build/eval/`, and `wado test` reruns the files
    /// `build/test/` holds as passed. Cache *writes* still happen, so a
    /// follow-up run without `--no-cache` benefits from a warm cache again.
    pub no_cache: bool,
    pub opt: wado_compiler::OptOverrides,
    /// `--optimize-threads`, where given; see [`Self::optimize_threads`].
    pub opt_threads: Option<usize>,
    pub allocator: Option<String>,
    /// `-f <flag>` codegen feature flags, forwarded verbatim to
    /// `CompilerOptions::codegen_flags`; the compiler validates them.
    pub codegen_flags: Vec<String>,
    /// `--no-lint` / `--lint` / `--lint-generated`, forwarded to
    /// `CompilerOptions::lints`.
    pub lints: wado_compiler::Lints,
    /// `-D NAME=value` overrides, `--param-*` policy, and whatever fallbacks the
    /// subcommand supplies for itself.
    pub params: ParamInputs,
}

impl Default for CompileKnobs {
    fn default() -> Self {
        Self {
            opt_level: OptLevel::default(),
            log_level: args::DEFAULT_LOG_LEVEL,
            skip_validation: false,
            no_cache: false,
            opt: wado_compiler::OptOverrides::default(),
            opt_threads: None,
            allocator: None,
            codegen_flags: Vec::new(),
            lints: wado_compiler::Lints::default(),
            params: ParamInputs::default(),
        }
    }
}

impl CompileKnobs {
    #[must_use]
    pub const fn compiler_opt_level(&self) -> wado_compiler::OptLevel {
        match self.opt_level {
            OptLevel::O0 => wado_compiler::OptLevel::O0,
            OptLevel::O1 => wado_compiler::OptLevel::O1,
            OptLevel::O2 => wado_compiler::OptLevel::O2,
            OptLevel::O3 => wado_compiler::OptLevel::O3,
            OptLevel::Os => wado_compiler::OptLevel::Os,
        }
    }

    /// The knobs of a subcommand that runs what it compiles at once (`run`,
    /// `serve`, `test`). They default to `-O1`: there the compile counts
    /// against the run, which `-O2` costs more than it saves, and `-O1` keeps
    /// the contract checks a program being developed wants. `-O0` inlines
    /// nothing, so a hot loop calls every operator it spells.
    #[must_use]
    pub fn compile_and_run() -> Self {
        Self {
            opt_level: OptLevel::O1,
            ..Self::default()
        }
    }

    /// The threads the optimizer runs on: `--optimize-threads`, else every
    /// available core. A subcommand compiling several files at once sets its
    /// share before this is read: 1 under `wado test`, the cores divided by the
    /// worlds under `wado build` and `wado publish`.
    #[must_use]
    pub fn optimize_threads(&self) -> usize {
        self.opt_threads.unwrap_or_else(|| {
            std::thread::available_parallelism().map_or(1, std::num::NonZero::get)
        })
    }

    /// The compiler options these knobs set, the rest at their defaults.
    #[must_use]
    pub fn compiler_options(&self) -> wado_compiler::CompilerOptions {
        wado_compiler::CompilerOptions {
            opt_level: self.compiler_opt_level(),
            skip_validation: self.skip_validation,
            opt: self.opt.clone(),
            parallelism: self.optimize_threads(),
            log_level: Some(self.log_level),
            allocator: self.allocator.clone(),
            codegen_flags: self.codegen_flags.clone(),
            lints: self.lints,
            params: self.params.clone(),
            ..wado_compiler::CompilerOptions::default()
        }
    }

    /// Apply a matched [`CompileKnobOpt`], consuming its value from the parser.
    pub fn apply(&mut self, opt: CompileKnobOpt, parser: &mut Parser) -> Result<(), CliExit> {
        match opt {
            CompileKnobOpt::OptLevel => self.opt_level = parse_opt_level_arg(parser)?,
            CompileKnobOpt::InlineThreshold => {
                self.opt.inline_threshold = Some(args::parse_inline_threshold_arg(
                    "--optimize-inline-threshold",
                    parser,
                )?);
            }
            CompileKnobOpt::InlineHotCallsiteThreshold => {
                self.opt.inline_hot_callsite_threshold = Some(args::parse_inline_threshold_arg(
                    "--optimize-inline-hot-callsite-threshold",
                    parser,
                )?);
            }
            CompileKnobOpt::InlineColdCallsiteThreshold => {
                self.opt.inline_cold_callsite_threshold = Some(args::parse_inline_threshold_arg(
                    "--optimize-inline-cold-callsite-threshold",
                    parser,
                )?);
            }
            CompileKnobOpt::InlineGrowth => {
                self.opt.inline_growth =
                    Some(args::parse_u32_arg("--optimize-inline-growth", parser)?);
            }
            CompileKnobOpt::OptIterations => {
                self.opt.iterations = Some(args::parse_u32_arg("--optimize-iterations", parser)?);
            }
            CompileKnobOpt::Specializations => {
                self.opt.specializations =
                    Some(args::parse_u32_arg("--optimize-specializations", parser)? as usize);
            }
            CompileKnobOpt::OptThreads => {
                let threads = args::parse_u32_arg("--optimize-threads", parser)?;
                if threads == 0 {
                    return Err(CliExit::error(
                        "--optimize-threads requires a positive integer",
                    ));
                }
                self.opt_threads = Some(threads as usize);
            }
            CompileKnobOpt::PredictableBranchThreshold => {
                self.opt.predictable_branch_threshold = Some(parse_percent_arg(
                    "--optimize-predictable-branch-threshold",
                    parser,
                )?);
            }
            CompileKnobOpt::ProfileSummaryCutoffHot => {
                self.opt.profile_summary_cutoff_hot = Some(parse_cutoff_arg(
                    "--optimize-profile-summary-cutoff-hot",
                    parser,
                )?);
            }
            CompileKnobOpt::ProfileSummaryCutoffCold => {
                self.opt.profile_summary_cutoff_cold = Some(parse_cutoff_arg(
                    "--optimize-profile-summary-cutoff-cold",
                    parser,
                )?);
            }
            CompileKnobOpt::LogLevel => self.log_level = args::parse_log_level_arg(parser)?,
            CompileKnobOpt::Allocator => self.allocator = Some(args::require_string(parser)?),
            CompileKnobOpt::Feature => self.codegen_flags.push(args::require_string(parser)?),
            CompileKnobOpt::NoCache => self.no_cache = true,
            CompileKnobOpt::NoValidate => self.skip_validation = true,
            CompileKnobOpt::NoLint => self.lints = wado_compiler::Lints::Off,
            CompileKnobOpt::Lint => self.lints = wado_compiler::Lints::Own,
            CompileKnobOpt::LintGenerated => self.lints = wado_compiler::Lints::Generated,
        }
        Ok(())
    }
}

fn parse_percent_arg(opt: &str, parser: &mut Parser) -> Result<Percent, CliExit> {
    let n = args::parse_u32_arg(opt, parser)?;
    Percent::new(n).ok_or_else(|| {
        CliExit::error(format!(
            "{opt} is a share in percent, at most {}, got {n}",
            Percent::WHOLE
        ))
    })
}

fn parse_cutoff_arg(opt: &str, parser: &mut Parser) -> Result<Cutoff, CliExit> {
    let n = args::parse_u32_arg(opt, parser)?;
    Cutoff::new(n).ok_or_else(|| {
        CliExit::error(format!(
            "{opt} is a share in parts per million, at most {}, got {n}",
            Cutoff::PER_MILLION
        ))
    })
}

/// Every knob a compiling subcommand parses for the `wado` process itself.
/// A subcommand without them leaves the allocator returning freed memory.
#[derive(Clone, Copy, Debug)]
pub struct HostKnobs {
    pub freed_memory: FreedMemory,
}

impl Default for HostKnobs {
    /// Retaining: a subcommand that runs long, where a resident set held at
    /// its peak outlives the compile that grew it, starts from returning.
    fn default() -> Self {
        Self {
            freed_memory: FreedMemory::Retain,
        }
    }
}

impl HostKnobs {
    pub fn apply(&mut self, opt: HostKnobOpt) {
        self.freed_memory = match opt {
            HostKnobOpt::RetainMemory => FreedMemory::Retain,
            HostKnobOpt::NoRetainMemory => FreedMemory::Return,
        };
    }

    /// Set the process up as the knobs say, before the subcommand runs.
    pub fn install(self) {
        set_freed_memory(self.freed_memory);
    }
}

/// Whether a custom section is embedded in the output component. `Auto` follows
/// the optimization level (off under `-Os`, where the metadata never reaches a
/// CM host); `On` / `Off` are the explicit `--embed-*` / `--no-embed-*` flags.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum EmbedPolicy {
    #[default]
    Auto,
    On,
    Off,
}

impl EmbedPolicy {
    /// Resolve against `opt_level`. Returns `Some(explicit)`, where `explicit`
    /// marks a user-forced `On` (so an embedding failure is fatal rather than a
    /// warning), or `None` to skip. See WEP
    /// `wep-2026-05-02-wit-interoperability.md` §"Embedding policy".
    #[must_use]
    pub const fn resolve(self, opt_level: OptLevel) -> Option<bool> {
        match self {
            Self::Off => None,
            Self::On => Some(true),
            Self::Auto if matches!(opt_level, OptLevel::Os) => None,
            Self::Auto => Some(false),
        }
    }
}

/// The `component-type` WIT section and the `[package]` metadata sections.
#[derive(Clone, Copy, Debug, Default)]
pub struct EmbedOptions {
    pub wit: EmbedPolicy,
    pub metadata: EmbedPolicy,
}

/// The `--embed-*` / `--no-embed-*` flags, as accepted by `compile` and `build`.
#[derive(Clone, Copy)]
pub enum EmbedOpt {
    NoWit,
    Wit,
    NoMetadata,
    Metadata,
}

impl EmbedOpt {
    /// `compile` embeds WIT but takes its metadata decision from the manifest,
    /// so it exposes only the WIT pair.
    pub const WIT_ONLY: &[Self] = &[Self::NoWit, Self::Wit];
    pub const ALL: &[Self] = &[Self::NoWit, Self::Wit, Self::NoMetadata, Self::Metadata];

    #[must_use]
    pub const fn spec(self) -> OptSpec {
        match self {
            Self::NoWit => OptSpec {
                long: Some("no-embed-wit"),
                short: None,
                value: None,
                desc: "Do not embed the WIT `component-type` section in the output",
            },
            Self::Wit => OptSpec {
                long: Some("embed-wit"),
                short: None,
                value: None,
                desc: "Force embedding the WIT section on (e.g. under -Os, where it is off by default)",
            },
            Self::NoMetadata => OptSpec {
                long: Some("no-embed-metadata"),
                short: None,
                value: None,
                desc: "Do not embed the [package] metadata sections in the output",
            },
            Self::Metadata => OptSpec {
                long: Some("embed-metadata"),
                short: None,
                value: None,
                desc: "Force embedding the [package] metadata on (e.g. under -Os, where it is off by default)",
            },
        }
    }
}

impl EmbedOptions {
    /// Apply a matched [`EmbedOpt`]. Setting a slot the opposite way twice is
    /// the mutual-exclusion error; repeating the same flag is idempotent.
    pub fn apply(&mut self, opt: EmbedOpt) -> Result<(), CliExit> {
        let (slot, wanted, pair) = match opt {
            EmbedOpt::NoWit => (&mut self.wit, EmbedPolicy::Off, "embed-wit"),
            EmbedOpt::Wit => (&mut self.wit, EmbedPolicy::On, "embed-wit"),
            EmbedOpt::NoMetadata => (&mut self.metadata, EmbedPolicy::Off, "embed-metadata"),
            EmbedOpt::Metadata => (&mut self.metadata, EmbedPolicy::On, "embed-metadata"),
        };
        if *slot != EmbedPolicy::Auto && *slot != wanted {
            return Err(CliExit::error(format!(
                "`--no-{pair}` and `--{pair}` are mutually exclusive"
            )));
        }
        *slot = wanted;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{CompileKnobOpt, EmbedOpt, EmbedOptions, EmbedPolicy, OptLevel};
    use wado_compiler::CodegenFlags;

    #[test]
    fn the_feature_help_lists_every_supported_flag() {
        let listed: Vec<&str> = CompileKnobOpt::Feature
            .spec()
            .desc
            .lines()
            .skip(1)
            .map(|line| line.split_whitespace().next().unwrap())
            .collect();
        assert_eq!(listed, CodegenFlags::SUPPORTED);
    }

    #[test]
    fn auto_embeds_below_os_and_skips_at_os() {
        assert_eq!(EmbedPolicy::Auto.resolve(OptLevel::O2), Some(false));
        assert_eq!(EmbedPolicy::Auto.resolve(OptLevel::Os), None);
    }

    #[test]
    fn explicit_on_wins_at_os_and_off_always_skips() {
        assert_eq!(EmbedPolicy::On.resolve(OptLevel::Os), Some(true));
        assert_eq!(EmbedPolicy::Off.resolve(OptLevel::O2), None);
    }

    #[test]
    fn opposing_embed_flags_are_mutually_exclusive() {
        let mut opts = EmbedOptions::default();
        opts.apply(EmbedOpt::NoWit).unwrap();
        let err = opts.apply(EmbedOpt::Wit).unwrap_err();
        assert!(err.message.contains("mutually exclusive"), "{err:?}");
    }

    #[test]
    fn repeating_one_embed_flag_is_idempotent() {
        let mut opts = EmbedOptions::default();
        opts.apply(EmbedOpt::Metadata).unwrap();
        opts.apply(EmbedOpt::Metadata).unwrap();
        assert_eq!(opts.metadata, EmbedPolicy::On);
    }
}
