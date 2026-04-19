//! Integration tests for `Engine::definition` — LSP go-to-definition coverage
//! across parameters, locals, items, types, method calls, imports, effects,
//! and file-path jumps.

use indexmap::IndexMap;
use wado_compiler::{CompilerHost, Diagnostic as CompilerDiagnostic, SourceError};
use wado_lsp::{DefinitionResult, Engine, Position};

struct TestHost {
    sources: IndexMap<String, Vec<u8>>,
}

impl TestHost {
    fn new(path: &str, source: &str) -> Self {
        let mut sources = IndexMap::new();
        sources.insert(path.to_string(), source.as_bytes().to_vec());
        Self { sources }
    }

    fn with_files(files: &[(&str, &str)]) -> Self {
        let mut sources = IndexMap::new();
        for (path, source) in files {
            sources.insert((*path).to_string(), source.as_bytes().to_vec());
        }
        Self { sources }
    }
}

impl CompilerHost for TestHost {
    async fn load_source(&self, path: &str) -> Result<Vec<u8>, SourceError> {
        if let Some(b) = self.sources.get(path) {
            return Ok(b.clone());
        }
        Err(SourceError::NotFound {
            path: path.to_string(),
        })
    }

    fn emit_diagnostic(&self, _diagnostic: CompilerDiagnostic) {}
}

async fn def_at(source: &str, line: u32, character: u32) -> Option<DefinitionResult> {
    let path = "/test.wado";
    let uri = format!("file://{path}");
    let host = TestHost::new(path, source);
    let mut engine = Engine::new();
    engine.open_document(&uri, source.to_string());
    engine
        .definition(&uri, Position { line, character }, &host)
        .await
}

async fn def_at_in(
    files: &[(&str, &str)],
    entry: &str,
    line: u32,
    character: u32,
) -> Option<DefinitionResult> {
    let uri = format!("file://{entry}");
    let host = TestHost::with_files(files);
    let entry_source = files
        .iter()
        .find(|(p, _)| *p == entry)
        .map(|(_, s)| *s)
        .expect("entry file present");
    let mut engine = Engine::new();
    engine.open_document(&uri, entry_source.to_string());
    engine
        .definition(&uri, Position { line, character }, &host)
        .await
}

fn assert_range(result: &DefinitionResult, line: u32, start: u32, end: u32) {
    assert_eq!(result.range.start.line, line, "start line");
    assert_eq!(result.range.start.character, start, "start char");
    assert_eq!(result.range.end.line, line, "end line");
    assert_eq!(result.range.end.character, end, "end char");
}

#[test]
fn param_definition() {
    futures::executor::block_on(async {
        let source = "fn add(a: i32, b: i32) -> i32 {\n    return a + b;\n}\n";
        let result = def_at(source, 1, 11)
            .await
            .expect("definition of a in body");
        assert_eq!(result.range.start.line, 0);
        assert_eq!(result.range.start.character, 7);
        assert_eq!(result.range.end.character, 8);
    });
}

#[test]
fn local_var_definition() {
    futures::executor::block_on(async {
        let source = "fn f() -> i32 {\n    let x: i32 = 1;\n    return x;\n}\n";
        let result = def_at(source, 2, 11)
            .await
            .expect("definition of x in return");
        assert_eq!(result.range.start.line, 1);
        assert_eq!(result.range.start.character, 8);
        assert_eq!(result.range.end.character, 9);
    });
}

#[test]
fn shadow_resolution() {
    futures::executor::block_on(async {
        let source = "fn f() -> i32 {\n    let x = 1;\n    let x = x + 1;\n    return x;\n}\n";
        let result = def_at(source, 2, 12)
            .await
            .expect("RHS x resolves to outer let");
        assert_eq!(result.range.start.line, 1);
        assert_eq!(result.range.start.character, 8);
        assert_eq!(result.range.end.character, 9);
    });
}

#[test]
fn item_definition() {
    futures::executor::block_on(async {
        let source =
            "fn helper() -> i32 {\n    return 1;\n}\nfn run() -> i32 {\n    return helper();\n}\n";
        let result = def_at(source, 4, 11)
            .await
            .expect("call-site resolves to fn helper");
        assert_eq!(result.range.start.line, 0);
        assert_eq!(result.range.start.character, 3);
        assert_eq!(result.range.end.character, 9);
    });
}

#[test]
fn struct_destructuring_binding_definition() {
    futures::executor::block_on(async {
        let source = concat!(
            "struct Point { x: i32, y: i32 }\n",
            "fn f(p: Point) -> i32 {\n",
            "    let { x, y } = p;\n",
            "    return x + y;\n",
            "}\n",
        );
        let result = def_at(source, 3, 11).await.expect("use of destructured x");
        assert_eq!(result.range.start.line, 2);
        assert_eq!(result.range.start.character, 10);
        assert_eq!(result.range.end.character, 11);
    });
}

#[test]
fn tuple_destructuring_binding_definition() {
    futures::executor::block_on(async {
        let source = concat!(
            "fn f() -> i32 {\n",
            "    let [a, b] = [1, 2];\n",
            "    return a + b;\n",
            "}\n",
        );
        let result = def_at(source, 2, 11).await.expect("use of a");
        assert_eq!(result.range.start.line, 1);
        assert_eq!(result.range.start.character, 9);
        assert_eq!(result.range.end.character, 10);
    });
}

#[test]
fn closure_param_definition() {
    futures::executor::block_on(async {
        let source = concat!(
            "fn f() -> i32 {\n",
            "    let g = |x: i32| x + 1;\n",
            "    return g(1);\n",
            "}\n",
        );
        let result = def_at(source, 1, 21).await.expect("use of x in body");
        assert_eq!(result.range.start.line, 1);
        assert_eq!(result.range.start.character, 13);
        assert_eq!(result.range.end.character, 14);
    });
}

#[test]
fn closure_capture_definition() {
    futures::executor::block_on(async {
        let source = concat!(
            "fn f() -> i32 {\n",
            "    let outer = 10;\n",
            "    let g = |x: i32| x + outer;\n",
            "    return g(1);\n",
            "}\n",
        );
        let result = def_at(source, 2, 25).await.expect("capture of outer");
        assert_eq!(result.range.start.line, 1);
        assert_eq!(result.range.start.character, 8);
        assert_eq!(result.range.end.character, 13);
    });
}

#[test]
fn if_let_binding_definition() {
    futures::executor::block_on(async {
        let source = concat!(
            "fn f(opt: Option<i32>) -> i32 {\n",
            "    if let Some(v) = opt {\n",
            "        return v;\n",
            "    }\n",
            "    return 0;\n",
            "}\n",
        );
        let result = def_at(source, 2, 15).await.expect("use of v");
        assert_eq!(result.range.start.line, 1);
        assert_eq!(result.range.start.character, 16);
        assert_eq!(result.range.end.character, 17);
    });
}

#[test]
fn match_arm_binding_definition() {
    futures::executor::block_on(async {
        let source = concat!(
            "fn f(opt: Option<i32>) -> i32 {\n",
            "    return match opt {\n",
            "        Some(v) => v,\n",
            "        None => 0,\n",
            "    };\n",
            "}\n",
        );
        let result = def_at(source, 2, 19).await.expect("use of v in arm");
        assert_eq!(result.range.start.line, 2);
        assert_eq!(result.range.start.character, 13);
        assert_eq!(result.range.end.character, 14);
    });
}

#[test]
fn while_let_binding_definition() {
    futures::executor::block_on(async {
        let source = concat!(
            "fn f(mut opt: Option<i32>) -> i32 {\n",
            "    while let Some(v) = opt {\n",
            "        return v;\n",
            "    }\n",
            "    return 0;\n",
            "}\n",
        );
        let result = def_at(source, 2, 15).await.expect("use of v in body");
        assert_range(&result, 1, 19, 20);
    });
}

#[test]
fn for_of_binding_definition() {
    futures::executor::block_on(async {
        let source = concat!(
            "fn f(items: Array<i32>) -> i32 {\n",
            "    let mut total = 0;\n",
            "    for let item of items {\n",
            "        total = total + item;\n",
            "    }\n",
            "    return total;\n",
            "}\n",
        );
        let result = def_at(source, 3, 25).await.expect("use of item in body");
        assert_range(&result, 2, 12, 16);
    });
}

#[test]
fn c_style_for_binding_definition() {
    futures::executor::block_on(async {
        let source = concat!(
            "fn f() -> i32 {\n",
            "    let mut sum = 0;\n",
            "    for let mut i = 0; i < 10; i += 1 {\n",
            "        sum = sum + i;\n",
            "    }\n",
            "    return sum;\n",
            "}\n",
        );
        let result = def_at(source, 3, 20).await.expect("use of i in body");
        assert_range(&result, 2, 16, 17);
    });
}

#[test]
fn match_or_pattern_binding_definition() {
    futures::executor::block_on(async {
        let source = concat!(
            "variant Sign { Pos(i32), Neg(i32), Zero }\n",
            "fn f(s: Sign) -> i32 {\n",
            "    return match s {\n",
            "        Pos(n) | Neg(n) => n,\n",
            "        Zero => 0,\n",
            "    };\n",
            "}\n",
        );
        let result = def_at(source, 3, 27).await.expect("use of n in arm body");
        assert_range(&result, 3, 12, 13);
    });
}

#[test]
fn struct_type_in_param_definition() {
    futures::executor::block_on(async {
        let source = concat!(
            "struct Point { x: i32, y: i32 }\n",
            "fn f(p: Point) -> i32 {\n",
            "    return p.x;\n",
            "}\n",
        );
        let result = def_at(source, 1, 9).await.expect("Point in param type");
        assert_range(&result, 0, 7, 12);
    });
}

#[test]
fn struct_type_in_literal_definition() {
    futures::executor::block_on(async {
        let source = concat!(
            "struct Point { x: i32, y: i32 }\n",
            "fn f() -> Point {\n",
            "    return Point { x: 1, y: 2 };\n",
            "}\n",
        );
        let result = def_at(source, 2, 13).await.expect("Point in literal");
        assert_range(&result, 0, 7, 12);
    });
}

#[test]
fn struct_type_in_return_annotation_definition() {
    futures::executor::block_on(async {
        let source = concat!(
            "struct Point { x: i32, y: i32 }\n",
            "fn origin() -> Point {\n",
            "    return Point { x: 0, y: 0 };\n",
            "}\n",
        );
        let result = def_at(source, 1, 16).await.expect("Point in return type");
        assert_range(&result, 0, 7, 12);
    });
}

#[test]
fn struct_field_access_definition() {
    futures::executor::block_on(async {
        let source = concat!(
            "struct Point { x: i32, y: i32 }\n",
            "fn f(p: Point) -> i32 {\n",
            "    return p.x;\n",
            "}\n",
        );
        let result = def_at(source, 2, 13).await.expect("p.x field access");
        assert_range(&result, 0, 15, 16);
    });
}

#[test]
fn struct_field_in_literal_definition() {
    futures::executor::block_on(async {
        let source = concat!(
            "struct Point { x: i32, y: i32 }\n",
            "fn f() -> Point {\n",
            "    return Point { x: 1, y: 2 };\n",
            "}\n",
        );
        let result = def_at(source, 2, 19).await.expect("field x in literal");
        assert_range(&result, 0, 15, 16);
    });
}

#[test]
fn enum_type_reference_definition() {
    futures::executor::block_on(async {
        let source = concat!(
            "enum Color { Red, Green, Blue }\n",
            "fn f() -> Color {\n",
            "    return Color::Red;\n",
            "}\n",
        );
        let result = def_at(source, 2, 13).await.expect("Color:: type ref");
        assert_range(&result, 0, 5, 10);
    });
}

#[test]
fn enum_case_reference_definition() {
    futures::executor::block_on(async {
        let source = concat!(
            "enum Color { Red, Green, Blue }\n",
            "fn f() -> Color {\n",
            "    return Color::Red;\n",
            "}\n",
        );
        let result = def_at(source, 2, 19).await.expect("Color::Red case ref");
        assert_range(&result, 0, 13, 16);
    });
}

#[test]
fn variant_type_reference_definition() {
    futures::executor::block_on(async {
        let source = concat!(
            "variant Maybe<T> { Just(T), Nothing }\n",
            "fn f() -> Maybe<i32> {\n",
            "    return Maybe::Just(1);\n",
            "}\n",
        );
        let result = def_at(source, 2, 13).await.expect("Maybe:: type ref");
        assert_range(&result, 0, 8, 13);
    });
}

#[test]
fn variant_case_in_constructor_definition() {
    futures::executor::block_on(async {
        let source = concat!(
            "variant Maybe<T> { Just(T), Nothing }\n",
            "fn f() -> Maybe<i32> {\n",
            "    return Maybe::Just(1);\n",
            "}\n",
        );
        let result = def_at(source, 2, 20).await.expect("Maybe::Just case ref");
        assert_range(&result, 0, 19, 23);
    });
}

#[test]
fn variant_case_in_pattern_definition() {
    futures::executor::block_on(async {
        let source = concat!(
            "variant Maybe<T> { Just(T), Nothing }\n",
            "fn f(m: Maybe<i32>) -> i32 {\n",
            "    return match m {\n",
            "        Just(v) => v,\n",
            "        Nothing => 0,\n",
            "    };\n",
            "}\n",
        );
        let result = def_at(source, 3, 10).await.expect("Just pattern");
        assert_range(&result, 0, 19, 23);
    });
}

#[test]
fn flags_type_reference_definition() {
    futures::executor::block_on(async {
        let source = concat!(
            "flags Perms { Read, Write, Execute }\n",
            "fn f() -> Perms {\n",
            "    return Perms::Read;\n",
            "}\n",
        );
        let result = def_at(source, 2, 13).await.expect("Perms:: type ref");
        assert_range(&result, 0, 6, 11);
    });
}

#[test]
fn flags_member_reference_definition() {
    futures::executor::block_on(async {
        let source = concat!(
            "flags Perms { Read, Write, Execute }\n",
            "fn f() -> Perms {\n",
            "    return Perms::Read;\n",
            "}\n",
        );
        let result = def_at(source, 2, 19).await.expect("Perms::Read member");
        assert_range(&result, 0, 14, 18);
    });
}

#[test]
fn trait_type_in_impl_definition() {
    futures::executor::block_on(async {
        let source = concat!(
            "trait Greet {\n",
            "    fn greet(&self) -> i32;\n",
            "}\n",
            "struct Bot { id: i32 }\n",
            "impl Greet for Bot {\n",
            "    fn greet(&self) -> i32 {\n",
            "        return self.id;\n",
            "    }\n",
            "}\n",
        );
        let result = def_at(source, 4, 7).await.expect("Greet in impl header");
        assert_range(&result, 0, 6, 11);
    });
}

#[test]
fn inherent_method_call_definition() {
    futures::executor::block_on(async {
        let source = concat!(
            "struct Counter { n: i32 }\n",
            "impl Counter {\n",
            "    fn get(&self) -> i32 {\n",
            "        return self.n;\n",
            "    }\n",
            "}\n",
            "fn use_it(c: Counter) -> i32 {\n",
            "    return c.get();\n",
            "}\n",
        );
        let result = def_at(source, 7, 14).await.expect("c.get() call");
        assert_range(&result, 2, 7, 10);
    });
}

#[test]
fn static_method_call_definition() {
    futures::executor::block_on(async {
        let source = concat!(
            "struct Point { x: i32, y: i32 }\n",
            "impl Point {\n",
            "    fn origin() -> Point {\n",
            "        return Point { x: 0, y: 0 };\n",
            "    }\n",
            "}\n",
            "fn f() -> Point {\n",
            "    return Point::origin();\n",
            "}\n",
        );
        let result = def_at(source, 7, 20)
            .await
            .expect("Point::origin static call");
        assert_range(&result, 2, 7, 13);
    });
}

#[test]
fn global_variable_read_definition() {
    futures::executor::block_on(async {
        let source = concat!(
            "global PI: f64 = 3.14;\n",
            "fn f() -> f64 {\n",
            "    return PI;\n",
            "}\n",
        );
        let result = def_at(source, 2, 11).await.expect("read of PI");
        assert_range(&result, 0, 7, 9);
    });
}

#[test]
fn global_variable_write_definition() {
    futures::executor::block_on(async {
        let source = concat!(
            "global mut counter: i32 = 0;\n",
            "fn inc() {\n",
            "    counter = counter + 1;\n",
            "}\n",
        );
        let result = def_at(source, 2, 5).await.expect("write of counter");
        assert_range(&result, 0, 11, 18);
    });
}

#[test]
fn newtype_reference_definition() {
    futures::executor::block_on(async {
        let source = concat!(
            "type Meters = f64;\n",
            "fn f() -> Meters {\n",
            "    return 100.0 as Meters;\n",
            "}\n",
        );
        let result = def_at(source, 1, 12).await.expect("Meters in return type");
        assert_range(&result, 0, 5, 11);
    });
}

#[test]
fn generic_type_parameter_use_definition() {
    futures::executor::block_on(async {
        let source = concat!("fn identity<T>(x: T) -> T {\n", "    return x;\n", "}\n",);
        let result = def_at(source, 0, 18).await.expect("T in param type");
        assert_range(&result, 0, 12, 13);
    });
}

#[test]
fn imported_item_use_definition() {
    futures::executor::block_on(async {
        let lib = "pub fn helper() -> i32 { return 42; }\n";
        let entry = concat!(
            "use { helper } from \"./lib.wado\";\n",
            "fn main() -> i32 {\n",
            "    return helper();\n",
            "}\n",
        );
        let result = def_at_in(
            &[("./lib.wado", lib), ("/test.wado", entry)],
            "/test.wado",
            2,
            14,
        )
        .await
        .expect("call of imported helper");
        assert_eq!(result.uri, "file:///lib.wado");
        assert_range(&result, 0, 7, 13);
    });
}

#[test]
fn use_specifier_definition() {
    futures::executor::block_on(async {
        let lib = "pub fn helper() -> i32 { return 42; }\n";
        let entry = concat!(
            "use { helper } from \"./lib.wado\";\n",
            "fn main() -> i32 {\n",
            "    return helper();\n",
            "}\n",
        );
        let result = def_at_in(
            &[("./lib.wado", lib), ("/test.wado", entry)],
            "/test.wado",
            0,
            9,
        )
        .await
        .expect("cursor on `helper` inside use{}");
        assert_eq!(result.uri, "file:///lib.wado");
        assert_range(&result, 0, 7, 13);
    });
}

#[test]
fn aliased_import_use_definition() {
    futures::executor::block_on(async {
        let lib = "pub fn helper() -> i32 { return 42; }\n";
        let entry = concat!(
            "use { helper as h } from \"./lib.wado\";\n",
            "fn main() -> i32 {\n",
            "    return h();\n",
            "}\n",
        );
        let result = def_at_in(
            &[("./lib.wado", lib), ("/test.wado", entry)],
            "/test.wado",
            2,
            11,
        )
        .await
        .expect("call of aliased h()");
        assert_eq!(result.uri, "file:///lib.wado");
        assert_range(&result, 0, 7, 13);
    });
}

#[test]
fn effect_in_with_clause_definition() {
    futures::executor::block_on(async {
        let source = concat!(
            "effect Log {\n",
            "    fn write(&self, msg: String);\n",
            "}\n",
            "fn run() with Log {\n",
            "}\n",
        );
        let result = def_at(source, 3, 15).await.expect("Log in with clause");
        assert_range(&result, 0, 7, 10);
    });
}

#[test]
fn generic_effect_parameter_use_definition() {
    futures::executor::block_on(async {
        let source = concat!(
            "fn wrapper<effect E>(f: fn() with E) with E {\n",
            "    f();\n",
            "}\n",
        );
        let result = def_at(source, 0, 34).await.expect("E in inner with clause");
        assert_range(&result, 0, 18, 19);
    });
}

#[test]
fn include_str_path_definition() {
    futures::executor::block_on(async {
        let runtime = "// helper\n";
        let entry = concat!(
            "fn f() {\n",
            "    let x = #include_str(\"./runtime.wado\");\n",
            "}\n",
        );
        let result = def_at_in(
            &[("/runtime.wado", runtime), ("/test.wado", entry)],
            "/test.wado",
            1,
            30,
        )
        .await
        .expect("cursor inside #include_str path");
        assert_eq!(result.uri, "file:///runtime.wado");
        assert_eq!(result.range.start.line, 0);
        assert_eq!(result.range.start.character, 0);
    });
}

// The cursor on the `#include_str` macro name (not the path literal) currently
// falls into `file_path_definition` because `Literal::IncludeStr` stores only
// the path string and the matcher keys off the entire literal span. This test
// pins the behaviour so narrowing the match to the path literal (see TODO in
// wado-lsp/CLAUDE.md) is a deliberate change.
#[test]
fn include_str_macro_name_currently_jumps_to_file() {
    futures::executor::block_on(async {
        let runtime = "// helper\n";
        let entry = concat!(
            "fn f() {\n",
            "    let x = #include_str(\"./runtime.wado\");\n",
            "}\n",
        );
        // Cursor on the `#` of `#include_str` (column 13, 1-based).
        let result = def_at_in(
            &[("/runtime.wado", runtime), ("/test.wado", entry)],
            "/test.wado",
            1,
            12,
        )
        .await
        .expect("cursor on `#include_str` macro name");
        assert_eq!(result.uri, "file:///runtime.wado");
    });
}

#[test]
fn include_bytes_path_definition() {
    futures::executor::block_on(async {
        let icon = "fake-bytes\n";
        let entry = concat!(
            "fn f() {\n",
            "    let x = #include_bytes(\"./icon.png\");\n",
            "}\n",
        );
        let result = def_at_in(
            &[("/icon.png", icon), ("/test.wado", entry)],
            "/test.wado",
            1,
            32,
        )
        .await
        .expect("cursor inside #include_bytes path");
        assert_eq!(result.uri, "file:///icon.png");
        assert_eq!(result.range.start.line, 0);
        assert_eq!(result.range.start.character, 0);
    });
}

#[test]
fn use_from_filename_definition() {
    futures::executor::block_on(async {
        let lib = "pub fn helper() -> i32 { return 42; }\n";
        let entry = concat!(
            "use { helper } from \"./lib.wado\";\n",
            "fn main() -> i32 {\n",
            "    return helper();\n",
            "}\n",
        );
        let result = def_at_in(
            &[("./lib.wado", lib), ("/test.wado", entry)],
            "/test.wado",
            0,
            25,
        )
        .await
        .expect("cursor inside \"./lib.wado\" string");
        assert_eq!(result.uri, "file:///lib.wado");
        assert_eq!(result.range.start.line, 0);
        assert_eq!(result.range.start.character, 0);
    });
}
