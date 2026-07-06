//! Native wasmtime runtime for Kiln generators.
//!
//! Instantiates a compiled generator component, linking only the two host
//! imports (`read-file` and `emit-diagnostic`). Everything else — WASI, clocks,
//! random, http — is unlinked; a generator that imports any of those fails at
//! link time, which is the determinism guarantee.
//!
//! Revision 3: each generator has its own synthesized world (the `options` shape is
//! per-generator), so `generate` is invoked dynamically as typed `Val`s rather
//! than through a shared static binding. The invocation's validated options are
//! materialized into a typed value shaped by the component's own introspected
//! parameter type.
//!
//! See WEP 2026-04-12 (Kiln) §"Host-delegated execution".

use std::sync::{Arc, Mutex};

use anyhow::Result;
use wasmtime::component::types::Type;
use wasmtime::component::{Component, Func, HasSelf, Instance, Linker, Val};
use wasmtime::{Engine, Store};

use wado_compiler::kiln::{CanonicalOptions, CanonicalValue};
use wado_compiler::{
    CompilerHost, GeneratorDiagnostic, GeneratorDiagnosticLevel, GeneratorError,
    GeneratorInputFile, GeneratorOutputFile, GeneratorReadRecord, GeneratorRequest,
    GeneratorResponse, GeneratorRunnerError, GeneratorSourceSpan,
};

wasmtime::component::bindgen!({
    path: "../wado-compiler/lib/core/kiln/generator.wit",
    world: "generator",
    imports: { default: async },
    exports: { default: async },
});

use self::core::kiln::kiln_host;

/// Host policy for a single generator invocation.
///
/// `fuel` is the wasmtime fuel ceiling for the call; `0` means no ceiling
/// (the store is seeded with `u64::MAX`). The default is `0` because no
/// finite ceiling has yet proven to fit every Gale-sized grammar — the
/// 1 GiB initial pick tripped on `SQLite`. WEP 2026-04-12 (Kiln)
/// open-question #10 tracks exposing this as a `wado.toml` knob and
/// pairing it with a wall-clock deadline.
#[derive(Default)]
pub struct KilnRunPolicy {
    pub fuel: u64,
}

struct KilnHostState<H: CompilerHost> {
    host: Arc<H>,
    reads: Arc<Mutex<Vec<GeneratorReadRecord>>>,
    diagnostics: Arc<Mutex<Vec<GeneratorDiagnostic>>>,
}

impl<H: CompilerHost + 'static> kiln_host::Host for KilnHostState<H> {
    async fn read_file(&mut self, path: String) -> Result<String, kiln_host::HostError> {
        match self.host.load_source(&path).await {
            Ok(bytes) => {
                let hash = wado_compiler::kiln::content_hash(&bytes);
                self.reads.lock().unwrap().push(GeneratorReadRecord {
                    path: path.clone(),
                    content_hash: hash,
                });
                String::from_utf8(bytes)
                    .map_err(|e| kiln_host::HostError::Io(format!("{path}: not UTF-8: {e}")))
            }
            Err(wado_compiler::SourceError::NotFound { .. }) => Err(kiln_host::HostError::NotFound),
            Err(e) => Err(kiln_host::HostError::Io(e.to_string())),
        }
    }

    async fn emit_diagnostic(&mut self, diagnostic: kiln_host::Diagnostic) {
        self.diagnostics
            .lock()
            .unwrap()
            .push(lift_diagnostic(diagnostic));
    }
}

fn lift_diagnostic(d: kiln_host::Diagnostic) -> GeneratorDiagnostic {
    GeneratorDiagnostic {
        level: match d.level {
            kiln_host::DiagnosticLevel::Error => GeneratorDiagnosticLevel::Error,
            kiln_host::DiagnosticLevel::Warning => GeneratorDiagnosticLevel::Warning,
            kiln_host::DiagnosticLevel::Info => GeneratorDiagnosticLevel::Info,
            kiln_host::DiagnosticLevel::Hint => GeneratorDiagnosticLevel::Hint,
        },
        span: d.span.map(|s| GeneratorSourceSpan {
            path: s.path,
            byte_start: s.byte_start,
            byte_end: s.byte_end,
        }),
        message: d.message,
    }
}

/// Locate the generator's `generate` export by scanning the component's
/// exported interfaces. `generate` always references named `core:kiln/types`
/// records, so the compiler groups it into a synthesized default interface
/// whose FQ it owns (and may make package-specific) — scanning avoids
/// hardcoding that FQ.
fn find_generate<T>(
    component: &Component,
    engine: &Engine,
    instance: &Instance,
    store: &mut Store<T>,
) -> Result<Func, GeneratorRunnerError> {
    let interface_names: Vec<String> = component
        .component_type()
        .exports(engine)
        .map(|(name, _)| name.to_string())
        .collect();
    for name in interface_names {
        if let Some(iface) = instance.get_export_index(&mut *store, None, &name)
            && let Some(idx) = instance.get_export_index(&mut *store, Some(&iface), "generate")
            && let Some(func) = instance.get_func(&mut *store, idx)
        {
            return Ok(func);
        }
    }
    Err(GeneratorRunnerError::Host(
        "generator exports no `generate`".to_string(),
    ))
}

/// Build an `input-file` record `Val` (`{ path, content }`).
fn input_file_val(f: &GeneratorInputFile) -> Val {
    Val::Record(vec![
        ("path".to_string(), Val::String(f.path.clone())),
        ("content".to_string(), Val::String(f.content.clone())),
    ])
}

/// Lift a `response` record payload (`{ files: list<output-file> }`) into the
/// host-facing output list.
fn lift_response(payload: Option<&Val>) -> Result<Vec<GeneratorOutputFile>, GeneratorRunnerError> {
    let Some(Val::Record(fields)) = payload else {
        return Err(GeneratorRunnerError::Host(format!(
            "response is not a record: {payload:?}"
        )));
    };
    let files = record_field(fields, "files")?;
    let Val::List(items) = files else {
        return Err(GeneratorRunnerError::Host(format!(
            "response.files is not a list: {files:?}"
        )));
    };
    items.iter().map(lift_output_file_val).collect()
}

/// Lift one `output-file` record (`{ path, content, is-entry }`).
fn lift_output_file_val(v: &Val) -> Result<GeneratorOutputFile, GeneratorRunnerError> {
    let Val::Record(fields) = v else {
        return Err(GeneratorRunnerError::Host(format!(
            "output-file is not a record: {v:?}"
        )));
    };
    Ok(GeneratorOutputFile {
        path: record_string(fields, "path")?,
        content: record_string(fields, "content")?,
        is_entry: matches!(record_field(fields, "is-entry")?, Val::Bool(true)),
    })
}

/// Lift the generator's `error` variant payload into [`GeneratorError`].
fn lift_error_val(payload: Option<&Val>) -> Result<GeneratorError, GeneratorRunnerError> {
    let Some(Val::Variant(case, inner)) = payload else {
        return Err(GeneratorRunnerError::Host(format!(
            "error payload is not a variant: {payload:?}"
        )));
    };
    // Every `core:kiln/types::error` case carries a string message; an
    // unexpected payload shape or case name is a host/component contract
    // violation, surfaced rather than coerced to a dummy message or `Other`.
    let msg = match inner.as_deref() {
        Some(Val::String(s)) => s.clone(),
        other => {
            return Err(GeneratorRunnerError::Host(format!(
                "error variant `{case}` payload is not a string: {other:?}"
            )));
        }
    };
    Ok(match case.as_str() {
        "invalid-schema" => GeneratorError::InvalidSchema(msg),
        "unsupported" => GeneratorError::Unsupported(msg),
        "other" => GeneratorError::Other(msg),
        _ => {
            return Err(GeneratorRunnerError::Host(format!(
                "unknown error variant case `{case}`"
            )));
        }
    })
}

fn record_field<'a>(
    fields: &'a [(String, Val)],
    name: &str,
) -> Result<&'a Val, GeneratorRunnerError> {
    fields
        .iter()
        .find(|(n, _)| n == name)
        .map(|(_, v)| v)
        .ok_or_else(|| GeneratorRunnerError::Host(format!("missing record field `{name}`")))
}

fn record_string(fields: &[(String, Val)], name: &str) -> Result<String, GeneratorRunnerError> {
    match record_field(fields, name)? {
        Val::String(s) => Ok(s.clone()),
        other => Err(GeneratorRunnerError::Host(format!(
            "record field `{name}` is not a string: {other:?}"
        ))),
    }
}

/// Materialize the invocation's validated options into a typed [`Val`], shaped
/// by the component's own introspected options parameter type — the single
/// source of truth for widths, enum-vs-string, and field order. The option
/// values carry raw Wado identifiers, mapped to the component's CM kebab-case
/// names here.
fn options_to_val(options: &CanonicalOptions, ty: &Type) -> Result<Val, String> {
    canonical_to_val(&CanonicalValue::Struct(options.values.clone()), ty)
}

fn canonical_to_val(v: &CanonicalValue, ty: &Type) -> Result<Val, String> {
    let mismatch = || format!("options value {v:?} does not fit component type {ty:?}");
    Ok(match ty {
        Type::Bool => match v {
            CanonicalValue::Bool(b) => Val::Bool(*b),
            _ => return Err(mismatch()),
        },
        Type::S8 => Val::S8(as_i64(v).ok_or_else(mismatch)? as i8),
        Type::U8 => Val::U8(as_u64(v).ok_or_else(mismatch)? as u8),
        Type::S16 => Val::S16(as_i64(v).ok_or_else(mismatch)? as i16),
        Type::U16 => Val::U16(as_u64(v).ok_or_else(mismatch)? as u16),
        Type::S32 => Val::S32(as_i64(v).ok_or_else(mismatch)? as i32),
        Type::U32 => Val::U32(as_u64(v).ok_or_else(mismatch)? as u32),
        Type::S64 => Val::S64(as_i64(v).ok_or_else(mismatch)?),
        Type::U64 => Val::U64(as_u64(v).ok_or_else(mismatch)?),
        // Options collapse every float to an f64 (`CanonicalValue` has no f32
        // variant), so an `f32` field is narrowed here.
        Type::Float32 => match v {
            CanonicalValue::F64(f) => Val::Float32(*f as f32),
            _ => return Err(mismatch()),
        },
        Type::Float64 => match v {
            CanonicalValue::F64(f) => Val::Float64(*f),
            _ => return Err(mismatch()),
        },
        Type::String => match v {
            CanonicalValue::String(s) => Val::String(s.clone()),
            _ => return Err(mismatch()),
        },
        // Enum cases carry raw Wado names; the component enum uses CM
        // kebab-case case names, so normalize before building the Val.
        Type::Enum(_) => match v {
            CanonicalValue::Enum(s) => Val::Enum(wado_compiler::name::to_kebab(s)),
            _ => return Err(mismatch()),
        },
        Type::Option(o) => match v {
            CanonicalValue::None => Val::Option(None),
            CanonicalValue::Some(inner) => {
                Val::Option(Some(Box::new(canonical_to_val(inner, &o.ty())?)))
            }
            // A present, non-wrapped value against an Option type is `Some`.
            other => Val::Option(Some(Box::new(canonical_to_val(other, &o.ty())?))),
        },
        Type::Record(r) => {
            let CanonicalValue::Struct(entries) = v else {
                return Err(mismatch());
            };
            let mut out = Vec::new();
            for f in r.fields() {
                // Option values carry raw Wado field names; the component record
                // field is CM kebab-case.
                let matched = entries
                    .iter()
                    .find(|(name, _)| wado_compiler::name::to_kebab(name) == f.name);
                let val = match matched {
                    Some((_, cv)) => canonical_to_val(cv, &f.ty)?,
                    None if matches!(f.ty, Type::Option(_)) => Val::Option(None),
                    None => {
                        return Err(format!("missing required options field `{}`", f.name));
                    }
                };
                out.push((f.name.to_string(), val));
            }
            Val::Record(out)
        }
        other => return Err(format!("unsupported options type: {other:?}")),
    })
}

/// Read a signed integer from an options value (validation stores signed
/// fields as `I64`, unsigned as `U64`; accept either when it fits).
fn as_i64(v: &CanonicalValue) -> Option<i64> {
    match v {
        CanonicalValue::I64(n) => Some(*n),
        CanonicalValue::U64(n) => i64::try_from(*n).ok(),
        _ => None,
    }
}

fn as_u64(v: &CanonicalValue) -> Option<u64> {
    match v {
        CanonicalValue::U64(n) => Some(*n),
        CanonicalValue::I64(n) => u64::try_from(*n).ok(),
        _ => None,
    }
}

/// Build a wasmtime [`Component`] from raw bytes. Cranelift-AOT
/// dominates here (~7s on a 432KB generator), so the host caches the
/// result across invocations that share the same bytes — see
/// [`crate::compiler_host::FilesystemCompilerHost`].
pub fn compile_component(
    engine: &Engine,
    component_wasm: &[u8],
) -> Result<Component, GeneratorRunnerError> {
    Component::from_binary(engine, component_wasm)
        .map_err(|e| GeneratorRunnerError::Host(format!("component compile: {e}")))
}

/// Instantiate a pre-built [`Component`] against the
/// `core:kiln/generator` world, invoke `generate`, and return the
/// response plus the files the generator read via `host::read-file`.
/// Always pass a [`Component`] obtained from the host's cache rather
/// than building one inline — see [`compile_component`].
pub async fn run_generator<H: CompilerHost + 'static>(
    engine: &Engine,
    host: Arc<H>,
    component: &Component,
    request: GeneratorRequest,
    policy: KilnRunPolicy,
) -> (
    Result<GeneratorResponse, GeneratorRunnerError>,
    Vec<GeneratorDiagnostic>,
) {
    let reads = Arc::new(Mutex::new(Vec::<GeneratorReadRecord>::new()));
    let diagnostics = Arc::new(Mutex::new(Vec::<GeneratorDiagnostic>::new()));

    // Run the generator in an inner future that yields the typed outcome, then
    // drain the diagnostics and return them alongside it — on both the success
    // and the failure path. `emit_diagnostic` guarantees they are printed even
    // when the generator returns successfully; relaying on the error path too
    // keeps a failing generator from swallowing the diagnostics that explain
    // the failure. The only host reachable here is the collect-only inner
    // host, so the relay (print + collect) is the caller's job (see
    // `FilesystemCompilerHost::run_generator`).
    let reads_inner = reads.clone();
    let diagnostics_inner = diagnostics.clone();
    let outcome: Result<GeneratorResponse, GeneratorRunnerError> = async move {
        let state = KilnHostState {
            host: host.clone(),
            reads: reads_inner.clone(),
            diagnostics: diagnostics_inner,
        };

        // The kiln determinism guarantee (WEP 2026-04-12 §"Design
        // principles" #1) says the linker exposes only `core:kiln/kiln-
        // host`. The compiler handles the panic-path stderr elision
        // at codegen time so the generator component never imports WASI
        // in the first place.
        let mut linker: Linker<KilnHostState<H>> = Linker::new(engine);
        kiln_host::add_to_linker::<_, HasSelf<_>>(&mut linker, |s| s)
            .map_err(|e| GeneratorRunnerError::Host(format!("linker setup: {e}")))?;

        let mut store = Store::new(engine, state);
        let fuel = if policy.fuel == 0 {
            u64::MAX
        } else {
            policy.fuel
        };
        store
            .set_fuel(fuel)
            .map_err(|e| GeneratorRunnerError::Host(format!("set fuel: {e}")))?;

        let instance = linker
            .instantiate_async(&mut store, component)
            .await
            .map_err(|e| GeneratorRunnerError::Host(format!("instantiate: {e}")))?;

        let generate = find_generate(component, engine, &instance, &mut store)?;

        // `generate(primary, inputs[, options])`: the options parameter is
        // present only when the generator declares a non-empty `Options`.
        let options_ty = generate.ty(&store).params().nth(2).map(|(_, t)| t);
        let mut args: Vec<Val> = Vec::with_capacity(3);
        args.push(input_file_val(&request.primary));
        args.push(Val::List(
            request.inputs.iter().map(input_file_val).collect(),
        ));
        if let Some(ty) = &options_ty {
            let val = options_to_val(&request.options, ty)
                .map_err(|e| GeneratorRunnerError::Host(format!("options: {e}")))?;
            args.push(val);
        }

        // `call_async` drives the async task-return export to completion; its
        // error is a host/runtime failure, distinct from the generator's own
        // typed `error` in the `Val::Result` payload below.
        let mut results = [Val::Bool(false)];
        generate
            .call_async(&mut store, &args, &mut results)
            .await
            .map_err(|e| GeneratorRunnerError::Host(format!("generate call: {e}")))?;

        let [result] = results;
        match result {
            Val::Result(Ok(payload)) => Ok(GeneratorResponse {
                files: lift_response(payload.as_deref())?,
                reads: std::mem::take(&mut *reads_inner.lock().unwrap()),
            }),
            Val::Result(Err(payload)) => Err(GeneratorRunnerError::Generator(lift_error_val(
                payload.as_deref(),
            )?)),
            other => Err(GeneratorRunnerError::Host(format!(
                "generate returned a non-result value: {other:?}"
            ))),
        }
    }
    .await;

    let emitted: Vec<GeneratorDiagnostic> = diagnostics.lock().unwrap().drain(..).collect();
    (outcome, emitted)
}

pub(crate) fn relay_diagnostic<H: CompilerHost + ?Sized>(host: &H, diag: GeneratorDiagnostic) {
    use wado_compiler::{Code, Diagnostic, DiagnosticSpan, Severity};
    let severity = match diag.level {
        GeneratorDiagnosticLevel::Error => Severity::Error,
        GeneratorDiagnosticLevel::Warning => Severity::Warning,
        GeneratorDiagnosticLevel::Info => Severity::Info,
        GeneratorDiagnosticLevel::Hint => Severity::Info,
    };
    let span = diag.span.map(|s| DiagnosticSpan {
        file: s.path,
        line: 0,
        column: 0,
        end_line: None,
        end_column: None,
    });
    host.emit_diagnostic(Diagnostic {
        severity,
        code: Code::Log,
        message: diag.message,
        span,
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::create_kiln_engine;

    #[test]
    fn malformed_component_is_host_error() {
        // Cranelift AOT now happens up-front in `compile_component`
        // (so the host can cache the result across invocations); this
        // is where a malformed module surfaces, not from
        // `run_generator`.
        let engine = create_kiln_engine(wasmtime::OptLevel::Speed).expect("engine");
        match compile_component(&engine, &[0, 1, 2, 3]) {
            Err(GeneratorRunnerError::Host(msg)) => {
                assert!(msg.contains("component compile"), "msg = {msg}");
            }
            Err(other) => panic!("expected Host error, got {other:?}"),
            Ok(_) => panic!("expected error on malformed bytes, got Ok"),
        }
    }

    #[test]
    fn sha256_known_vector_empty() {
        let digest = wado_compiler::kiln::content_hash(b"");
        let expected = [
            0xe3, 0xb0, 0xc4, 0x42, 0x98, 0xfc, 0x1c, 0x14, 0x9a, 0xfb, 0xf4, 0xc8, 0x99, 0x6f,
            0xb9, 0x24, 0x27, 0xae, 0x41, 0xe4, 0x64, 0x9b, 0x93, 0x4c, 0xa4, 0x95, 0x99, 0x1b,
            0x78, 0x52, 0xb8, 0x55,
        ];
        assert_eq!(digest, expected);
    }

    #[test]
    fn sha256_known_vector_abc() {
        let digest = wado_compiler::kiln::content_hash(b"abc");
        let expected = [
            0xba, 0x78, 0x16, 0xbf, 0x8f, 0x01, 0xcf, 0xea, 0x41, 0x41, 0x40, 0xde, 0x5d, 0xae,
            0x22, 0x23, 0xb0, 0x03, 0x61, 0xa3, 0x96, 0x17, 0x7a, 0x9c, 0xb4, 0x10, 0xff, 0x61,
            0xf2, 0x00, 0x15, 0xad,
        ];
        assert_eq!(digest, expected);
    }

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    /// End-to-end: a revision-3 typed-options generator drives through the dynamic
    /// `Val` invocation — options materialized against the introspected param
    /// type, `generate` called dynamically, and the `Result<Response, Error>`
    /// lifted back. The emitted file name depends on `options.verbose`, so a
    /// correct round-trip proves the option value crossed the boundary.
    /// Minimal host: the round-trip generator loads no sources and reads no
    /// files, so `load_source` is never hit and diagnostics are dropped.
    struct NoopHost;

    impl CompilerHost for NoopHost {
        fn load_source(
            &self,
            path: &str,
        ) -> impl std::future::Future<Output = Result<Vec<u8>, wado_compiler::SourceError>> + Send
        {
            let path = path.to_string();
            async move { Err(wado_compiler::SourceError::NotFound { path }) }
        }

        fn emit_diagnostic(&self, _diagnostic: wado_compiler::Diagnostic) {}
    }

    #[test]
    fn typed_options_generator_round_trips() {
        use std::sync::Arc;
        use wado_compiler::{CompilerOptions, LogLevel, compile_with_options};

        const SRC: &str = r#"
use { Request, Response, Error, OutputFile } from "core:kiln";

pub struct Options {
    pub verbose: bool,
}

export fn generate(req: Request<Options>) -> Result<Response, Error> {
    let name = if req.options.verbose { "verbose.wado" } else { "quiet.wado" };
    return Result::Ok(Response {
        files: [OutputFile { path: name, content: "pub fn x() {}", is_entry: true }],
    });
}
"#;

        let options = CompilerOptions {
            log_level: Some(LogLevel::Warn),
            target_world: Some("core:kiln/generator".to_string()),
            ..CompilerOptions::default()
        };
        let compiled = runtime()
            .block_on(compile_with_options(
                SRC,
                &NoopHost,
                Some("generator.wado"),
                options,
            ))
            .expect("generator compiles to a component");

        let options = CanonicalOptions {
            descriptor: wado_compiler::kiln::OptionsDescriptor::default(),
            values: vec![("verbose".to_string(), CanonicalValue::Bool(true))],
        };

        let engine = create_kiln_engine(wasmtime::OptLevel::Speed).expect("engine");
        let component = compile_component(&engine, &compiled.wasm).expect("component");
        let request = GeneratorRequest {
            primary: GeneratorInputFile {
                path: "schema.txt".to_string(),
                content: "hello".to_string(),
            },
            inputs: vec![],
            options,
        };

        let (outcome, _diags) = runtime().block_on(run_generator(
            &engine,
            Arc::new(NoopHost),
            &component,
            request,
            KilnRunPolicy::default(),
        ));

        let response = outcome.expect("generator runs and returns Ok");
        assert_eq!(response.files.len(), 1, "one output file");
        assert_eq!(
            response.files[0].path, "verbose.wado",
            "options.verbose=true selects the verbose file name"
        );
        assert!(response.files[0].is_entry);
        assert_eq!(response.files[0].content, "pub fn x() {}");
    }

    /// The validated options carry raw Wado field/case names and store every
    /// float as an f64, while the component type the host maps them onto uses
    /// CM kebab-case names and `float32`. This exercises all three: a
    /// `snake_case` field (`max_depth`), an `f32` field (`ratio`), and an enum
    /// with a multi-word case (`Mode::HighContrast`). The generator only emits
    /// `ok.wado` when it sees all three option values materialized correctly.
    #[test]
    fn typed_options_round_trip_covers_kebab_and_float_widths() {
        use std::sync::Arc;
        use wado_compiler::{CompilerOptions, LogLevel, compile_with_options};

        const SRC: &str = r#"
use { Request, Response, Error, OutputFile } from "core:kiln";

pub enum Mode {
    Fast,
    HighContrast,
}

pub struct Options {
    pub max_depth: i32,
    pub ratio: f32,
    pub mode: Mode,
}

export fn generate(req: Request<Options>) -> Result<Response, Error> {
    let ok = req.options.max_depth == 3
        && req.options.ratio > 0.4
        && req.options.mode matches { Mode::HighContrast };
    let name = if ok { "ok.wado" } else { "bad.wado" };
    return Result::Ok(Response {
        files: [OutputFile { path: name, content: "pub fn x() {}", is_entry: true }],
    });
}
"#;

        let options = CompilerOptions {
            log_level: Some(LogLevel::Warn),
            target_world: Some("core:kiln/generator".to_string()),
            ..CompilerOptions::default()
        };
        let compiled = runtime()
            .block_on(compile_with_options(
                SRC,
                &NoopHost,
                Some("generator.wado"),
                options,
            ))
            .expect("generator compiles to a component");

        // Exactly what the driver's validation produces — raw Wado names, an
        // f64 for the f32 field, the enum by its raw case name.
        let options = CanonicalOptions {
            descriptor: wado_compiler::kiln::OptionsDescriptor::default(),
            values: vec![
                ("max_depth".to_string(), CanonicalValue::I64(3)),
                ("ratio".to_string(), CanonicalValue::F64(0.5)),
                (
                    "mode".to_string(),
                    CanonicalValue::Enum("HighContrast".to_string()),
                ),
            ],
        };

        let engine = create_kiln_engine(wasmtime::OptLevel::Speed).expect("engine");
        let component = compile_component(&engine, &compiled.wasm).expect("component");
        let request = GeneratorRequest {
            primary: GeneratorInputFile {
                path: "schema.txt".to_string(),
                content: "hello".to_string(),
            },
            inputs: vec![],
            options,
        };

        let (outcome, _diags) = runtime().block_on(run_generator(
            &engine,
            Arc::new(NoopHost),
            &component,
            request,
            KilnRunPolicy::default(),
        ));

        let response = outcome.expect("generator runs and returns Ok");
        assert_eq!(response.files.len(), 1);
        assert_eq!(
            response.files[0].path, "ok.wado",
            "all three option values (snake_case i32, f32, multi-word enum case) must decode"
        );
    }
}
