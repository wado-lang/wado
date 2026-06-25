//! A physical module reached through several import paths must be interned
//! under one canonical identity, so the loader loads it once.
//!
//! Regression for #1423: an import that escapes *above* the entry directory and
//! re-enters (`../src/gen/parser.wado` for what the entry imports directly as
//! `./gen/parser.wado`) used to intern a second `ModuleSource` for the same
//! file, double-loading it (duplicate symbols / type identities) and breaking
//! the Kiln redirect on that path. Canonical identities collapse both spellings
//! onto `./gen/parser.wado`.

#![allow(unused_crate_dependencies)]

use std::sync::Mutex;

use indexmap::IndexMap;
use wado_compiler::{
    CompilerHost, Diagnostic, LogLevel, ModuleSource, SourceError, kiln::InvocationIndex, load,
    parse, semantics_of,
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
fn escape_reentry_import_loads_one_module() {
    // The host keys files by the path the loader hands `load_source`. The entry
    // is `p/src/main.wado`; `parser.wado` sits under the entry dir and `helper`
    // sits *above* it. `helper` re-imports `parser` via a `../`-chain.
    let host = MapHost::new(&[
        (
            "p/src/main.wado",
            "use { parse_it } from \"./gen/parser.wado\";\n\
             use { use_helper } from \"../shared/helper.wado\";\n\
             export fn run() { let _ = parse_it(); let _ = use_helper(); }\n",
        ),
        (
            "./gen/parser.wado",
            "pub struct Ast { pub n: i32 }\npub fn parse_it() -> Ast { return Ast { n: 1 } }\n",
        ),
        (
            "../shared/helper.wado",
            "use { Ast, parse_it } from \"../src/gen/parser.wado\";\n\
             pub fn use_helper() -> i32 { let a: Ast = parse_it(); return a.n }\n",
        ),
    ]);

    let loaded = block_on(async {
        let parsed = parse(host.sources.get("p/src/main.wado").unwrap());
        load(
            parsed,
            Some("p/src/main.wado"),
            &host,
            InvocationIndex::new(),
            LogLevel::default(),
        )
        .await
        .expect("loader should succeed")
    });

    // `parser.wado` must appear once — under the canonical `./gen/parser.wado`,
    // never additionally as `../src/gen/parser.wado`.
    let parser_ids: Vec<&str> = loaded
        .modules
        .keys()
        .filter_map(|ms| match ms {
            ModuleSource::Local { path } if path.ends_with("gen/parser.wado") => {
                Some(path.as_str())
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        parser_ids,
        vec!["./gen/parser.wado"],
        "the shared module must be interned once under its canonical identity",
    );
    assert!(
        !loaded.modules.keys().any(
            |ms| matches!(ms, ModuleSource::Local { path } if path == "../src/gen/parser.wado")
        ),
        "the non-canonical escape-reentry spelling must not intern a second module",
    );
}

#[test]
fn entry_escape_reentry_loads_once() {
    // The entry itself spells an import with a redundant `..`-escape that
    // re-enters its own directory; it must canonicalize to the direct identity,
    // not intern a second copy (the entry branch is canonicalized too).
    let host = MapHost::new(&[
        (
            "p/src/main.wado",
            "use { p } from \"../src/gen/parser.wado\";\n\
             use { q } from \"./gen/parser.wado\";\n\
             export fn run() { let _ = p(); let _ = q(); }\n",
        ),
        (
            "./gen/parser.wado",
            "pub fn p() -> i32 { return 1 }\npub fn q() -> i32 { return 2 }\n",
        ),
    ]);

    let loaded = block_on(async {
        let parsed = parse(host.sources.get("p/src/main.wado").unwrap());
        load(
            parsed,
            Some("p/src/main.wado"),
            &host,
            InvocationIndex::new(),
            LogLevel::default(),
        )
        .await
        .expect("loader should succeed")
    });

    let parser_count = loaded
        .modules
        .keys()
        .filter(
            |ms| matches!(ms, ModuleSource::Local { path } if path.ends_with("gen/parser.wado")),
        )
        .count();
    assert_eq!(
        parser_count,
        1,
        "the entry's escape-reentry import must not double-load: {:?}",
        loaded.modules.keys().collect::<Vec<_>>(),
    );
}

#[test]
fn imported_global_resolves_through_escape_reentry() {
    // A module above the entry dir imports a GLOBAL from a module it reaches via
    // a `..`-chain that re-enters. The elaborator's imported-global resolution
    // must canonicalize like the loader, or the global is looked up under an
    // identity absent from the loaded modules and silently dropped.
    let host = MapHost::new(&[
        (
            "p/src/main.wado",
            "use { uses_g } from \"../shared/helper.wado\";\n\
             use { p } from \"./gen/parser.wado\";\n\
             export fn run() { let _ = uses_g(); let _ = p(); }\n",
        ),
        (
            "./gen/parser.wado",
            "pub global ANSWER: i32 = 42;\npub fn p() -> i32 { return ANSWER }\n",
        ),
        (
            "../shared/helper.wado",
            "use { ANSWER } from \"../src/gen/parser.wado\";\n\
             pub fn uses_g() -> i32 { return ANSWER }\n",
        ),
    ]);

    let sem = block_on(async {
        let parsed = parse(host.sources.get("p/src/main.wado").unwrap());
        let loaded = load(
            parsed,
            Some("p/src/main.wado"),
            &host,
            InvocationIndex::new(),
            LogLevel::default(),
        )
        .await
        .expect("loader should succeed");
        semantics_of(loaded, &host, LogLevel::default(), true)
    });

    assert!(
        sem.is_complete(),
        "imported global through escape-reentry must resolve; diagnostics: {:#?}",
        host.diagnostics
            .lock()
            .unwrap()
            .iter()
            .map(|d| &d.message)
            .collect::<Vec<_>>(),
    );
}
