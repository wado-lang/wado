//! The editor's view of a bad inline `options` table.
//!
//! The LSP cannot run a generator, but the `Options` struct it validates
//! against is a property of the generator's *source*, which the host can read.
//! So a typo in `with { generator: { options: … } }` squiggles the key that is
//! wrong, in the file the user is editing — not line 1, and not only under
//! `wado check`.
//!
//! Both spellings of a reachable generator are covered: an inline
//! `module: "./gen.wado"` path, and a `lib:` `[build-dependencies]` nickname
//! whose entry is a path package. A registry generator has no source to read
//! and is skipped, silently — `wado check` still reports it.

use wado_lsp::{Diagnostic, Engine, FilesystemCompilerHost, Severity};

const GENERATOR: &str = r#"use { Request, Response, OutputFile, Error } from "core:kiln";

pub struct Options {
    pub verbose: bool,
}

export fn generate(req: Request<Options>) -> Result<Response, Error> {
    let _ = req.options.verbose;
    return Result::Ok(Response {
        files: [OutputFile {
            path: "greeting.wado",
            content: "pub fn greeting() -> String { return \"hi\"; }",
            is_entry: true,
        }],
    });
}
"#;

/// The consumer, with `options` on line 5 (1-based): `options:` at column 9 and
/// the table's first key at column 20.
fn consumer(module: &str, options_table: &str) -> String {
    format!(
        r#"use {{ println, Stdout }} from "core:cli";
use {{ greeting }} from "./schema.idl" with {{
    generator: {{
        module: "{module}",
        options: {options_table},
    }},
}};

export fn run() with Stdout {{
    println(greeting());
}}
"#
    )
}

struct Fixture {
    root: tempfile::TempDir,
    uri: String,
    source: String,
}

/// A workspace whose consumer names its generator by `module`. `package` writes
/// the generator as a `[build-dependencies]` package next door; otherwise it is
/// a bare `gen.wado` beside the consumer.
fn build(module: &str, options_table: &str, package: bool) -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let app = root.join("app");
    std::fs::create_dir_all(app.join("src")).unwrap();

    let build_deps = if package {
        let gen_pkg = root.join("gen");
        std::fs::create_dir_all(gen_pkg.join("src")).unwrap();
        std::fs::write(
            gen_pkg.join("wado.toml"),
            "[package]\nname = \"gen\"\nversion = \"0.1.0\"\n\n\
             [world]\n\"core:kiln/generator\" = \"src/generator.wado\"\n",
        )
        .unwrap();
        std::fs::write(gen_pkg.join("src/generator.wado"), GENERATOR).unwrap();
        "\n[build-dependencies]\n\"lib:gen\" = { path = \"../gen\" }\n"
    } else {
        std::fs::write(app.join("src/gen.wado"), GENERATOR).unwrap();
        ""
    };

    std::fs::write(
        app.join("wado.toml"),
        format!(
            "[package]\nname = \"app\"\nversion = \"0.1.0\"\n\n\
             [world]\n\"wasi:cli/command\" = \"src/main.wado\"\n{build_deps}"
        ),
    )
    .unwrap();
    std::fs::write(app.join("src/schema.idl"), "anything\n").unwrap();

    let source = consumer(module, options_table);
    let entry = app.join("src/main.wado");
    std::fs::write(&entry, &source).unwrap();

    Fixture {
        root: tmp,
        uri: format!("file://{}", entry.display()),
        source,
    }
}

fn diagnostics(fixture: &Fixture) -> Vec<Diagnostic> {
    let host = FilesystemCompilerHost::new(fixture.root.path().join("app"));
    let mut engine = Engine::new();
    engine.open_document(&fixture.uri, fixture.source.clone());
    futures::executor::block_on(engine.diagnostics(&fixture.uri, &host))
}

fn options_errors(diags: &[Diagnostic]) -> Vec<&Diagnostic> {
    diags
        .iter()
        .filter(|d| d.code == "GENERATOR_OPTIONS_INVALID")
        .collect()
}

/// LSP ranges are 0-based, so line 5 column 20 of the source is `(4, 19)`.
fn at(d: &Diagnostic) -> (u32, u32) {
    (d.range.start.line, d.range.start.character)
}

#[test]
fn unknown_key_squiggles_the_key() {
    let fixture = build("./gen.wado", "{ verbsoe: false }", false);
    let diags = diagnostics(&fixture);
    let errors = options_errors(&diags);

    let unknown = errors
        .iter()
        .find(|d| {
            d.message
                .contains("unknown options field `options.verbsoe`")
        })
        .unwrap_or_else(|| panic!("expected an unknown-field error, got {diags:#?}"));
    assert_eq!(unknown.severity, Severity::Error);
    assert_eq!(at(unknown), (4, 19));

    let missing = errors
        .iter()
        .find(|d| {
            d.message
                .contains("required options field `options.verbose`")
        })
        .unwrap_or_else(|| panic!("expected a missing-field error, got {diags:#?}"));
    assert_eq!(
        at(missing),
        (4, 8),
        "a missing field blames the options key"
    );
}

#[test]
fn build_dependency_generator_is_read_from_its_package() {
    let fixture = build("lib:gen", "{ verbsoe: false }", true);
    let diags = diagnostics(&fixture);

    let unknown = options_errors(&diags)
        .into_iter()
        .find(|d| {
            d.message
                .contains("unknown options field `options.verbsoe`")
        })
        .unwrap_or_else(|| panic!("expected an unknown-field error, got {diags:#?}"));
    assert_eq!(at(unknown), (4, 19));
}

#[test]
fn a_wrong_type_squiggles_its_own_key() {
    let fixture = build("./gen.wado", "{ verbose: 1 }", false);
    let diags = diagnostics(&fixture);

    let mismatch = options_errors(&diags)
        .into_iter()
        .find(|d| {
            d.message
                .contains("`options.verbose` expected bool, got integer")
        })
        .unwrap_or_else(|| panic!("expected a type-mismatch error, got {diags:#?}"));
    assert_eq!(at(mismatch), (4, 19));
}

#[test]
fn a_valid_options_table_reports_nothing() {
    let fixture = build("./gen.wado", "{ verbose: true }", false);
    let diags = diagnostics(&fixture);

    assert!(
        options_errors(&diags).is_empty(),
        "a valid table must not be reported, got {diags:#?}",
    );
}

/// A registry generator has no source on disk, so the LSP has no descriptor and
/// says nothing about its options rather than guessing they are wrong.
#[test]
fn an_unreachable_generator_is_silent() {
    let fixture = build("fake:gen@0.1", "{ verbsoe: false }", false);
    let diags = diagnostics(&fixture);

    assert!(
        options_errors(&diags).is_empty(),
        "no descriptor means no verdict, got {diags:#?}",
    );
}

/// The generator's own compile must not leak into the consumer's diagnostics:
/// its errors belong to its own file, and a span-less one would otherwise
/// anchor at line 1 of a file it says nothing about.
#[test]
fn a_broken_generator_reports_nothing_on_the_consumer() {
    let fixture = build("./gen.wado", "{ verbose: true }", false);
    let generator = fixture.root.path().join("app/src/gen.wado");
    std::fs::write(&generator, "use { Missing } from \"./nowhere.wado\";\n").unwrap();

    let diags = diagnostics(&fixture);

    assert!(
        !diags.iter().any(|d| d.message.contains("nowhere.wado")),
        "the generator's own failure must stay off the consumer, got {diags:#?}",
    );
}
