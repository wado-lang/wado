//! What an e2e fixture's `__DATA__` section says about how to compile it, read
//! once for both the e2e harness and the golden dumps, so the goldens record
//! the program the tests run.

use indexmap::IndexMap;
use serde::de::Error as _;
use serde::{Deserialize, Deserializer};
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
    /// The `-f` codegen flags to compile with, at every level.
    #[serde(default)]
    pub codegen_flags: Vec<String>,
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
    #[serde(default, deserialize_with = "policy_level")]
    pub param_unknown: Option<ParamPolicyLevel>,
    /// Override the `--param-invalid` policy level (`error` / `warn` / `ignore`).
    #[serde(default, deserialize_with = "policy_level")]
    pub param_invalid: Option<ParamPolicyLevel>,
    /// Override the `--param-missing` policy level (`error` / `warn` / `ignore`).
    #[serde(default, deserialize_with = "policy_level")]
    pub param_missing: Option<ParamPolicyLevel>,
}

/// A level is rejected while `__DATA__` is read, so the reader that knows which
/// fixture it is reading reports it.
fn policy_level<'de, D: Deserializer<'de>>(de: D) -> Result<Option<ParamPolicyLevel>, D::Error> {
    let level = String::deserialize(de)?;
    ParamPolicyLevel::parse(&level)
        .map(Some)
        .ok_or_else(|| D::Error::custom(format!("invalid policy level: {level:?}")))
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
    #[must_use]
    pub fn param_inputs(&self) -> ParamInputs {
        let defaults = ParamPolicy::default();
        let policy = ParamPolicy {
            unknown: self.param_unknown.unwrap_or(defaults.unknown),
            invalid: self.param_invalid.unwrap_or(defaults.invalid),
            missing: self.param_missing.unwrap_or(defaults.missing),
        };
        ParamInputs {
            overrides: self.params.clone().into_iter().collect(),
            defaults: self.param_defaults.clone().into_iter().collect(),
            policy,
        }
    }
}
