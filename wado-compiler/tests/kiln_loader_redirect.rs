//! Test that the loader's Kiln invocation redirect correctly rewrites a
//! `use { X } from "<schema>"` clause to the generator's emitted entry
//! module.

#![allow(unused_crate_dependencies)]

use std::sync::Mutex;

use indexmap::IndexMap;
use wado_compiler::{
    CompilerHost, Diagnostic, LogLevel, Semantics, SourceError, kiln::InvocationIndex, load, parse,
    semantics_of,
};

/// Run the three-stage frontend (parse → load → `semantics_of`) with the
/// given kiln invocation index. Test-local helper that mirrors the
/// shape of `Engine::snapshot`'s build path.
fn build_with_invocations(
    source: &str,
    filename: &str,
    host: &impl CompilerHost,
    invocations: InvocationIndex,
) -> Semantics {
    block_on(async {
        let parsed = parse(source);
        let loaded = load(
            parsed,
            Some(filename),
            host,
            invocations,
            LogLevel::default(),
        )
        .await
        .expect("loader should succeed in this fixture");
        semantics_of(loaded, host, LogLevel::default())
    })
}

struct MapHost {
    sources: IndexMap<String, String>,
    diagnostics: Mutex<Vec<Diagnostic>>,
}

impl MapHost {
    fn new(sources: &[(&str, &str)]) -> Self {
        Self {
            sources: sources
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect(),
            diagnostics: Mutex::new(Vec::new()),
        }
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

fn block_on<F: std::future::Future>(future: F) -> F::Output {
    tokio::runtime::Runtime::new().unwrap().block_on(future)
}

#[test]
fn invocation_index_redirects_use_from_schema_to_generated_entry() {
    let entry = r#"
use { greet } from "./sample.proto";

export fn run() {
    greet();
}
"#;
    let generated = r"
pub fn greet() {}
";
    let host = MapHost::new(&[("build/kiln/test-invocation/sample.wado", generated)]);

    let mut idx = InvocationIndex::new();
    idx.insert(
        "entry.wado",
        "./sample.proto",
        "build/kiln/test-invocation/sample.wado",
    );

    let sem = build_with_invocations(entry, "entry.wado", &host, idx);
    if !sem.is_complete() {
        let diags = host.diagnostics.lock().unwrap().clone();
        panic!(
            "semantics did not complete; diagnostics: {:#?}",
            diags.iter().map(|d| &d.message).collect::<Vec<_>>()
        );
    }

    let redirected = sem
        .interner
        .borrow_mut()
        .redirected("build/kiln/test-invocation/sample.wado");
    assert!(
        sem.modules.contains_key(&redirected),
        "loader should have loaded the generated entry module, got: {:?}",
        sem.modules.keys().collect::<Vec<_>>()
    );
}

#[test]
fn empty_invocation_index_preserves_default_resolution() {
    let entry = r"
export fn run() {}
";
    let host = MapHost::new(&[]);
    let idx = InvocationIndex::new();
    let sem = build_with_invocations(entry, "entry.wado", &host, idx);
    let entry_ms = sem.interner.borrow_mut().entry_point("entry.wado");
    assert!(sem.modules.contains_key(&entry_ms));
}
