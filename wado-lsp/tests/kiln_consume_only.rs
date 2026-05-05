//! Integration tests for `Engine`'s consume-only Kiln support.
//!
//! Pins the M7 contract from WEP 2026-04-12 §"Transitional consume-only
//! mode" / §"Caching and the `<primary>.kiln.json` cache file":
//!
//! - **Cache hit** — `<output_dir>/<primary>.kiln.json` exists and every
//!   recorded hash still matches what is on disk. `Engine::diagnostics`
//!   returns no `KilnStaleCache` warning, the redirect from
//!   `use { ... } from "./schema.g4"` to the generated entry module
//!   fires, and downstream symbol resolution sees the `.wado` file.
//! - **Cache miss** — the schema's bytes drifted from the recorded
//!   hash. `Engine::diagnostics` surfaces a `KilnStaleCache` warning,
//!   the redirect does *not* fire, and the LSP writes nothing back to
//!   `<output_dir>` (a follow-up `wado compile` natively will refresh
//!   the cache).
//!
//! The fixture is built programmatically inside a `tempdir()` so the
//! test owns every byte that contributes to a cache hash. Committed
//! fixtures would force us to re-encode hashes whenever the schema
//! body changes.

use std::path::Path;

use wado_compiler::kiln::metadata::{
    FileHash, METADATA_VERSION, Metadata, OutputEntry, metadata_filename,
};
use wado_compiler::kiln::{content_hash, hex_digest};
use wado_lsp::{Engine, FilesystemCompilerHost, Severity};

const SCHEMA_BODY: &str = "// dummy calc grammar — body content is irrelevant\n";
const GENERATED_BODY: &str = "#![generated(by = \"fake:gen@0.1\", sources = [\"grammars/calc.g4\"])]\n\
     pub fn parse() -> i32 { return 42; }\n";

fn entry_source() -> &'static str {
    "use { parse } from \"./grammars/calc.g4\" with {\n    \
     generator: {\n        \
     module: \"fake:gen@0.1\",\n        \
     output_dir: \"tests/generated\",\n    \
     },\n\
     };\n\
     fn run() {\n    \
     let _x = parse();\n\
     }\n"
}

fn hex_sha256(bytes: &[u8]) -> String {
    hex_digest(&content_hash(bytes))
}

struct Fixture {
    root: tempfile::TempDir,
    entry_uri: String,
}

/// Build a fresh consume-only workspace under `tempdir()`.
///
/// `schema_on_disk` is the bytes the user is currently looking at. The
/// metadata always pins the hash of [`SCHEMA_BODY`], so writing a
/// different value here simulates a post-generation edit and lets the
/// test request a cache-miss without rebuilding the metadata file.
fn build_fixture(schema_on_disk: &str) -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();

    std::fs::write(
        root.join("wado.toml"),
        "[package]\nname = \"kiln-lsp-test\"\nversion = \"0.0.0\"\n",
    )
    .unwrap();

    let schema_dir = root.join("grammars");
    std::fs::create_dir_all(&schema_dir).unwrap();
    let schema_path = schema_dir.join("calc.g4");
    std::fs::write(&schema_path, schema_on_disk).unwrap();

    let gen_dir = root.join("tests/generated");
    std::fs::create_dir_all(&gen_dir).unwrap();
    std::fs::write(gen_dir.join("calc.wado"), GENERATED_BODY).unwrap();

    // The metadata pins `SCHEMA_BODY`'s hash, even when the on-disk
    // copy carries different bytes — that's how we simulate drift.
    let primary_hash = hex_sha256(SCHEMA_BODY.as_bytes());
    let generated_hash = hex_sha256(GENERATED_BODY.as_bytes());
    // No `OptionsDescriptor` reaches the LSP, so `options_canonical` is
    // empty and its hash is the SHA-256 of an empty input.
    let options_hash = hex_sha256(&[]);

    let metadata = Metadata {
        version: METADATA_VERSION,
        invocation: "kiln-test".to_string(),
        generator: "fake:gen@0.1".to_string(),
        generator_source_hash: String::new(),
        primary: FileHash {
            path: "grammars/calc.g4".to_string(),
            hash: primary_hash,
        },
        inputs: Vec::new(),
        reads: Vec::new(),
        options_hash,
        outputs: vec![OutputEntry {
            path: "tests/generated/calc.wado".to_string(),
            hash: generated_hash,
            entry: true,
        }],
    };

    let metadata_path = gen_dir.join(metadata_filename("grammars/calc.g4"));
    std::fs::write(
        &metadata_path,
        serde_json::to_string_pretty(&metadata).unwrap(),
    )
    .unwrap();

    let entry_path = root.join("main.wado");
    std::fs::write(&entry_path, entry_source()).unwrap();

    Fixture {
        root: tmp,
        entry_uri: format!("file://{}", entry_path.display()),
    }
}

fn engine_with(fixture: &Fixture) -> (Engine, FilesystemCompilerHost) {
    let host = FilesystemCompilerHost::new(fixture.root.path().to_path_buf());
    let mut engine = Engine::new();
    engine.open_document(&fixture.entry_uri, entry_source().to_string());
    (engine, host)
}

fn has_kiln_stale_cache_warning(diags: &[wado_lsp::Diagnostic]) -> bool {
    diags
        .iter()
        .any(|d| d.severity == Severity::Warning && d.code == "KILN_STALE_CACHE")
}

fn errors(diags: &[wado_lsp::Diagnostic]) -> Vec<&wado_lsp::Diagnostic> {
    diags
        .iter()
        .filter(|d| d.severity == Severity::Error)
        .collect()
}

#[test]
fn cache_hit_does_not_warn_and_redirects() {
    futures::executor::block_on(async {
        let fixture = build_fixture(SCHEMA_BODY);
        let (engine, host) = engine_with(&fixture);

        let diags = engine.diagnostics(&fixture.entry_uri, &host).await;

        assert!(
            !has_kiln_stale_cache_warning(&diags),
            "expected no KilnStaleCache warning on a fresh cache, got {diags:#?}",
        );
        // The schema's hash matched, so the redirect should fire and
        // `parse` should resolve cleanly. A regression in the redirect
        // would surface as an unresolved-symbol error here.
        assert!(
            errors(&diags).is_empty(),
            "expected clean compile when the cache is fresh, got {:#?}",
            errors(&diags),
        );
    });
}

#[test]
fn cache_miss_emits_stale_warning() {
    futures::executor::block_on(async {
        // The on-disk schema differs from what the metadata pinned, so
        // the LSP must classify the invocation as stale.
        let fixture = build_fixture("// edited grammar — hash will not match\n");
        let (engine, host) = engine_with(&fixture);

        let diags = engine.diagnostics(&fixture.entry_uri, &host).await;

        assert!(
            has_kiln_stale_cache_warning(&diags),
            "expected a KilnStaleCache warning on schema drift, got {diags:#?}",
        );
    });
}

#[test]
fn cache_miss_does_not_write_back() {
    futures::executor::block_on(async {
        let fixture = build_fixture("// edited grammar\n");
        let metadata_path = fixture
            .root
            .path()
            .join("tests/generated")
            .join(metadata_filename("grammars/calc.g4"));
        let before = std::fs::read(&metadata_path).unwrap();

        let (engine, host) = engine_with(&fixture);
        let _ = engine.diagnostics(&fixture.entry_uri, &host).await;

        // Consume-only mode is read-only by contract.
        let after = std::fs::read(&metadata_path).unwrap();
        assert_eq!(before, after, "consume-only LSP must not rewrite the cache");
        assert!(
            metadata_check_no_orphan_writes(fixture.root.path()),
            "consume-only LSP must not create stray files under the workspace",
        );
    });
}

/// Sanity-check: after a consume-only diagnostic pass the workspace
/// must contain only the files we wrote in `build_fixture`. Catches
/// accidental regressions where the LSP path picks up a CLI-only write
/// helper.
fn metadata_check_no_orphan_writes(root: &Path) -> bool {
    let mut entries: Vec<String> = walk_files(root)
        .into_iter()
        .map(|p| {
            p.strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/")
        })
        .collect();
    entries.sort();

    let expected: Vec<String> = vec![
        "grammars/calc.g4".to_string(),
        "main.wado".to_string(),
        "tests/generated/calc.g4.kiln.json".to_string(),
        "tests/generated/calc.wado".to_string(),
        "wado.toml".to_string(),
    ];
    entries == expected
}

fn walk_files(root: &Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    walk(root, &mut out);
    out
}

fn walk(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in rd.flatten() {
        let p = entry.path();
        if p.is_dir() {
            walk(&p, out);
        } else {
            out.push(p);
        }
    }
}
