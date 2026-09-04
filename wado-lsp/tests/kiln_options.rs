//! The editor's view of a bad inline `options` table: a typo squiggles the key
//! that is wrong, with no `wado compile` and no generator run in between.

use std::path::Path;

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

    write_entry(tmp, &app, consumer(module, options_table))
}

fn write_entry(tmp: tempfile::TempDir, app: &Path, source: String) -> Fixture {
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

/// The one options error whose message contains `needle`, and the 0-based
/// `(line, character)` it squiggles. Line 5 column 20 is `(4, 19)`.
fn error_at(diags: &[Diagnostic], needle: &str) -> (u32, u32) {
    let found = options_errors(diags)
        .into_iter()
        .find(|d| d.message.contains(needle))
        .unwrap_or_else(|| panic!("expected an options error matching {needle:?}, got {diags:#?}"));
    assert_eq!(found.severity, Severity::Error);
    (found.range.start.line, found.range.start.character)
}

#[test]
fn unknown_key_squiggles_the_key() {
    let fixture = build("./gen.wado", "{ verbsoe: false }", false);
    let diags = diagnostics(&fixture);

    assert_eq!(
        error_at(&diags, "unknown options field `options.verbsoe`"),
        (4, 19)
    );
    assert_eq!(
        error_at(&diags, "required options field `options.verbose`"),
        (4, 8),
        "a missing field blames the options key"
    );
}

#[test]
fn build_dependency_generator_is_read_from_its_package() {
    let fixture = build("lib:gen", "{ verbsoe: false }", true);
    let diags = diagnostics(&fixture);

    assert_eq!(
        error_at(&diags, "unknown options field `options.verbsoe`"),
        (4, 19)
    );
}

#[test]
fn a_wrong_type_squiggles_its_own_key() {
    let fixture = build("./gen.wado", "{ verbose: 1 }", false);
    let diags = diagnostics(&fixture);

    assert_eq!(
        error_at(&diags, "`options.verbose` expected bool, got integer"),
        (4, 19)
    );
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

/// Two clauses through one generator describe it once and still answer for
/// themselves: the shared descriptor must not cost the second clause its
/// diagnostic, nor move it onto the first clause's key.
#[test]
fn two_clauses_through_one_generator_each_get_their_own_error() {
    let fixture = build("./gen.wado", "{ verbose: true }", false);
    let app = fixture.root.path().join("app");
    std::fs::write(app.join("src/other.idl"), "anything\n").unwrap();
    let fixture = write_entry(
        fixture.root,
        &app,
        r#"use { greeting } from "./schema.idl" with {
    generator: { module: "./gen.wado", options: { verbsoe: true } },
};
use { other } from "./other.idl" with {
    generator: { module: "./gen.wado", options: { verbsoe: false } },
};
"#
        .to_string(),
    );

    let diags = diagnostics(&fixture);
    let unknown: Vec<(u32, u32)> = options_errors(&diags)
        .iter()
        .filter(|d| {
            d.message
                .contains("unknown options field `options.verbsoe`")
        })
        .map(|d| (d.range.start.line, d.range.start.character))
        .collect();

    assert_eq!(unknown, vec![(1, 50), (4, 50)], "got {diags:#?}");
}
