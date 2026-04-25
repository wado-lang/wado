//! Test that the loader's Kiln invocation redirect correctly rewrites a
//! `use { X } from "<schema>"` clause to the generator's emitted entry
//! module.

#![allow(unused_crate_dependencies)]

use std::sync::Mutex;

use indexmap::IndexMap;
use wado_compiler::{
    CompilerHost, Diagnostic, ModuleSource, SourceError, annotate_with_invocations,
    kiln::{DeclScope, InvocationIndex},
};

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
        DeclScope::LocalTo("entry.wado".to_string()),
        "./sample.proto",
        "build/kiln/test-invocation/sample.wado",
    );

    let annotated = if let Ok(a) = block_on(annotate_with_invocations(
        entry,
        &host,
        Some("entry.wado"),
        idx,
    )) {
        a
    } else {
        let diags = host.diagnostics.lock().unwrap().clone();
        panic!(
            "annotate failed; diagnostics: {:#?}",
            diags.iter().map(|d| &d.message).collect::<Vec<_>>()
        );
    };

    let redirected = ModuleSource::Redirected {
        uri: "build/kiln/test-invocation/sample.wado".to_string(),
    };
    assert!(
        annotated.modules.contains_key(&redirected),
        "loader should have loaded the generated entry module, got: {:?}",
        annotated.modules.keys().collect::<Vec<_>>()
    );
}

#[test]
fn empty_invocation_index_preserves_default_resolution() {
    let entry = r"
export fn run() {}
";
    let host = MapHost::new(&[]);
    let idx = InvocationIndex::new();
    let annotated = block_on(annotate_with_invocations(
        entry,
        &host,
        Some("entry.wado"),
        idx,
    ))
    .unwrap();
    let entry_ms = ModuleSource::EntryPoint {
        filename: "entry.wado".to_string(),
    };
    assert!(annotated.modules.contains_key(&entry_ms));
}
