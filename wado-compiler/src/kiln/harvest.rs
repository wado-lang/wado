//! The module-graph walk behind every inline `with { generator }` clause.
//! Kiln's unit is the module, not the entry, and one walk serves every host.

use std::collections::VecDeque;

use crate::ast::{ImportAttributes, Item, Module, UseDecl};
use crate::compiler_host::{Code, DependencyIndex, Diagnostic, Severity};
use crate::hashmap::{IndexMap, IndexSet};
use crate::kiln::inline::InvocationIndex;
use crate::module_source::{ModuleSource, ModuleSourceInterner, PackageHost};
use crate::name::{
    canonicalize_entry_point, decl_file_of, resolve_import_with_entry, resolve_module_path,
};
use crate::parse;
use crate::path::is_cwd_relative;
use crate::remote::Integrity;

/// Every module the walk reached, keyed by the path it was reached through.
pub struct Harvest {
    /// Module path → parsed AST, entry first.
    pub modules: IndexMap<String, Module>,
    /// Module path → the identity the loader resolves that module under, which
    /// is what an [`InvocationIndex`] key must be ([`remap_decl_files`]).
    pub identities: IndexMap<String, String>,
    /// Module path → the `wado.toml` of the package it belongs to, `None` for
    /// a module in no package. A module of the walk's own package is absent.
    pub packages: IndexMap<String, Option<String>>,
}

/// Walk the module graph from `entry_key` and `entry_ast`, parsing every Wado
/// source import reachable from them: a `./` / `../` path, a remote module,
/// and a module of a source dependency in `dependencies` at any depth.
///
/// A module carries two names, seeded by `entry_key` and `entry_identity`: a
/// clause path is spelled against the key, and the loader reads the module
/// under the identity. They differ because a generator's own compile is rooted
/// at its entry's directory while its invocations are rooted at its package.
/// Each import resolves as the loader resolves it, so the identity is the one
/// the loader gives the module.
///
/// `load` receives the loader identity, and the first pin an import of a remote
/// module wrote, and `host` answers about packages as
/// the loader's host does, so a relatively reached file is placed in the
/// package the loader places it in. The caller supplies the entry's tree
/// because the loader does not read the entry through a host either.
///
/// A module that fails to load, or whose parse recovered from an error, is
/// skipped: a mid-edit source must not trigger codegen side effects. So is a
/// key in `settled`, which an earlier walk from the same root harvested along
/// with everything it imports.
pub async fn harvest_module_graph<L>(
    entry_key: &str,
    entry_identity: &str,
    entry_ast: Module,
    settled: &IndexSet<String>,
    dependencies: &DependencyIndex,
    host: &impl PackageHost,
    load: L,
) -> Harvest
where
    L: AsyncFn(&str, Option<Integrity>) -> Option<String>,
{
    let mut modules = IndexMap::<String, Module>::default();
    let mut identities = IndexMap::<String, String>::default();
    let mut packages = IndexMap::<String, Option<String>>::default();

    let mut interner = ModuleSourceInterner::new();
    interner.set_dependencies(dependencies.clone());
    interner.bound_packages(&canonicalize_entry_point(entry_identity), host);
    // The entry's identity must stay byte-identical to its `EntryPoint.filename`
    // (interned verbatim by the loader), so the redirect it keys is found at
    // resolve time — do not normalize it here.
    let entry = interner.entry_point(entry_identity);
    identities.insert(entry_key.to_string(), entry_identity.to_string());

    // The first pin any import of a remote module wrote, the one the loader
    // asks the host for it with.
    let mut pins = IndexMap::<String, Integrity>::default();
    let mut queue: VecDeque<(String, ModuleSource, Option<Module>)> =
        VecDeque::from([(entry_key.to_string(), entry.clone(), Some(entry_ast))]);
    while let Some((key, source, ast)) = queue.pop_front() {
        if modules.contains_key(&key) || settled.contains(&key) {
            continue;
        }
        let ast = if let Some(ast) = ast {
            ast
        } else {
            let identity = decl_file_of(&source);
            let pin = pins.get(identity).copied();
            let Some(text) = load(identity, pin).await else {
                continue;
            };
            let Ok(parsed) = parse(&text).into_fail_fast() else {
                continue;
            };
            parsed.ast
        };

        for use_decl in wado_source_imports(&ast) {
            let import = use_decl.source.as_str();
            let child = resolve_import_with_entry(&mut interner, &source, import, Some(&entry));
            let (child, child_key) = match child {
                // A remote module is keyed by its URL, which is also how a
                // clause path relative to it is spelled.
                ModuleSource::Remote { ref url, .. } => {
                    let url = url.to_string();
                    if let Some(pin) = written_pin(use_decl) {
                        pins.entry(url.clone()).or_insert(pin);
                    }
                    (child, url)
                }
                ModuleSource::Local { path, .. } if is_cwd_relative(import) => {
                    let path = path.to_string();
                    let placed = interner.place(&path, host);
                    (placed, resolve_module_path(&key, import))
                }
                ModuleSource::Dependency { ref path, .. } => {
                    let child_key = key_of(entry_key, path);
                    (child, child_key)
                }
                // A dependency of a package a path reached first.
                ModuleSource::Local { ref path, .. } if interner.is_placed(path) => {
                    let child_key = key_of(entry_key, path);
                    (child, child_key)
                }
                // A back-reference to the entry folds onto it; anything else
                // is not Wado source this walk reads.
                _ => continue,
            };
            // A relative path into another package's tree places the module there.
            if !child.is_entry_package() {
                let manifest = interner.dependency_manifest(&child);
                packages.insert(child_key.clone(), manifest.map(|m| m.path.clone()));
            }
            identities.insert(child_key.clone(), decl_file_of(&child).to_string());
            queue.push_back((child_key, child, None));
        }
        modules.insert(key, ast);
    }

    Harvest {
        modules,
        identities,
        packages,
    }
}

/// The key of the module the loader names `identity`, a path relative to the
/// entry's directory: the same path, spelled from the walk's root.
fn key_of(entry_key: &str, identity: &str) -> String {
    if is_cwd_relative(identity) {
        resolve_module_path(entry_key, identity)
    } else {
        resolve_module_path(entry_key, &format!("./{identity}"))
    }
}

/// The imports of `module` that are Wado source: those with neither `generator`
/// nor `type`, whatever the extension.
fn wado_source_imports(module: &Module) -> impl Iterator<Item = &UseDecl> {
    module.items.iter().filter_map(|item| {
        let Item::Use(use_decl) = item else {
            return None;
        };
        let is_source = use_decl
            .attributes
            .as_ref()
            .is_none_or(ImportAttributes::reads_as_source);
        is_source.then_some(use_decl)
    })
}

/// The pin `use_decl` writes, where it writes a well-formed one; the loader
/// reports any other.
fn written_pin(use_decl: &UseDecl) -> Option<Integrity> {
    let entry = use_decl.attributes.as_ref()?.integrity()?;
    Integrity::parse(entry.value.as_str()?)
}

/// Rewrite `index`'s `decl_file` keys from harvest keys to loader identities,
/// which is what the loader asks under. Keys absent from `identities` pass
/// through.
///
/// Two keys landing on one `(identity, from)` with different targets is
/// reported rather than resolved last-write-wins; an identical target is a
/// harmless duplicate. Canonical identities make the conflict unreachable, so
/// this guards against a future identity-scheme regression.
#[must_use]
pub fn remap_decl_files(
    index: &mut InvocationIndex,
    identities: &IndexMap<String, String>,
) -> Vec<Diagnostic> {
    let rewritten: Vec<(String, String, String)> = index
        .entries()
        .map(|(decl, from, uri)| {
            let decl = identities.get(decl).map_or(decl, String::as_str);
            (decl.to_string(), from.to_string(), uri.to_string())
        })
        .collect();

    let mut diagnostics = Vec::new();
    let mut fresh = InvocationIndex::new();
    for (decl, from, uri) in rewritten {
        // `redirect` normalizes `from` identically to `insert`, so this sees
        // the key `insert` would write.
        if let Some(existing) = fresh.redirect(&decl, &from)
            && existing != uri
        {
            diagnostics.push(Diagnostic {
                severity: Severity::Error,
                code: Code::KilnRedirectConflict,
                message: format!(
                    "kiln: two generator invocations for `use ... from {from:?}` in `{decl}` \
                     redirect to different generated modules (`{existing}` and `{uri}`); \
                     a module cannot resolve one import to two generated entries",
                ),
                span: None,
            });
            continue;
        }
        fresh.insert(&decl, &from, &uri);
    }
    *index = fresh;
    diagnostics
}

#[cfg(test)]
mod tests {
    use futures::executor::block_on;

    use super::*;
    use crate::compiler_host::DependencyManifest;
    use crate::module_source::TestPackages;

    fn ast_of(source: &str) -> Module {
        parse(source).ast
    }

    fn no_packages() -> TestPackages<impl Fn(&str) -> Option<String>> {
        TestPackages {
            root: |_: &str| None,
            indexes: IndexMap::default(),
        }
    }

    /// The walk with no dependencies, every module in no package.
    async fn walk(
        entry_key: &str,
        entry_identity: &str,
        entry_ast: Module,
        settled: &IndexSet<String>,
        load: impl AsyncFn(&str, Option<Integrity>) -> Option<String>,
    ) -> Harvest {
        let dependencies = DependencyIndex::default();
        harvest_module_graph(
            entry_key,
            entry_identity,
            entry_ast,
            settled,
            &dependencies,
            &no_packages(),
            load,
        )
        .await
    }

    /// A host that matches its keys exactly, as the in-memory hosts do.
    /// `FilesystemCompilerHost` would answer either spelling, and would hide a
    /// walk that asks under keys the loader never uses.
    fn exact(
        sources: &'static [(&'static str, &'static str)],
    ) -> impl AsyncFn(&str, Option<Integrity>) -> Option<String> {
        move |path: &str, _| {
            let hit = sources
                .iter()
                .find(|(p, _)| *p == path)
                .map(|(_, s)| (*s).to_string());
            async move { hit }
        }
    }

    #[test]
    fn walks_past_the_entry_into_imported_modules() {
        // Keyed the way the loader reads them: entry-dir-relative identities.
        let sources: &[(&str, &str)] = &[
            ("./lib.wado", "use { b } from \"./deep/util.wado\";\n"),
            ("./deep/util.wado", "pub fn b() {}\n"),
        ];
        let harvest = block_on(walk(
            "src/main.wado",
            "src/main.wado",
            ast_of("use { a } from \"./lib.wado\";\n"),
            &IndexSet::default(),
            exact(sources),
        ));

        assert_eq!(
            harvest.modules.keys().collect::<Vec<_>>(),
            ["src/main.wado", "src/lib.wado", "src/deep/util.wado"],
        );
        // Identities are anchored on the entry's directory, which is what the
        // loader names a local module by (`Loader::resolve_import`).
        assert_eq!(
            harvest.identities.get("src/deep/util.wado").unwrap(),
            "./deep/util.wado",
        );
    }

    #[test]
    fn walks_into_source_whatever_its_extension_and_not_into_a_schema_or_asset() {
        let sources: &[(&str, &str)] = &[
            ("./helpers.g4", "pub fn h() {}\n"),
            ("./Calc.g4", "grammar Calc;\n"),
            ("./m.wasm", "\0asm"),
        ];
        let harvest = block_on(walk(
            "src/main.wado",
            "src/main.wado",
            ast_of(
                "use { h } from \"./helpers.g4\";\n\
                 use calc from \"./Calc.g4\" with { generator: { module: \"lib:gale\" } };\n\
                 use { f } from \"./m.wasm\" with { type: \"wasm\" };\n",
            ),
            &IndexSet::default(),
            exact(sources),
        ));

        assert_eq!(
            harvest.modules.keys().collect::<Vec<_>>(),
            ["src/main.wado", "src/helpers.g4"],
        );
    }

    #[test]
    fn a_cycle_terminates() {
        let sources: &[(&str, &str)] = &[("./b.wado", "use { y } from \"./a.wado\";\n")];
        let harvest = block_on(walk(
            "src/a.wado",
            "src/a.wado",
            ast_of("use { x } from \"./b.wado\";\n"),
            &IndexSet::default(),
            exact(sources),
        ));

        assert_eq!(harvest.modules.len(), 2);
        // The back-reference keeps the entry's pinned identity.
        assert_eq!(harvest.identities.get("src/a.wado").unwrap(), "src/a.wado");
    }

    #[test]
    fn remap_swaps_a_harvest_key_for_its_loader_identity() {
        let mut idx = InvocationIndex::new();
        idx.insert(
            "/abs/example/eval.wado",
            "./Arith.g4",
            "kiln:file:///g/arith.wado",
        );

        let mut identities = IndexMap::default();
        identities.insert(
            "/abs/example/eval.wado".to_string(),
            "./eval.wado".to_string(),
        );

        let conflicts = remap_decl_files(&mut idx, &identities);
        assert!(conflicts.is_empty(), "no conflict for a single mapping");

        assert!(
            idx.redirect("/abs/example/eval.wado", "./Arith.g4")
                .is_none(),
            "the harvest-key decl_file must no longer redirect"
        );
        assert_eq!(
            idx.redirect("./eval.wado", "./Arith.g4"),
            Some("kiln:file:///g/arith.wado"),
            "the loader identity must redirect to the same URI"
        );
    }

    // Two keys collapsing onto one `(identity, from)` with different targets
    // must be reported, not silently resolved last-write-wins (#1423).
    #[test]
    fn remap_reports_a_conflict_on_identity_collision() {
        let mut idx = InvocationIndex::new();
        idx.insert("/abs/a.wado", "./G.g4", "kiln:file:///g/from_a.wado");
        idx.insert("/abs/b.wado", "./G.g4", "kiln:file:///g/from_b.wado");

        let mut identities = IndexMap::default();
        identities.insert("/abs/a.wado".to_string(), "./shared.wado".to_string());
        identities.insert("/abs/b.wado".to_string(), "./shared.wado".to_string());

        let conflicts = remap_decl_files(&mut idx, &identities);
        assert_eq!(conflicts.len(), 1, "the collision must be reported");
        assert_eq!(conflicts[0].code, Code::KilnRedirectConflict);
        assert_eq!(conflicts[0].severity, Severity::Error);
        // The first redirect survives; the conflicting second is dropped.
        assert_eq!(
            idx.redirect("./shared.wado", "./G.g4"),
            Some("kiln:file:///g/from_a.wado"),
        );
    }

    // Same identity *and* target is a benign duplicate: no diagnostic.
    #[test]
    fn remap_allows_a_duplicate_with_an_identical_target() {
        let mut idx = InvocationIndex::new();
        idx.insert("/abs/a.wado", "./G.g4", "kiln:file:///g/shared.wado");
        idx.insert("/abs/b.wado", "./G.g4", "kiln:file:///g/shared.wado");

        let mut identities = IndexMap::default();
        identities.insert("/abs/a.wado".to_string(), "./shared.wado".to_string());
        identities.insert("/abs/b.wado".to_string(), "./shared.wado".to_string());

        let conflicts = remap_decl_files(&mut idx, &identities);
        assert!(conflicts.is_empty(), "identical target is not a conflict");
        assert_eq!(
            idx.redirect("./shared.wado", "./G.g4"),
            Some("kiln:file:///g/shared.wado"),
        );
    }

    #[test]
    fn an_unloadable_module_is_skipped_not_fatal() {
        let harvest = block_on(walk(
            "main.wado",
            "main.wado",
            ast_of("use { a } from \"./missing.wado\";\n"),
            &IndexSet::default(),
            async |_, _| None,
        ));

        assert_eq!(harvest.modules.keys().collect::<Vec<_>>(), ["main.wado"]);
    }

    #[test]
    fn walks_into_a_dependency_under_the_identity_the_loader_gives_it() {
        let lib = "../shapes/src/lib.wado";
        let manifest = "../shapes/wado.toml";
        let mut shapes = DependencyIndex {
            manifest: Some(DependencyManifest {
                path: manifest.to_string(),
                error: None,
            }),
            ..DependencyIndex::default()
        };
        shapes.exports.insert(
            "src/point.wado".to_string(),
            "../shapes/src/point.wado".to_string(),
        );
        let mut dependencies = DependencyIndex::default();
        dependencies
            .resolved
            .insert("lib:shapes".to_string(), lib.to_string());
        dependencies.packages.insert(lib.to_string(), shapes);
        let sources: &[(&str, &str)] = &[
            ("../shapes/src/lib.wado", "pub fn l() {}\n"),
            (
                "../shapes/src/point.wado",
                "use { h } from \"./helper.wado\";\n",
            ),
            ("../shapes/src/helper.wado", "pub fn h() {}\n"),
        ];
        let harvest = block_on(harvest_module_graph(
            "main.wado",
            "main.wado",
            ast_of(
                "use { l } from \"lib:shapes\";\n\
                 use { p } from \"lib:shapes/src/point.wado\";\n\
                 use { q } from \"lib:absent\";\n",
            ),
            &IndexSet::default(),
            &dependencies,
            &no_packages(),
            exact(sources),
        ));

        assert_eq!(
            harvest.modules.keys().collect::<Vec<_>>(),
            [
                "main.wado",
                "../shapes/src/lib.wado",
                "../shapes/src/point.wado",
                "../shapes/src/helper.wado"
            ],
        );
        assert_eq!(
            harvest.identities.get("../shapes/src/helper.wado").unwrap(),
            "../shapes/src/helper.wado",
        );
        assert_eq!(
            harvest.packages["../shapes/src/helper.wado"].as_deref(),
            Some(manifest)
        );
        assert!(!harvest.packages.contains_key("main.wado"));
    }

    #[test]
    fn a_settled_module_is_neither_read_nor_walked() {
        let sources: &[(&str, &str)] = &[
            ("./shared.wado", "use { b } from \"./deep.wado\";\n"),
            ("./deep.wado", "pub fn b() {}\n"),
        ];
        let settled: IndexSet<String> = ["src/shared.wado".to_string()].into_iter().collect();
        let harvest = block_on(walk(
            "src/a_test.wado",
            "src/a_test.wado",
            ast_of("use { a } from \"./shared.wado\";\n"),
            &settled,
            exact(sources),
        ));

        assert_eq!(
            harvest.modules.keys().collect::<Vec<_>>(),
            ["src/a_test.wado"]
        );
    }
}
