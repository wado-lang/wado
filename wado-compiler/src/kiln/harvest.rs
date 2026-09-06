//! The module-graph walk that finds every inline `with { generator }` clause.
//!
//! Kiln's unit is the module, not the entry: a clause in an imported module
//! produces a generated module for that module's own imports (WEP 2026-04-12
//! §"The loader redirect"). One walk serves every host — the CLI reads through
//! the filesystem, the LSP through `CompilerHost` — so the two cannot disagree
//! about which clauses exist.

use std::collections::VecDeque;

use crate::ast::{Item, Module};
use crate::compiler_host::{Code, Diagnostic, Severity};
use crate::hashmap::IndexMap;
use crate::kiln::inline::InvocationIndex;
use crate::name::{
    canonical_local_path, module_parent_dir, normalize_module_path, resolve_local_identity,
    resolve_module_path,
};

/// Every module the walk reached, keyed by the path it was reached through,
/// plus the loader identity each key resolves to.
pub struct Harvest {
    /// Module path (as the walk reached it) → parsed AST, entry first.
    pub modules: IndexMap<String, Module>,
    /// Module path → the identity the loader resolves that module under, which
    /// is what an [`InvocationIndex`] key must be ([`remap_decl_files`]).
    pub identities: IndexMap<String, String>,
}

/// Walk the local module graph from `entry_key`, parsing every `./` / `../`
/// `.wado` import reachable from it.
///
/// `entry_ast` supplies the entry's already-parsed tree (the LSP's editor
/// buffer, which `load` cannot see); `None` loads it like any other module.
/// `load` reads a module by the path this walk resolved for it. A module that
/// fails to load, or whose parse recovered from an error, is skipped: a
/// mid-edit source must not trigger codegen side effects.
pub async fn harvest_module_graph<L>(entry_key: &str, entry_ast: Option<Module>, load: L) -> Harvest
where
    L: AsyncFn(&str) -> Option<String>,
{
    let mut modules = IndexMap::<String, Module>::default();
    let mut identities = IndexMap::<String, String>::default();

    let entry_dir = module_parent_dir(entry_key).to_string();
    // The entry's key must stay byte-identical to its `EntryPoint.filename`
    // (interned verbatim by the loader), so the redirect it keys is found at
    // resolve time — do not normalize it here.
    identities.insert(entry_key.to_string(), entry_key.to_string());

    // (key, loader identity, pre-parsed AST, is_entry)
    let mut queue: VecDeque<(String, String, Option<Module>, bool)> = VecDeque::from([(
        entry_key.to_string(),
        entry_key.to_string(),
        entry_ast,
        true,
    )]);
    while let Some((key, loader_id, ast, is_entry)) = queue.pop_front() {
        if modules.contains_key(&key) {
            continue;
        }
        let ast = if let Some(ast) = ast {
            ast
        } else {
            let Some(source) = load(&key).await else {
                continue;
            };
            let Ok(parsed) = crate::parse(&source).into_fail_fast() else {
                continue;
            };
            parsed.ast
        };

        for import in local_wado_imports(&ast) {
            let child_key = resolve_module_path(&key, import);
            // Must match the loader's identity derivation (entry vs deeper).
            let child_loader_id = if is_entry {
                canonical_local_path(&entry_dir, &normalize_module_path(import))
            } else {
                resolve_local_identity(&entry_dir, &loader_id, import)
            };
            // Don't overwrite the entry's pinned identity (back-refs fold to it).
            if child_key != entry_key {
                identities.insert(child_key.clone(), child_loader_id.clone());
            }
            if !modules.contains_key(&child_key) {
                queue.push_back((child_key, child_loader_id, None, false));
            }
        }
        modules.insert(key, ast);
    }

    Harvest {
        modules,
        identities,
    }
}

/// The `./` / `../` `.wado` imports of `module`. Non-`.wado` sources are the
/// Kiln schemas themselves, and `core:` / `wasi:` / dependency specifiers name
/// no module in this graph.
fn local_wado_imports(module: &Module) -> impl Iterator<Item = &str> {
    module.items.iter().filter_map(|item| {
        let Item::Use(use_decl) = item else {
            return None;
        };
        let src = use_decl.source.as_str();
        ((src.starts_with("./") || src.starts_with("../"))
            && src.to_ascii_lowercase().ends_with(".wado"))
        .then_some(src)
    })
}

/// Rewrite `index`'s `decl_file` keys from harvest keys to loader identities
/// (keys absent from `identities` pass through), so the loader's lookup —
/// which asks under the identity it resolved the declaring module by — hits.
///
/// Returns a diagnostic for each *conflict* — two keys resolving to the same
/// `(loader_identity, from)` but different targets — instead of silently
/// dropping a redirect (last-write-wins); matching targets are a harmless
/// duplicate. Unreachable with canonical identities; guards against a future
/// identity-scheme regression.
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
    use super::*;

    fn block_on<F: Future>(future: F) -> F::Output {
        futures::executor::block_on(future)
    }

    #[test]
    fn walks_past_the_entry_into_imported_modules() {
        let sources = [
            ("src/main.wado", "use { a } from \"./lib.wado\";\n"),
            ("src/lib.wado", "use { b } from \"./deep/util.wado\";\n"),
            ("src/deep/util.wado", "pub fn b() {}\n"),
        ];
        let harvest = block_on(harvest_module_graph("src/main.wado", None, async |path| {
            sources
                .iter()
                .find(|(p, _)| *p == path)
                .map(|(_, s)| (*s).to_string())
        }));

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
    fn a_cycle_terminates() {
        let sources = [
            ("src/a.wado", "use { x } from \"./b.wado\";\n"),
            ("src/b.wado", "use { y } from \"./a.wado\";\n"),
        ];
        let harvest = block_on(harvest_module_graph("src/a.wado", None, async |path| {
            sources
                .iter()
                .find(|(p, _)| *p == path)
                .map(|(_, s)| (*s).to_string())
        }));

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
        let harvest = block_on(harvest_module_graph(
            "main.wado",
            Some(crate::parse("use { a } from \"./missing.wado\";\n").ast),
            async |_| None,
        ));

        assert_eq!(harvest.modules.keys().collect::<Vec<_>>(), ["main.wado"]);
    }
}
