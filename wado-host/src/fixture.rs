//! What an e2e fixture's `__DATA__` section says about how to compile it, read
//! once for both the e2e harness and the golden dumps, so the goldens record
//! the program the tests run.

use indexmap::IndexMap;
use serde::Deserialize;
use wado_compiler::param_resolution::{ParamInputs, ParamPolicy, ParamPolicyLevel};

use crate::HostStubs;

/// The text after a fixture's `__DATA__` line, if it has one.
#[must_use]
pub fn data_section(source: &str) -> Option<&str> {
    let marker = "\n__DATA__\n";
    match source.find(marker) {
        Some(pos) => Some(&source[pos + marker.len()..]),
        None => source.strip_prefix("__DATA__\n"),
    }
}

/// The compile inputs among a fixture's `__DATA__` keys. The other keys state
/// expectations, which only the e2e harness reads.
#[derive(Debug, Default, Deserialize)]
pub struct CompileInputs {
    /// Compile-time parameter overrides (`-D NAME=value`) for `#[param]` globals.
    #[serde(default)]
    pub params: IndexMap<String, String>,
    /// Stubbed compile-time environment for `#[param(from_env = ...)]`.
    #[serde(default)]
    pub param_env: IndexMap<String, String>,
    /// Host-supplied parameter fallbacks, as `wado test` supplies `log.level`.
    #[serde(default)]
    pub param_defaults: IndexMap<String, String>,
    /// Stubbed path `[dependencies]`: name → the dependency's `[package].lib`,
    /// relative to the fixture directory. Each entry is its own package.
    #[serde(default)]
    pub dependencies: IndexMap<String, String>,
    /// Override the `--param-unknown` policy level (`error` / `warn` / `ignore`).
    #[serde(default)]
    pub param_unknown: Option<String>,
    /// Override the `--param-invalid` policy level (`error` / `warn` / `ignore`).
    #[serde(default)]
    pub param_invalid: Option<String>,
    /// Override the `--param-missing` policy level (`error` / `warn` / `ignore`).
    #[serde(default)]
    pub param_missing: Option<String>,
}

impl CompileInputs {
    /// What the compiling host answers from these inputs.
    #[must_use]
    pub fn host_stubs(&self) -> HostStubs {
        HostStubs {
            env: self.param_env.clone(),
            dependencies: self.dependencies.clone(),
        }
    }

    /// The `#[param]` inputs the compiler resolves against.
    ///
    /// # Panics
    /// If a `param_*` policy names no level.
    #[must_use]
    pub fn param_inputs(&self) -> ParamInputs {
        let mut policy = ParamPolicy::default();
        for (field, level, slot) in [
            ("param_unknown", &self.param_unknown, &mut policy.unknown),
            ("param_invalid", &self.param_invalid, &mut policy.invalid),
            ("param_missing", &self.param_missing, &mut policy.missing),
        ] {
            if let Some(level) = level {
                *slot = ParamPolicyLevel::parse(level)
                    .unwrap_or_else(|| panic!("invalid {field} level: {level:?}"));
            }
        }
        ParamInputs {
            overrides: self.params.clone().into_iter().collect(),
            defaults: self.param_defaults.clone().into_iter().collect(),
            policy,
        }
    }
}
