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
//! - **Cache miss** — any recorded hash drifts from disk (primary,
//!   declared input, or transitive `read-file` dependency).
//!   `Engine::diagnostics` surfaces a `KilnStaleCache` warning, the
//!   redirect does *not* fire, and the LSP writes nothing back to
//!   `<output_dir>` (a follow-up `wado compile` natively will refresh
//!   the cache).
//! - **Hit-but-modified** — every input still matches but the generated
//!   `.wado` was hand-edited after generation. The redirect still
//!   fires (the user's edit is honored) and a `KilnGeneratedModified`
//!   warning surfaces so `wado check` won't be silently bypassed.
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
const READ_BODY: &str = "// transitive lexer grammar pulled in via host::read-file\n";
const GENERATED_BODY: &str = "#![generated(by = \"fake:gen@0.1\", sources = [\"grammars/calc.g4\"])]\n\
     pub fn parse() -> i32 { return 42; }\n";

const READ_PATH: &str = "grammars/calc_lexer.g4";

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

/// Knobs the builder consults to materialize the workspace state. Each
/// scenario flips exactly the bytes (or metadata fields) it needs to
/// exercise; defaults give a clean cache hit.
struct FixtureSpec<'a> {
    /// Bytes the user is currently looking at. The metadata pins
    /// `SCHEMA_BODY`'s hash, so passing anything else simulates drift.
    schema_on_disk: &'a str,
    /// When `Some`, write the metadata with one `reads` entry pinned
    /// to `READ_BODY`'s hash and create the read file with the given
    /// bytes (so a non-default value drifts).
    read_on_disk: Option<&'a str>,
    /// Bytes of the generated `.wado` on disk. The metadata always
    /// pins `GENERATED_BODY`'s hash; passing anything else simulates a
    /// hand-edit (hit-but-modified).
    generated_on_disk: &'a str,
}

impl Default for FixtureSpec<'_> {
    fn default() -> Self {
        Self {
            schema_on_disk: SCHEMA_BODY,
            read_on_disk: None,
            generated_on_disk: GENERATED_BODY,
        }
    }
}

fn build_fixture(spec: FixtureSpec<'_>) -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();

    std::fs::write(
        root.join("wado.toml"),
        "[package]\nname = \"kiln-lsp-test\"\nversion = \"0.0.0\"\n",
    )
    .unwrap();

    let schema_dir = root.join("grammars");
    std::fs::create_dir_all(&schema_dir).unwrap();
    std::fs::write(schema_dir.join("calc.g4"), spec.schema_on_disk).unwrap();

    let mut reads = Vec::new();
    if let Some(read_bytes) = spec.read_on_disk {
        std::fs::write(root.join(READ_PATH), read_bytes).unwrap();
        reads.push(FileHash {
            path: READ_PATH.to_string(),
            hash: hex_sha256(READ_BODY.as_bytes()),
        });
    }

    let gen_dir = root.join("tests/generated");
    std::fs::create_dir_all(&gen_dir).unwrap();
    std::fs::write(gen_dir.join("calc.wado"), spec.generated_on_disk).unwrap();

    let metadata = Metadata {
        version: METADATA_VERSION,
        invocation: "kiln-test".to_string(),
        generator: "fake:gen@0.1".to_string(),
        generator_source_hash: String::new(),
        primary: FileHash {
            path: "grammars/calc.g4".to_string(),
            hash: hex_sha256(SCHEMA_BODY.as_bytes()),
        },
        inputs: Vec::new(),
        reads,
        // No `OptionsDescriptor` reaches the LSP, so `options_canonical`
        // is empty and its hash is the SHA-256 of an empty input.
        options_hash: hex_sha256(&[]),
        outputs: vec![OutputEntry {
            path: "tests/generated/calc.wado".to_string(),
            hash: hex_sha256(GENERATED_BODY.as_bytes()),
            entry: true,
        }],
    };

    std::fs::write(
        gen_dir.join(metadata_filename("grammars/calc.g4")),
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

fn has_warning(diags: &[wado_lsp::Diagnostic], code: &str) -> bool {
    diags
        .iter()
        .any(|d| d.severity == Severity::Warning && d.code == code)
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
        let fixture = build_fixture(FixtureSpec::default());
        let (engine, host) = engine_with(&fixture);

        let diags = engine.diagnostics(&fixture.entry_uri, &host).await;

        assert!(
            !has_warning(&diags, "KILN_STALE_CACHE"),
            "expected no KilnStaleCache warning on a fresh cache, got {diags:#?}",
        );
        assert!(
            !has_warning(&diags, "KILN_GENERATED_MODIFIED"),
            "expected no KilnGeneratedModified warning on a fresh cache, got {diags:#?}",
        );
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
        let fixture = build_fixture(FixtureSpec {
            schema_on_disk: "// edited grammar — hash will not match\n",
            ..FixtureSpec::default()
        });
        let (engine, host) = engine_with(&fixture);

        let diags = engine.diagnostics(&fixture.entry_uri, &host).await;

        assert!(
            has_warning(&diags, "KILN_STALE_CACHE"),
            "expected a KilnStaleCache warning on schema drift, got {diags:#?}",
        );
    });
}

#[test]
fn cache_miss_does_not_write_back() {
    futures::executor::block_on(async {
        let fixture = build_fixture(FixtureSpec {
            schema_on_disk: "// edited grammar\n",
            ..FixtureSpec::default()
        });
        let metadata_path = fixture
            .root
            .path()
            .join("tests/generated")
            .join(metadata_filename("grammars/calc.g4"));
        let before = std::fs::read(&metadata_path).unwrap();

        let (engine, host) = engine_with(&fixture);
        let _ = engine.diagnostics(&fixture.entry_uri, &host).await;

        let after = std::fs::read(&metadata_path).unwrap();
        assert_eq!(before, after, "consume-only LSP must not rewrite the cache");
        assert!(
            metadata_check_no_orphan_writes(fixture.root.path()),
            "consume-only LSP must not create stray files under the workspace",
        );
    });
}

#[test]
fn reads_drift_emits_stale_warning() {
    futures::executor::block_on(async {
        // metadata.reads pins the hash of READ_BODY, but the file on
        // disk now carries different bytes — a transitive dependency
        // changed. CLI parity: see `wado_cli::kiln_driver::cache_matches`.
        let fixture = build_fixture(FixtureSpec {
            read_on_disk: Some("// edited lexer\n"),
            ..FixtureSpec::default()
        });
        let (engine, host) = engine_with(&fixture);

        let diags = engine.diagnostics(&fixture.entry_uri, &host).await;

        assert!(
            has_warning(&diags, "KILN_STALE_CACHE"),
            "expected a KilnStaleCache warning on read-file drift, got {diags:#?}",
        );
    });
}

#[test]
fn output_modified_emits_warning_but_redirects() {
    futures::executor::block_on(async {
        // Every input still matches, but the user has hand-edited the
        // generated `.wado`. The redirect should still fire (their
        // edit is honored) and a `KilnGeneratedModified` warning must
        // surface so `wado check` won't be silently bypassed.
        let fixture = build_fixture(FixtureSpec {
            generated_on_disk: "#![generated(by = \"fake:gen@0.1\", sources = [\"grammars/calc.g4\"])]\n\
                 pub fn parse() -> i32 { return 99; }\n",
            ..FixtureSpec::default()
        });
        let (engine, host) = engine_with(&fixture);

        let diags = engine.diagnostics(&fixture.entry_uri, &host).await;

        assert!(
            has_warning(&diags, "KILN_GENERATED_MODIFIED"),
            "expected a KilnGeneratedModified warning on an edited output, got {diags:#?}",
        );
        assert!(
            !has_warning(&diags, "KILN_STALE_CACHE"),
            "an output edit must not surface as a stale-cache miss, got {diags:#?}",
        );
        assert!(
            errors(&diags).is_empty(),
            "the redirect should still fire — `parse` must resolve, got {:#?}",
            errors(&diags),
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
