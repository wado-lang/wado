//! The knobs shared across subcommands: [`CompileKnobs`] for what the compiler
//! is told, [`RuntimeKnobs`] for what the wasmtime engine is built with.

use lexopt::Parser;
use wado_compiler::LogLevel;
use wado_compiler::param_resolution::ParamInputs;

use crate::args::{self, CliExit, OptSpec};
use crate::runtime::{
    DEFAULT_COLLECTOR, DEFAULT_GC_HEAP_INITIAL_SIZE, parse_collector, parse_gc_heap_size,
};

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum OptLevel {
    O0,
    /// All passes except DCE. Iterations: 2, inline threshold: 10.
    O1,
    /// Production: all passes, including DCE. Iterations: 10, inline threshold: 10.
    #[default]
    O2,
    /// Aggressive. Iterations: 100, inline threshold: 20.
    O3,
    /// `O2` plus name-section stripping.
    Os,
}

impl OptLevel {
    /// wasmtime exposes only `None`/`Speed`/`SpeedAndSize`, so `O1`/`O2`/
    /// `O3` collapse to `Speed` — mirroring the `wasmtime` CLI's own `-O`
    /// mapping.
    #[must_use]
    pub const fn to_wasmtime(self) -> wasmtime::OptLevel {
        match self {
            Self::O0 => wasmtime::OptLevel::None,
            Self::O1 | Self::O2 | Self::O3 => wasmtime::OptLevel::Speed,
            Self::Os => wasmtime::OptLevel::SpeedAndSize,
        }
    }

    #[must_use]
    pub const fn to_compiler(self) -> wado_compiler::OptLevel {
        match self {
            Self::O0 => wado_compiler::OptLevel::O0,
            Self::O1 => wado_compiler::OptLevel::O1,
            Self::O2 => wado_compiler::OptLevel::O2,
            Self::O3 => wado_compiler::OptLevel::O3,
            Self::Os => wado_compiler::OptLevel::Os,
        }
    }
}

/// Parse `-O<n>`. The level is always attached and always explicit: a bare
/// `-O` is an error rather than a silent default, and the level is never taken
/// from the next argument, so `-O` cannot swallow the input file.
pub fn parse_opt_level_arg(parser: &mut Parser) -> Result<OptLevel, CliExit> {
    let Some(val) = parser.optional_value() else {
        return Err(CliExit::error(
            "-O requires a level. Use -O0, -O1, -O2, -O3, -Os, or -Og",
        ));
    };
    match val.to_string_lossy().as_ref() {
        "0" | "g" => Ok(OptLevel::O0),
        "1" => Ok(OptLevel::O1),
        "2" => Ok(OptLevel::O2),
        "3" => Ok(OptLevel::O3),
        "s" => Ok(OptLevel::Os),
        other => Err(CliExit::error(format!(
            "unknown optimization level '-O{other}'. Use -O0, -O1, -O2, -O3, -Os, or -Og"
        ))),
    }
}

/// A knob shared by the compiling subcommands. Each one declares the subset it
/// accepts as a `&[CompileKnobOpt]`, used for both matching and help rendering.
#[derive(Clone, Copy)]
pub enum CompileKnobOpt {
    OptLevel,
    InlineThreshold,
    InlineGrowth,
    OptIterations,
    LogLevel,
    Allocator,
    Feature,
    NoCache,
    NoValidate,
}

impl CompileKnobOpt {
    #[must_use]
    pub const fn spec(self) -> OptSpec {
        match self {
            Self::OptLevel => OptSpec {
                long: None,
                short: Some('O'),
                value: Some("<n>"),
                desc: "Optimization level: -O0, -O1, -O2, -O3, -Os",
            },
            Self::InlineThreshold => OptSpec {
                long: Some("optimize-inline-threshold"),
                short: None,
                value: Some("<n>"),
                desc: "Override inlining threshold (max statement count per function)",
            },
            Self::InlineGrowth => OptSpec {
                long: Some("optimize-inline-growth"),
                short: None,
                value: Some("<pct>"),
                desc: "Override how far inlining may grow the program, in percent",
            },
            Self::OptIterations => OptSpec {
                long: Some("optimize-iterations"),
                short: None,
                value: Some("<n>"),
                desc: "Override number of fixed-point optimization iterations",
            },
            Self::LogLevel => OptSpec {
                long: Some("log-level"),
                short: None,
                value: Some("<level>"),
                desc: "Log level: debug, info, warn, error, off (default: warn)",
            },
            Self::Allocator => OptSpec {
                long: Some("allocator"),
                short: None,
                value: Some("<mode>"),
                desc: "Allocator mode (default depends on target world):\nbump (CLI), freelist (HTTP), debug (test; no-reuse + 0xFF poison)",
            },
            // Repeatable; prefix a flag with `no-` to disable it. The compiler
            // validates the names.
            Self::Feature => OptSpec {
                long: None,
                short: Some('f'),
                value: Some("<flag>"),
                desc: "Toggle a codegen feature flag (repeatable; prefix no- to disable):\narray-copy       native Wasm array.copy instead of a loop (default: on)\nbranch-hinting   emit metadata.code.branch_hint entries (default: on)\nbare-asserts     assertion failures trap without a message (default: on at -Os)\nwide-arithmetic  native i64.mul_wide/add128/sub128 (default: on)",
            },
            Self::NoCache => OptSpec {
                long: Some("no-cache"),
                short: None,
                value: None,
                desc: "Bypass all build caches: re-run Kiln generators on every invocation\nand recompile generator wasm components from source.\nThe cache refreshes automatically, so this is normally unnecessary —\nit exists for benchmarking and cache-bug debugging.",
            },
            Self::NoValidate => OptSpec {
                long: Some("no-validate"),
                short: None,
                value: None,
                desc: "Skip Wasm validation (output raw bytes even if invalid)",
            },
        }
    }
}

/// Every knob a compiling subcommand parses and forwards to the compile core.
///
/// `allocator` stays `Option` so each subcommand keeps its own default
/// (`run` → bump, `serve` → freelist, `test` → debug) while `--allocator`
/// overrides.
#[derive(Clone, Debug)]
pub struct CompileKnobs {
    pub opt_level: OptLevel,
    pub log_level: LogLevel,
    pub skip_validation: bool,
    /// Ignore all build caches: every Kiln invocation re-runs its generator,
    /// and the generator wasm itself is recompiled from source instead of
    /// reused from `build/kiln/generators/`. Cache *writes* still happen, so a
    /// follow-up run without `--no-cache` benefits from a warm cache again.
    pub no_cache: bool,
    pub opt: wado_compiler::OptOverrides,
    pub allocator: Option<String>,
    /// `-f <flag>` codegen feature flags, forwarded verbatim to
    /// `CompilerOptions::codegen_flags`; the compiler validates them.
    pub codegen_flags: Vec<String>,
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
            allocator: None,
            codegen_flags: Vec::new(),
            params: ParamInputs::default(),
        }
    }
}

impl CompileKnobs {
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
            CompileKnobOpt::InlineGrowth => {
                self.opt.inline_growth =
                    Some(args::parse_u32_arg("--optimize-inline-growth", parser)?);
            }
            CompileKnobOpt::OptIterations => {
                self.opt.iterations = Some(args::parse_u32_arg("--optimize-iterations", parser)?);
            }
            CompileKnobOpt::LogLevel => self.log_level = args::parse_log_level_arg(parser)?,
            CompileKnobOpt::Allocator => self.allocator = Some(args::require_string(parser)?),
            CompileKnobOpt::Feature => self.codegen_flags.push(args::require_string(parser)?),
            CompileKnobOpt::NoCache => self.no_cache = true,
            CompileKnobOpt::NoValidate => self.skip_validation = true,
        }
        Ok(())
    }
}

/// A knob shared by the subcommands that host a guest (`run` / `serve` /
/// `test`), used for both matching and help rendering.
#[derive(Clone, Copy)]
pub enum RuntimeKnobOpt {
    Collector,
    GcHeapInitial,
}

impl RuntimeKnobOpt {
    pub const ALL: &[Self] = &[Self::Collector, Self::GcHeapInitial];

    #[must_use]
    pub const fn spec(self) -> OptSpec {
        match self {
            Self::Collector => OptSpec {
                long: Some("collector"),
                short: None,
                value: Some("<mode>"),
                desc: "GC collector (default: copying):\ncopying, drc (deferred ref-counting), null (never collects)",
            },
            Self::GcHeapInitial => OptSpec {
                long: Some("gc-heap-initial"),
                short: None,
                value: Some("<size>"),
                desc: "GC heap a guest starts with (default: 256m)\nBytes, or a k / m / g suffix. The copying collector splits it\ninto two semi-spaces, so a program allocates through half",
            },
        }
    }
}

/// Every knob a guest-hosting subcommand parses and forwards to the wasmtime
/// engine.
#[derive(Clone, Copy, Debug)]
pub struct RuntimeKnobs {
    pub collector: wasmtime::Collector,
    pub gc_heap_initial_size: u64,
}

impl Default for RuntimeKnobs {
    fn default() -> Self {
        Self {
            collector: DEFAULT_COLLECTOR,
            gc_heap_initial_size: DEFAULT_GC_HEAP_INITIAL_SIZE,
        }
    }
}

impl RuntimeKnobs {
    /// Apply a matched [`RuntimeKnobOpt`], consuming its value from the parser.
    pub fn apply(&mut self, opt: RuntimeKnobOpt, parser: &mut Parser) -> Result<(), CliExit> {
        let spec = args::require_string(parser)?;
        match opt {
            RuntimeKnobOpt::Collector => {
                self.collector = parse_collector(&spec).map_err(CliExit::error)?;
            }
            RuntimeKnobOpt::GcHeapInitial => {
                self.gc_heap_initial_size = parse_gc_heap_size(&spec).map_err(CliExit::error)?;
            }
        }
        Ok(())
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
    use super::{EmbedOpt, EmbedOptions, EmbedPolicy, OptLevel, parse_opt_level_arg};
    use lexopt::Parser;

    /// Drive `parse_opt_level_arg` the way the parse loop does: the parser sits
    /// just past the matched `-O`.
    fn parse_opt_level(args: &[&str]) -> Result<OptLevel, String> {
        let mut parser = Parser::from_args(args);
        parser.next().unwrap().unwrap();
        parse_opt_level_arg(&mut parser).map_err(|e| e.message)
    }

    #[test]
    fn every_level_is_spelled_out() {
        for (arg, expected) in [
            ("-O0", OptLevel::O0),
            ("-O1", OptLevel::O1),
            ("-O2", OptLevel::O2),
            ("-O3", OptLevel::O3),
            ("-Os", OptLevel::Os),
            ("-Og", OptLevel::O0),
        ] {
            assert_eq!(parse_opt_level(&[arg]), Ok(expected), "failed for {arg}");
        }
    }

    #[test]
    fn a_bare_dash_o_is_rejected() {
        let err = parse_opt_level(&["-O", "input.wado"]).unwrap_err();
        assert!(err.contains("requires a level"), "{err}");
    }

    #[test]
    fn a_bare_dash_o_does_not_swallow_the_input_file() {
        // `-O` takes its level attached, never from the next argument, so a
        // rejected `-O` never blames the file that follows it.
        let err = parse_opt_level(&["-O", "input.wado"]).unwrap_err();
        assert!(!err.contains("input.wado"), "{err}");
    }

    #[test]
    fn an_unknown_level_is_rejected() {
        let err = parse_opt_level(&["-O9"]).unwrap_err();
        assert!(err.contains("unknown optimization level '-O9'"), "{err}");
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
