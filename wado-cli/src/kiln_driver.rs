//! Kiln pipeline driver — manifest-lowering + plan computation.
//!
//! This is the CLI-side glue between [`wado_manifest::Manifest`] and the
//! pure-data algorithmic module at [`wado_compiler::kiln`]. Its current
//! responsibility is planning only: lower manifest invocations to the
//! compiler's canonical form, dedup + topologically sort them, and compute
//! cache keys. Executing generators, writing outputs, and updating
//! `wado.lock` land in a subsequent commit alongside a committed
//! generator-component fixture.

use std::path::{Path, PathBuf};

use wado_compiler::kiln::{
    DeclSite, GeneratorModule, Invocation, InvocationPath, Plan, PlanError, build_plan,
};
use wado_manifest::{GeneratorInvocation, GeneratorModuleRef, Manifest};

/// Output directory default when `[build.generators.<name>].output_dir` is
/// unset: `build/kiln/<name>`.
pub const DEFAULT_OUTPUT_DIR_PREFIX: &str = "build/kiln";

/// Outcome of [`plan`].
#[derive(Debug)]
pub struct PlanOutcome {
    /// Scheduled invocations in execution order. Empty when the manifest has
    /// no `[build.generators]` section.
    pub plan: Plan,
    /// The manifest root used to resolve relative paths, preserved so the
    /// caller can read input files without re-deriving it.
    pub manifest_root: PathBuf,
}

/// Errors from the driver. Reuses the compiler-side [`PlanError`] for
/// dedup/cycle diagnostics and adds CLI-specific variants for the
/// lowering step.
#[derive(Debug)]
pub enum DriverError {
    /// The manifest's `module = { ... }` inline form is not yet supported by
    /// the driver. M3c ships with `module = "ns:name@ver"` only; inline
    /// records land together with build-dependency resolution.
    UnsupportedInlineModule { invocation: String },
    /// Dedup / cycle error from the planner.
    Plan(PlanError),
}

impl std::fmt::Display for DriverError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DriverError::UnsupportedInlineModule { invocation } => write!(
                f,
                "[build.generators.{invocation}] uses an inline `module = {{ ... }}` record; \
                 only `module = \"ns:name@version\"` is supported in M3c"
            ),
            DriverError::Plan(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for DriverError {}

impl From<PlanError> for DriverError {
    fn from(e: PlanError) -> Self {
        DriverError::Plan(e)
    }
}

/// Compute a plan for the manifest's `[build.generators]` section.
///
/// Takes an already-parsed manifest and the directory it was loaded from
/// (the `manifest_root`). Paths in `from` / `inputs` / `output_dir` are
/// interpreted relative to that root; the produced [`Invocation`]s carry
/// root-relative normalized paths.
///
/// Options are opaque to the driver at this stage: the canonical encoding
/// is produced by [`encode_options_canonical_provisional`] and passed
/// through as `options_canonical` bytes. M4 swaps the encoder without
/// changing this driver.
///
/// # Errors
///
/// - [`DriverError::UnsupportedInlineModule`] if any invocation uses inline
///   `module = { ... }`.
/// - [`DriverError::Plan`] for cycle / duplicate-generator errors.
///
/// The module-spec ↔ `[build-dependencies]` match is guaranteed by
/// `wado-manifest`'s `FromStr` validation, so this function trusts the
/// caller to pass a validated [`Manifest`].
pub fn plan(manifest: &Manifest, manifest_root: &Path) -> Result<PlanOutcome, DriverError> {
    let Some(build) = manifest.build.as_ref() else {
        return Ok(PlanOutcome {
            plan: Plan { order: Vec::new() },
            manifest_root: manifest_root.to_path_buf(),
        });
    };

    let mut invocations: Vec<Invocation> = Vec::with_capacity(build.generators.len());
    for (name, gi) in &build.generators {
        invocations.push(lower(name, gi)?);
    }

    let plan = build_plan(invocations)?;
    Ok(PlanOutcome {
        plan,
        manifest_root: manifest_root.to_path_buf(),
    })
}

/// Lower a single manifest-declared invocation to the compiler's canonical
/// form.
fn lower(name: &str, gi: &GeneratorInvocation) -> Result<Invocation, DriverError> {
    let module = match &gi.module {
        GeneratorModuleRef::Spec(spec) => GeneratorModule::Spec(spec.clone()),
        GeneratorModuleRef::Inline(_) => {
            return Err(DriverError::UnsupportedInlineModule {
                invocation: name.to_string(),
            });
        }
    };

    let from = InvocationPath::normalize(&gi.from);
    let inputs: Vec<InvocationPath> = gi
        .inputs
        .iter()
        .map(|p| InvocationPath::normalize(p))
        .collect();
    let output_dir = InvocationPath::normalize(
        gi.output_dir
            .as_deref()
            .map(std::string::ToString::to_string)
            .unwrap_or_else(|| format!("{DEFAULT_OUTPUT_DIR_PREFIX}/{name}"))
            .as_str(),
    );
    let options_canonical = encode_options_canonical_provisional(&gi.options);
    Ok(Invocation {
        decl_site: DeclSite::Manifest {
            name: name.to_string(),
        },
        module,
        from,
        inputs,
        output_dir,
        options_canonical,
    })
}

/// Provisional canonical TOML encoder — used by M3 until M4 replaces it with
/// the Component-Model lifted form.
///
/// Format (length-prefixed, depth-first, keys sorted alphabetically):
/// - `0` bool, 1 byte (0 / 1)
/// - `1` integer, 8 BE bytes
/// - `2` float, 8 BE bytes (IEEE-754 bits)
/// - `3` string, u64 BE length-prefix + UTF-8 bytes
/// - `4` array, u64 BE count + each element
/// - `5` table, u64 BE count + each (key string + value)
/// - `6` datetime, u64 BE length-prefix + RFC3339 string
#[must_use]
pub fn encode_options_canonical_provisional(value: &toml::Value) -> Vec<u8> {
    let mut out = Vec::new();
    write_toml(&mut out, value);
    out
}

fn write_toml(out: &mut Vec<u8>, v: &toml::Value) {
    match v {
        toml::Value::Boolean(b) => {
            out.push(0);
            out.push(u8::from(*b));
        }
        toml::Value::Integer(i) => {
            out.push(1);
            out.extend_from_slice(&i.to_be_bytes());
        }
        toml::Value::Float(f) => {
            out.push(2);
            out.extend_from_slice(&f.to_bits().to_be_bytes());
        }
        toml::Value::String(s) => {
            out.push(3);
            write_str(out, s);
        }
        toml::Value::Array(a) => {
            out.push(4);
            write_len(out, a.len());
            for item in a {
                write_toml(out, item);
            }
        }
        toml::Value::Table(t) => {
            out.push(5);
            let mut entries: Vec<(&String, &toml::Value)> = t.iter().collect();
            entries.sort_by(|a, b| a.0.cmp(b.0));
            write_len(out, entries.len());
            for (k, val) in entries {
                write_str(out, k);
                write_toml(out, val);
            }
        }
        toml::Value::Datetime(dt) => {
            out.push(6);
            write_str(out, &dt.to_string());
        }
    }
}

fn write_str(out: &mut Vec<u8>, s: &str) {
    write_len(out, s.len());
    out.extend_from_slice(s.as_bytes());
}

fn write_len(out: &mut Vec<u8>, n: usize) {
    out.extend_from_slice(&(n as u64).to_be_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(toml_str: &str) -> (Manifest, PathBuf) {
        let m: Manifest = toml_str.parse().expect("valid manifest");
        (m, PathBuf::from("/tmp/does-not-matter"))
    }

    #[test]
    fn empty_manifest_produces_empty_plan() {
        let (m, root) = parse(
            r#"
[package]
name = "a"
version = "0.1.0"
"#,
        );
        let outcome = plan(&m, &root).unwrap();
        assert!(outcome.plan.order.is_empty());
    }

    #[test]
    fn manifest_without_build_section_is_empty_plan() {
        let (m, root) = parse(
            r#"
[package]
name = "a"
version = "0.1.0"

[dependencies]
"#,
        );
        assert!(plan(&m, &root).unwrap().plan.order.is_empty());
    }

    #[test]
    fn single_generator_lowered_with_defaults() {
        let (m, root) = parse(
            r#"
[package]
name = "a"
version = "0.1.0"

[registries]
default = "https://wa.dev"

[build-dependencies]
proto = { package = "ns:proto", version = "^1.0.0" }

[build.generators.proto]
module = "ns:proto@1.0.0"
from = "./schema.proto"
"#,
        );
        let outcome = plan(&m, &root).unwrap();
        assert_eq!(outcome.plan.order.len(), 1);
        let inv = &outcome.plan.order[0];
        assert_eq!(inv.from.as_str(), "schema.proto");
        assert_eq!(inv.output_dir.as_str(), "build/kiln/proto");
        assert!(inv.inputs.is_empty());
        match &inv.module {
            GeneratorModule::Spec(s) => assert_eq!(s, "ns:proto@1.0.0"),
            other => panic!("expected Spec, got {other:?}"),
        }
    }

    #[test]
    fn explicit_output_dir_is_preserved() {
        let (m, root) = parse(
            r#"
[package]
name = "a"
version = "0.1.0"

[registries]
default = "https://wa.dev"

[build-dependencies]
proto = { package = "ns:proto", version = "^1.0.0" }

[build.generators.proto]
module = "ns:proto@1.0.0"
from = "./schema.proto"
output-dir = "gen/proto"
"#,
        );
        let outcome = plan(&m, &root).unwrap();
        assert_eq!(outcome.plan.order[0].output_dir.as_str(), "gen/proto");
    }

    #[test]
    fn two_disjoint_generators_both_scheduled() {
        let (m, root) = parse(
            r#"
[package]
name = "a"
version = "0.1.0"

[registries]
default = "https://wa.dev"

[build-dependencies]
proto = { package = "ns:proto", version = "^1.0.0" }
antlr = { package = "ns:antlr", version = "^1.0.0" }

[build.generators.proto]
module = "ns:proto@1.0.0"
from = "./a.proto"

[build.generators.grammar]
module = "ns:antlr@1.0.0"
from = "./a.g4"
"#,
        );
        let outcome = plan(&m, &root).unwrap();
        assert_eq!(outcome.plan.order.len(), 2);
    }

    #[test]
    fn inline_module_record_is_rejected() {
        let (m, root) = parse(
            r#"
[package]
name = "a"
version = "0.1.0"

[build.generators.proto]
module = { path = "../tools/proto" }
from = "./schema.proto"
"#,
        );
        let err = plan(&m, &root).unwrap_err();
        match err {
            DriverError::UnsupportedInlineModule { invocation } => {
                assert_eq!(invocation, "proto");
            }
            other => panic!("expected UnsupportedInlineModule, got {other:?}"),
        }
    }

    #[test]
    fn cycle_between_generators_surfaces_plan_error() {
        let (m, root) = parse(
            r#"
[package]
name = "a"
version = "0.1.0"

[registries]
default = "https://wa.dev"

[build-dependencies]
gen-a = { package = "ns:a", version = "^1.0.0" }
gen-b = { package = "ns:b", version = "^1.0.0" }

[build.generators.first]
module = "ns:a@1.0.0"
from = "build/kiln/second/x.proto"
output-dir = "build/kiln/first"

[build.generators.second]
module = "ns:b@1.0.0"
from = "build/kiln/first/y.proto"
output-dir = "build/kiln/second"
"#,
        );
        let err = plan(&m, &root).unwrap_err();
        assert!(matches!(err, DriverError::Plan(PlanError::Cycle { .. })));
    }

    #[test]
    fn provisional_encoder_is_key_order_independent() {
        let a: toml::Value = toml::from_str("x = 1\ny = 2").unwrap();
        let b: toml::Value = toml::from_str("y = 2\nx = 1").unwrap();
        assert_eq!(
            encode_options_canonical_provisional(&a),
            encode_options_canonical_provisional(&b),
        );
    }

    #[test]
    fn provisional_encoder_distinguishes_string_from_int() {
        let a: toml::Value = toml::from_str("x = \"1\"").unwrap();
        let b: toml::Value = toml::from_str("x = 1").unwrap();
        assert_ne!(
            encode_options_canonical_provisional(&a),
            encode_options_canonical_provisional(&b),
        );
    }
}
