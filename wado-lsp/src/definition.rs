//! Go-to-definition, powered by `wado_compiler::annotate`.
//!
//! Resolution flow:
//! 1. Run `annotate` to produce a fully-resolved [`Annotated`] snapshot.
//! 2. Use `Annotated::ast_id_at` to find the innermost AST node at the cursor.
//! 3. If that node is a use-site (Ident of a local), follow
//!    `Annotated::referenced_symbol` to the binding [`SymbolKey`].
//! 4. Otherwise the cursor AST id itself points at a declared symbol.
//! 5. Translate the resulting [`SymbolKey`] into a [`DefinitionResult`].

use serde::{Deserialize, Serialize};
use wado_compiler::CompilerHost;
use wado_compiler::annotate::annotate;
use wado_compiler::ast::{self, AstId, Item, Module};
use wado_compiler::token::Span;

use crate::diagnostics::{Position, Range};
use crate::location::{resolve_def_key, span_to_range, symbol_uri, uri_to_filename};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DefinitionResult {
    pub uri: String,
    pub range: Range,
}

/// Find the definition of the identifier at `position` in `source`.
///
/// `uri` is the URI of the document being edited; cross-file results carry
/// their own URI (derived from the defining module's `diagnostic_filename`).
pub async fn find_definition<H: CompilerHost>(
    source: &str,
    position: Position,
    uri: &str,
    host: &H,
) -> Option<DefinitionResult> {
    let filename = uri_to_filename(uri);
    let annotated = annotate(source, host, Some(&filename)).await.ok()?;

    let module = annotated.entry_module_source.clone();
    let line = position.line as usize + 1;
    let col = position.character as usize + 1;

    let def_key = resolve_def_key(&annotated, &module, line, col)?;

    let symbol = annotated.symbol_at(&def_key)?;
    let span = annotated
        .name_span_of(&def_key)
        .or(symbol.span)
        .or_else(|| span_of_ast_id(annotated.modules.get(&def_key.module)?, def_key.ast_id))?;
    let def_uri = symbol_uri(&annotated, symbol, uri)?;
    Some(DefinitionResult {
        uri: def_uri,
        range: span_to_range(&span),
    })
}

/// Best-effort span for an arbitrary [`AstId`] — walks module items looking for
/// a matching id. Used only when `name_span_of` has no name-span and the
/// symbol has no declared span (rare).
fn span_of_ast_id(module: &Module, target: AstId) -> Option<Span> {
    for item in &module.items {
        if let Some(span) = item_span_if_match(item, target) {
            return Some(span);
        }
    }
    None
}

fn item_span_if_match(item: &Item, target: AstId) -> Option<Span> {
    match item {
        Item::Function(f) if f.id == target => Some(f.span),
        Item::Struct(s) if s.id == target => Some(s.span),
        Item::Enum(e) if e.id == target => Some(e.span),
        Item::Variant(v) if v.id == target => Some(v.span),
        Item::Flags(fl) if fl.id == target => Some(fl.span),
        Item::Trait(t) if t.id == target => Some(t.span),
        Item::Newtype(n) if n.id == target => Some(n.span),
        Item::Global(g) if g.id == target => Some(g.span),
        _ => None,
    }
}

#[allow(dead_code)]
fn _touch_ast_module_import(module: &ast::Module) {
    let _ = module;
}

#[cfg(test)]
mod tests {
    use super::*;
    use indexmap::IndexMap;
    use wado_compiler::{Diagnostic as CompilerDiagnostic, SourceError};

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
            self.sources
                .get(path)
                .cloned()
                .ok_or_else(|| SourceError::NotFound {
                    path: path.to_string(),
                })
        }

        fn emit_diagnostic(&self, _diagnostic: CompilerDiagnostic) {}
    }

    async fn def_at(source: &str, line: u32, character: u32) -> Option<DefinitionResult> {
        let path = "/test.wado";
        let uri = format!("file://{path}");
        let host = TestHost::new(path, source);
        find_definition(source, Position { line, character }, &uri, &host).await
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
        find_definition(entry_source, Position { line, character }, &uri, &host).await
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
            let source = "fn helper() -> i32 {\n    return 1;\n}\nfn run() -> i32 {\n    return helper();\n}\n";
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
            let result = def_at(source, 7, 20).await.expect("Point::origin static call");
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
            let source = concat!(
                "fn identity<T>(x: T) -> T {\n",
                "    return x;\n",
                "}\n",
            );
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
                &[("/lib.wado", lib), ("/test.wado", entry)],
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
                &[("/lib.wado", lib), ("/test.wado", entry)],
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
                &[("/lib.wado", lib), ("/test.wado", entry)],
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
}
