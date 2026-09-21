//! End-to-end test that a minimal Wado source targeting the
//! `core:kiln/generator` world compiles to valid component bytes.
//!
//! Covers:
//! - Happy path (no options): `export fn generate(req: Request)` returning
//!   an empty `Response`.
//! - Typed options: `export fn generate(req: Request<Options>)` produces a
//!   valid component whose `generate` carries `Options` as a typed argument.
//! - Import-refusal: adding `use { now } from "wasi:clocks";` to a
//!   generator surfaces `Code::KilnGeneratorForbiddenImport`.
//!
//! See WEP 2026-04-12 §"Options are a typed argument in each generator's own
//! world".

use std::sync::Mutex;

use crate::common::{block_on, install_dev_stdlib};
use indexmap::IndexMap;
use wado_compiler::{
    Code, CompileResult, CompilerHost, CompilerOptions, Diagnostic, LogLevel, Severity,
    SourceError, compile_with_options, wir::ImportKind,
};

struct MapHost {
    sources: IndexMap<String, String>,
    diagnostics: Mutex<Vec<Diagnostic>>,
}

impl MapHost {
    fn new(sources: &[(&str, &str)]) -> Self {
        install_dev_stdlib();
        Self {
            sources: sources
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect(),
            diagnostics: Mutex::new(Vec::new()),
        }
    }

    fn diagnostics(&self) -> Vec<Diagnostic> {
        self.diagnostics.lock().unwrap().clone()
    }
}

impl CompilerHost for MapHost {
    fn load_source(
        &self,
        path: &str,
    ) -> impl std::future::Future<Output = Result<Vec<u8>, SourceError>> + Send {
        let result = self.sources.get(path).cloned();
        let path = path.to_string();
        async move {
            match result {
                Some(s) => Ok(s.into_bytes()),
                None => Err(SourceError::NotFound { path }),
            }
        }
    }

    fn emit_diagnostic(&self, diagnostic: Diagnostic) {
        self.diagnostics.lock().unwrap().push(diagnostic);
    }
}

fn kiln_options() -> CompilerOptions {
    // The revision-3 typed-request adapter produces a valid component, so no
    // `skip_validation` is needed — the `generate(primary, inputs, options)`
    // lift/lower round-trips through the CM ABI cleanly.
    CompilerOptions {
        log_level: Some(LogLevel::Warn),
        target_world: Some("core:kiln/generator".to_string()),
        retain_wir: true,
        ..CompilerOptions::default()
    }
}

fn diag_list(diags: &[Diagnostic]) -> String {
    diags
        .iter()
        .map(|d| format!("  {d}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Compile `source` in the generator world, panicking with the diagnostics if it
/// does not. `what` names the generator in that message.
fn compile_generator(source: &str, what: &str) -> CompileResult {
    let host = MapHost::new(&[]);
    let result = block_on(compile_with_options(
        source,
        &host,
        Some("generator.wado"),
        kiln_options(),
    ));
    let Ok(result) = result else {
        panic!(
            "{what} failed to compile:\n{}",
            diag_list(&host.diagnostics())
        );
    };
    result
}

/// Assert `source` is refused for importing `interface`, whatever reached it.
fn expect_forbidden_import(source: &str, interface: &str, what: &str) {
    let host = MapHost::new(&[]);
    let result = block_on(compile_with_options(
        source,
        &host,
        Some("generator.wado"),
        kiln_options(),
    ));
    assert!(result.is_err(), "{what} should fail to compile");

    let diags = host.diagnostics();
    let found = diags.iter().any(|d| {
        d.severity == Severity::Error
            && d.code == Code::KilnGeneratorForbiddenImport
            && d.message.contains(interface)
    });
    assert!(
        found,
        "expected KilnGeneratorForbiddenImport naming {interface}, got:\n{}",
        diag_list(&diags)
    );
}

const NOOP_GENERATOR: &str = r#"
use { Request, Response, Error } from "core:kiln";

export fn generate(req: Request) -> Result<Response, Error> {
    let _ = req.primary.path;
    return Result::Ok(Response { files: [] });
}
"#;

#[test]
fn noop_generator_compiles_to_component_bytes() {
    let result = compile_generator(NOOP_GENERATOR, "noop generator");
    assert!(
        !result.wasm.is_empty(),
        "noop generator produced empty wasm"
    );
    assert!(
        result.wasm.starts_with(b"\0asm"),
        "noop generator did not produce a component-shaped wasm (first bytes = {:?})",
        &result.wasm[..4.min(result.wasm.len())],
    );
}

const TYPED_OPTIONS_GENERATOR: &str = r#"
use { Request, Response, Error } from "core:kiln";

pub struct Options {
    pub highlight: bool,
    pub trace: bool,
}

export fn generate(req: Request<Options>) -> Result<Response, Error> {
    let _ = req.primary.path;
    let _ = req.options.highlight;
    let _ = req.options.trace;
    return Result::Ok(Response { files: [] });
}
"#;

#[test]
fn typed_options_generator_compiles_to_valid_component() {
    // No `skip_validation`: the revision-3 typed-options `generate(primary, inputs,
    // options)` shape must produce a valid component (unlike the old
    // `raw-request` GC-reference mismatch).
    let result = compile_generator(TYPED_OPTIONS_GENERATOR, "typed-options generator");
    assert!(result.wasm.starts_with(b"\0asm"), "not component-shaped");
}

/// A nested `Options` field type may be declared in another module, which is
/// where a generator that shares the type with its own library puts it. The
/// options descriptor already resolves one across modules; the CM emit has to
/// declare it too, or it reaches the named-type registry with nothing behind
/// the name.
const CROSS_MODULE_OPTIONS_GENERATOR: &str = r#"
use { Request, Response, Error } from "core:kiln";
use { Rule } from "./rule.wado";

pub struct Options {
    pub rules: List<Rule>,
}

export fn generate(req: Request<Options>) -> Result<Response, Error> {
    let _ = req.primary.path;
    let _ = req.options.rules.len();
    return Result::Ok(Response { files: [] });
}
"#;

const CROSS_MODULE_OPTIONS_RULE: &str = r#"
pub struct Rule {
    pub pattern: String,
    pub names: List<String>,
}
"#;

#[test]
fn options_field_type_from_another_module_compiles() {
    let host = MapHost::new(&[("./rule.wado", CROSS_MODULE_OPTIONS_RULE)]);
    let result = block_on(compile_with_options(
        CROSS_MODULE_OPTIONS_GENERATOR,
        &host,
        Some("generator.wado"),
        kiln_options(),
    ));
    let Ok(result) = result else {
        panic!(
            "cross-module options generator failed to compile:\n{}",
            diag_list(&host.diagnostics())
        );
    };
    assert!(result.wasm.starts_with(b"\0asm"), "not component-shaped");
}

/// Two submodules declaring one public type name reach the registry under the
/// same (interface, name), which it registers exactly once. A generator's
/// surface is published like a library's, so it owes the same diagnostic.
const DUPLICATE_TYPE_GENERATOR: &str = r#"
use { Request, Response, Error } from "core:kiln";
use { Rule } from "./rule.wado";
use { Other } from "./other.wado";

pub struct Options {
    pub rules: List<Rule>,
    pub others: List<Other>,
}

export fn generate(req: Request<Options>) -> Result<Response, Error> {
    let _ = req.primary.path;
    let _ = req.options.rules.len() + req.options.others.len();
    return Result::Ok(Response { files: [] });
}
"#;

const DUPLICATE_TYPE_RULE: &str = r#"
pub struct Rule {
    pub pattern: String,
}

pub struct Shared {
    pub a: String,
}
"#;

const DUPLICATE_TYPE_OTHER: &str = r#"
pub struct Other {
    pub name: String,
}

pub struct Shared {
    pub b: String,
}
"#;

#[test]
fn duplicate_public_type_across_generator_modules_is_diagnosed() {
    let host = MapHost::new(&[
        ("./rule.wado", DUPLICATE_TYPE_RULE),
        ("./other.wado", DUPLICATE_TYPE_OTHER),
    ]);
    let result = block_on(compile_with_options(
        DUPLICATE_TYPE_GENERATOR,
        &host,
        Some("generator.wado"),
        kiln_options(),
    ));
    assert!(
        result.is_err(),
        "a duplicate public type should not compile"
    );

    let diags = host.diagnostics();
    let found = diags
        .iter()
        .any(|d| d.code == Code::DuplicateDefinition && d.message.contains("`Shared`"));
    assert!(
        found,
        "expected DuplicateDefinition naming `Shared`, got:\n{}",
        diag_list(&diags)
    );
}

/// A generator may declare helper `export fn`s beside `generate`. Only
/// `generate` is the world contract, so a non-`generate` export (here a plain
/// `u32`-returning function) must not be force-routed through the async
/// task-return result binding, which would emit an invalid component.
const MULTI_EXPORT_GENERATOR: &str = r#"
use { Request, Response, Error } from "core:kiln";

export fn helper() -> u32 {
    return 7;
}

export fn generate(req: Request) -> Result<Response, Error> {
    let _ = req.primary.path;
    let _ = helper();
    return Result::Ok(Response { files: [] });
}
"#;

#[test]
fn generator_with_extra_export_compiles_to_valid_component() {
    let result = compile_generator(MULTI_EXPORT_GENERATOR, "multi-export generator");
    assert!(result.wasm.starts_with(b"\0asm"), "not component-shaped");
}

const FORBIDDEN_IMPORT_GENERATOR: &str = r#"
use { Request, Response, Error } from "core:kiln";
use { now } from "wasi:clocks";

export fn generate(req: Request) -> Result<Response, Error> {
    let _ = req.primary.path;
    let _ = now();
    return Result::Ok(Response { files: [] });
}
"#;

#[test]
fn generator_importing_wasi_clocks_is_rejected() {
    expect_forbidden_import(
        FORBIDDEN_IMPORT_GENERATOR,
        "wasi:clocks",
        "a generator with a wasi: import",
    );
}

/// A generator reaching a `core:*` module that itself imports WASI must not
/// pick up that import: the Kiln linker offers only `core:kiln/kiln-host`, so a
/// `wasi:*` interface carrying a function fails instantiation.
///
/// `core:log` is no `wasi:` import at the source level, so the `use`-site check
/// cannot see this and the guarantee is asserted on the emitted component.
const CORE_LOG_GENERATOR: &str = r#"
use { Request, Response, Error } from "core:kiln";
use { info } from "core:log";

export fn generate(req: Request) -> Result<Response, Error> {
    info(`generating`, { path: req.primary.path });
    return Result::Ok(Response { files: [] });
}
"#;

/// The `wasi:` interfaces a generator's import plan carries, each with the kind
/// codegen emits it as. `wit_import_plan` pins the plan against the component's
/// actual imports, so reading the plan reads what the linker will be asked for.
fn wasi_imports_of(source: &str, what: &str) -> Vec<(String, ImportKind)> {
    let package = compile_generator(source, what)
        .wir_package
        .expect("wir package retained");
    let mut entries: Vec<(String, ImportKind)> = package
        .import_plan
        .iter()
        .filter(|e| e.fq.starts_with("wasi:"))
        .map(|e| (e.fq.clone(), e.kind))
        .collect();
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    entries.dedup();
    entries
}

#[test]
fn generator_using_core_log_adds_no_wasi_import() {
    let baseline = wasi_imports_of(NOOP_GENERATOR, "noop generator");
    assert!(
        baseline.is_empty(),
        "a generator that only answers `generate` needs no WASI at all, got {baseline:?}"
    );

    // What `core:log` adds is `wasi:cli/types`, an instance exporting only an
    // `error-code` enum. A type-only import needs nothing from the linker,
    // which is why a generator carrying it still instantiates; anything
    // function-bearing would not.
    let with_log = wasi_imports_of(CORE_LOG_GENERATOR, "core:log generator");
    let provided: Vec<&(String, ImportKind)> = with_log
        .iter()
        .filter(|(_, kind)| *kind != ImportKind::SharedTypes)
        .collect();
    assert!(
        provided.is_empty(),
        "core:log added {provided:?}, a WASI import the Kiln linker never provides"
    );
    assert!(
        !with_log.is_empty(),
        "core:log imports no WASI interface at all, so this test guards nothing"
    );
}

/// Opting into a stamped sink picks up WASI without writing a `wasi:` import:
/// `WallClock::now()` is `#[ambient]`, so nothing in the generator's source
/// names the clock and only the import plan sees it.
const WALL_CLOCK_SINK_GENERATOR: &str = r#"
use { Request, Response, Error } from "core:kiln";
use { Log, TextSink, WallClock, info } from "core:log";

export fn generate(req: Request) -> Result<Response, Error> {
    let mut sink: TextSink<WallClock> = TextSink {};
    with Log => &mut sink do {
        info(`generating`, { path: req.primary.path });
    }
    return Result::Ok(Response { files: [] });
}
"#;

#[test]
fn generator_installing_a_stamped_sink_is_rejected() {
    expect_forbidden_import(
        WALL_CLOCK_SINK_GENERATOR,
        "wasi:clocks",
        "a generator reaching wasi:clocks through a sink",
    );
}
