//! Inline-invocation collection for `use ... with { generator: { ... } }`.
//!
//! Every Kiln invocation is declared at the import site; the manifest does
//! not declare invocations. This module walks the parsed [`UseDecl`]s,
//! extracts the `generator: {...}` object from
//! [`crate::ast::ImportAttributes`], and builds matching
//! [`crate::kiln::Invocation`]s before [`crate::kiln::plan::build_plan`] runs.
//!
//! Two clauses with identical `(module, from, inputs, output_dir,
//! options_canonical)` tuples are merged into a single invocation. Two
//! clauses that share `from` but disagree on any other field produce a
//! diagnostic citing both spans.

use sha2::{Digest, Sha256};

use crate::ast::{AttrValue, Module, UseDecl};
use crate::compiler_host::{Code, Diagnostic, DiagnosticSpan, Severity};
use crate::hashmap::IndexMap;

use super::cache::{encode_options_canonical, hex_digest};
use super::invocation::{DeclSite, GeneratorModule, Invocation, InvocationPath};
use super::options::OptionsDescriptor;
use super::options_check::{CanonicalOptions, validate};

/// Default output-directory prefix for inline invocations: each clause lands
/// under `build/kiln/<synthetic_id>` unless it declares its own `output_dir`.
pub const DEFAULT_INLINE_OUTPUT_DIR_PREFIX: &str = "build/kiln";

/// Resolver-side lookup table that redirects a `use ... from "<from>"` whose
/// `<from>` path matches an inline Kiln invocation's primary source to the
/// invocation's generated entry module.
///
/// Built once per compilation unit from the inline invocation set, after the
/// pipeline has populated `build/kiln/…` so the entry module's on-disk
/// location is known. For consume-only mode (no pipeline run), the index is
/// built from the last recorded lockfile entry's `output.entry = true` path
/// — that path lives on disk already or the cache check failed.
///
/// Lookups are keyed by `(declaring_file_normalized, from_path_normalized)`.
/// A bare `use ... from "./schema.<ext>"` in any other file fails with
/// `Code::KilnMissingWith`.
#[derive(Debug, Default, Clone)]
pub struct InvocationIndex {
    /// Entries mapping `(decl_file, from_path)` → entry module URI. The URI
    /// is an opaque resource identifier (typically a `file:` URI
    /// produced by the CLI pipeline) that the loader hands verbatim to
    /// `CompilerHost::load_source` — it is not normalized as a path.
    entries: IndexMap<(String, String), String>,
}

impl InvocationIndex {
    /// Create an empty index.
    #[must_use]
    pub fn new() -> Self {
        Self {
            entries: IndexMap::default(),
        }
    }

    /// Record that a `use ... from "<from>"` inside `decl_file` redirects to
    /// `entry_uri`. Both keys are normalized for matching; `entry_uri` is
    /// stored opaquely.
    ///
    /// Silently overwrites a prior entry with the same key. The caller is
    /// responsible for keeping the set of recorded invocations conflict-free
    /// (see [`collect_inline_invocations`]).
    pub fn insert(&mut self, decl_file: &str, from: &str, entry_uri: &str) {
        let from = InvocationPath::normalize(from).as_str().to_string();
        self.entries
            .insert((decl_file.to_string(), from), entry_uri.to_string());
    }

    /// Look up the entry URI for a `(decl_file, from)` pair. Returns the
    /// opaque URI of the entry module, or `None` if no invocation
    /// matches.
    #[must_use]
    pub fn redirect(&self, decl_file: &str, from: &str) -> Option<&str> {
        let from = InvocationPath::normalize(from).as_str().to_string();
        self.entries
            .get(&(decl_file.to_string(), from))
            .map(String::as_str)
    }

    /// Returns `true` when no invocations have been recorded. Consumers
    /// (e.g. the resolver) can short-circuit the redirect check.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Scan every `UseDecl` in `modules` for an inline
/// `with { generator: { ... } }` clause and lower it to a canonical
/// [`Invocation`].
///
/// `descriptors` is a per-module-spec lookup — when present, the inline
/// clause's `options` object is validated and encoded through the typed
/// pipeline. Missing descriptors fall through to an empty canonical options
/// blob (matching the behavior for a generator without a declared
/// `Options` struct).
///
/// Diagnostics are batched: a single malformed clause does not prevent the
/// rest of the tree from being collected. Returns `Err` only when at least
/// one `Severity::Error` diagnostic was emitted.
///
/// # Errors
/// Every shape mismatch, options-validation error, and dedup conflict is
/// reported through the returned `Vec<Diagnostic>`.
pub fn collect_inline_invocations<'a, I>(
    modules: I,
    descriptors: &IndexMap<String, OptionsDescriptor>,
    manifest_root: &str,
) -> Result<Vec<Invocation>, Vec<Diagnostic>>
where
    I: IntoIterator<Item = (&'a str, &'a Module)>,
{
    let mut diagnostics: Vec<Diagnostic> = Vec::new();
    let mut by_tuple: IndexMap<String, Invocation> = IndexMap::default();
    let mut by_from: IndexMap<String, (Invocation, String)> = IndexMap::default();

    for (module_path, module) in modules {
        for use_decl in use_decls_of(module) {
            let Some(attrs) = use_decl.attributes.as_ref() else {
                continue;
            };
            let Some(gen_cfg) = attrs.generator() else {
                continue;
            };

            match lower_inline(module_path, use_decl, gen_cfg, descriptors, manifest_root) {
                Ok(invocation) => {
                    let tuple_key = identity_key(&invocation);
                    if let Some(existing) = by_tuple.get(&tuple_key) {
                        let _ = existing;
                        continue;
                    }
                    let from_key = invocation.from.as_str().to_string();
                    if let Some((prior, prior_mod)) = by_from.get(&from_key)
                        && identity_key(prior) != tuple_key
                    {
                        diagnostics.push(Diagnostic {
                            severity: Severity::Error,
                            code: Code::GeneratorOptionsInvalid,
                            message: format!(
                                "kiln: two inline generator clauses disagree for `from = \"{}\"` \
                                     (first in {}, second in {})",
                                invocation.from.as_str(),
                                prior_mod,
                                module_path,
                            ),
                            span: Some(span_of(module_path, use_decl)),
                        });
                        continue;
                    }
                    by_tuple.insert(tuple_key.clone(), invocation.clone());
                    by_from.insert(from_key, (invocation, module_path.to_string()));
                }
                Err(mut errs) => diagnostics.append(&mut errs),
            }
        }
    }

    if diagnostics.iter().any(|d| d.severity == Severity::Error) {
        return Err(diagnostics);
    }
    Ok(by_tuple.into_iter().map(|(_, v)| v).collect())
}

fn use_decls_of(module: &Module) -> impl Iterator<Item = &UseDecl> {
    module.items.iter().filter_map(|it| match it {
        crate::ast::Item::Use(u) => Some(u),
        _ => None,
    })
}

fn lower_inline(
    module_path: &str,
    use_decl: &UseDecl,
    cfg: &IndexMap<String, AttrValue>,
    descriptors: &IndexMap<String, OptionsDescriptor>,
    manifest_root: &str,
) -> Result<Invocation, Vec<Diagnostic>> {
    let mut errors: Vec<Diagnostic> = Vec::new();

    // `module` accepts the same module-specifier shape as a `use ...
    // from "<source>"` clause:
    //   - "./..." / "../..." → relative path resolved against the
    //     importing file (same as a regular `use`).
    //   - "<ns>:<name>[@<ver>]" → registry / stdlib namespace spec.
    // Anything else is rejected. The author is already writing
    // path-relative imports a few characters earlier
    // (`use ... from "./schema.g4"`), so the inline `module` field
    // follows the same rule.
    let module = match cfg.get("module") {
        Some(AttrValue::String(s)) => {
            lower_module_specifier(module_path, use_decl, s, manifest_root, &mut errors)
        }
        Some(other) => {
            errors.push(Diagnostic {
                severity: Severity::Error,
                code: Code::GeneratorOptionsInvalid,
                message: format!(
                    "kiln: `generator.module` must be a module specifier string \
                     (\"./path/to/generator.wado\" or \"ns:name@ver\"), got {}",
                    attr_kind(other),
                ),
                span: Some(span_of(module_path, use_decl)),
            });
            None
        }
        None => {
            errors.push(Diagnostic {
                severity: Severity::Error,
                code: Code::GeneratorOptionsInvalid,
                message: "kiln: inline `with { generator: {...} }` requires a `module` field"
                    .to_string(),
                span: Some(span_of(module_path, use_decl)),
            });
            None
        }
    };

    let from = InvocationPath::normalize(&use_decl.source);
    let inputs = match cfg.get("inputs") {
        None => Vec::new(),
        Some(AttrValue::Array(items)) => items
            .iter()
            .enumerate()
            .filter_map(|(i, v)| match v {
                AttrValue::String(s) => Some(InvocationPath::normalize(s)),
                other => {
                    errors.push(Diagnostic {
                        severity: Severity::Error,
                        code: Code::GeneratorOptionsInvalid,
                        message: format!(
                            "kiln: `generator.inputs[{i}]` must be a string, got {}",
                            attr_kind(other),
                        ),
                        span: Some(span_of(module_path, use_decl)),
                    });
                    None
                }
            })
            .collect(),
        Some(other) => {
            errors.push(Diagnostic {
                severity: Severity::Error,
                code: Code::GeneratorOptionsInvalid,
                message: format!(
                    "kiln: `generator.inputs` must be an array of strings, got {}",
                    attr_kind(other),
                ),
                span: Some(span_of(module_path, use_decl)),
            });
            Vec::new()
        }
    };

    let options_canonical = if let Some(module) = module.as_ref() {
        encode_options(
            module,
            cfg.get("options"),
            use_decl,
            module_path,
            descriptors,
            &mut errors,
        )
    } else {
        Vec::new()
    };

    let output_dir_override = match cfg.get("output_dir") {
        None => None,
        Some(AttrValue::String(s)) => Some(InvocationPath::normalize(s)),
        Some(other) => {
            errors.push(Diagnostic {
                severity: Severity::Error,
                code: Code::GeneratorOptionsInvalid,
                message: format!(
                    "kiln: `generator.output_dir` must be a string, got {}",
                    attr_kind(other),
                ),
                span: Some(span_of(module_path, use_decl)),
            });
            None
        }
    };

    if !errors.is_empty() {
        return Err(errors);
    }
    let module = module.expect("module was validated above");

    let synthetic_id = {
        let mut h = Sha256::new();
        h.update(module_key(&module).as_bytes());
        h.update(from.as_str().as_bytes());
        for p in &inputs {
            h.update(p.as_str().as_bytes());
            h.update([0u8]);
        }
        h.update(&options_canonical);
        let digest: [u8; 32] = h.finalize().into();
        format!("kiln-{}", &hex_digest(&digest)[..16])
    };

    let output_dir = output_dir_override.unwrap_or_else(|| {
        InvocationPath::normalize(&format!(
            "{DEFAULT_INLINE_OUTPUT_DIR_PREFIX}/{synthetic_id}"
        ))
    });

    Ok(Invocation {
        decl_site: DeclSite {
            module: module_path.to_string(),
            synthetic_id,
        },
        module,
        from,
        inputs,
        output_dir,
        options_canonical,
        raw_options: cfg.get("options").cloned(),
    })
}

/// Lower an inline `module: "<specifier>"` value to a [`GeneratorModule`].
///
/// Accepted shapes mirror the `<source>` slot of a regular `use ... from
/// "<source>"` clause:
///
/// - `./...` / `../...` — relative path resolved against the file that
///   carries the inline clause (`module_path`). This matches what the
///   loader does for the `from` slot two lines above.
/// - `<ns>:<name>[@<ver>]` — registry / stdlib namespace identifier.
///   Stored verbatim as a [`GeneratorModule::Spec`] string until the
///   build-dependency resolver lands.
///
/// A bare relative name without `./` is rejected with a hint to add the
/// prefix — the same diagnostic regular `use` clauses produce.
fn lower_module_specifier(
    module_path: &str,
    use_decl: &UseDecl,
    spec: &str,
    manifest_root: &str,
    errors: &mut Vec<Diagnostic>,
) -> Option<GeneratorModule> {
    // Relative path: resolve against the inline-clause's owning file the
    // same way the loader resolves a normal `use` import. This keeps the
    // author writing one consistent style of path inside a single `use`.
    //
    // The resolved path is then re-anchored to the manifest root so the
    // resulting `LocalPath` is reliably reachable from the consuming
    // project — the provider resolves it as `manifest_root.join(path)`
    // when reading the file on disk and computing a stable cache key.
    if spec.starts_with("./") || spec.starts_with("../") {
        let resolved = crate::name::resolve_module_path(module_path, spec);
        let manifest_relative = strip_manifest_root_prefix(manifest_root, &resolved);
        return Some(GeneratorModule::LocalPath(InvocationPath::normalize(
            &manifest_relative,
        )));
    }
    // Namespaced specifier (`ns:name@ver`, `core:foo`, `wasi:foo`, …).
    // The compiler does not interpret the body here — the build-dep
    // resolver / provider does.
    if let Some(colon) = spec.find(':')
        && colon > 0
        && spec[..colon]
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Some(GeneratorModule::Spec(spec.to_string()));
    }
    errors.push(Diagnostic {
        severity: Severity::Error,
        code: Code::GeneratorOptionsInvalid,
        message: format!(
            "kiln: `generator.module` must be a relative path (\"./generator.wado\") \
             or a namespaced spec (\"ns:name@ver\"), got `{spec}`"
        ),
        span: Some(span_of(module_path, use_decl)),
    });
    None
}

/// Re-anchor `resolved` (project-root-relative) to `manifest_root` so the
/// resulting [`GeneratorModule::LocalPath`] is relative to the manifest
/// root; the provider resolves it as `manifest_root.join(path)`.
///
/// When `resolved` lies under `manifest_root` the shared prefix is stripped.
/// When `resolved` lies above or beside `manifest_root` (e.g. an inline
/// clause in `wasm-size/foo/bar.wado` referencing
/// `../../package-gale/src/generator.wado`), `..` segments are emitted to
/// walk up out of `manifest_root` before descending into `resolved`. An
/// empty `manifest_root` means the path is already project-root-relative
/// and is returned verbatim.
fn strip_manifest_root_prefix(manifest_root: &str, resolved: &str) -> String {
    let root = manifest_root.trim_end_matches('/');
    if root.is_empty() {
        return resolved.to_string();
    }
    let root_parts: Vec<&str> = root.split('/').filter(|p| !p.is_empty()).collect();
    let resolved_parts: Vec<&str> = resolved.split('/').filter(|p| !p.is_empty()).collect();
    let common = root_parts
        .iter()
        .zip(resolved_parts.iter())
        .take_while(|(a, b)| a == b)
        .count();
    let ups = root_parts.len() - common;
    let mut out = Vec::with_capacity(ups + resolved_parts.len() - common);
    out.extend(std::iter::repeat_n("..", ups));
    out.extend(resolved_parts.iter().skip(common).copied());
    out.join("/")
}

fn encode_options(
    module: &GeneratorModule,
    options_value: Option<&AttrValue>,
    use_decl: &UseDecl,
    module_path: &str,
    descriptors: &IndexMap<String, OptionsDescriptor>,
    errors: &mut Vec<Diagnostic>,
) -> Vec<u8> {
    let key = module_key(module);
    let Some(descriptor) = descriptors.get(&key) else {
        return Vec::new();
    };

    let canonical: CanonicalOptions = match validate(descriptor, options_value) {
        Ok(c) => c,
        Err(mut errs) => {
            for d in &mut errs {
                if d.span.is_none() {
                    d.span = Some(span_of(module_path, use_decl));
                }
            }
            errors.append(&mut errs);
            return Vec::new();
        }
    };
    encode_options_canonical(&canonical)
}

fn module_key(module: &GeneratorModule) -> String {
    match module {
        GeneratorModule::Spec(s) => format!("spec:{s}"),
        GeneratorModule::LocalPath(p) => format!("path:{}", p.as_str()),
    }
}

fn identity_key(inv: &Invocation) -> String {
    let mut h = Sha256::new();
    h.update(module_key(&inv.module).as_bytes());
    h.update(inv.from.as_str().as_bytes());
    for p in &inv.inputs {
        h.update(p.as_str().as_bytes());
        h.update([0u8]);
    }
    h.update(inv.output_dir.as_str().as_bytes());
    h.update(&inv.options_canonical);
    let digest: [u8; 32] = h.finalize().into();
    hex_digest(&digest)
}

fn span_of(module_path: &str, use_decl: &UseDecl) -> DiagnosticSpan {
    DiagnosticSpan::from_span(&use_decl.span, Some(module_path))
}

fn attr_kind(v: &AttrValue) -> &'static str {
    match v {
        AttrValue::String(_) => "string",
        AttrValue::Int(_) => "integer",
        AttrValue::Float(_) => "float",
        AttrValue::Bool(_) => "bool",
        AttrValue::Array(_) => "array",
        AttrValue::Object(_) => "object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::{AstId, ImportAttributes, Item, UseDecl, UseItem};
    use crate::token::Span;

    fn span() -> Span {
        Span::new(0, 0, 1, 1)
    }

    fn module_with_use(source: &str, attrs: ImportAttributes) -> Module {
        let use_decl = UseDecl {
            id: AstId(0),
            is_pub: false,
            source: source.to_string(),
            source_span: span(),
            source_id: AstId(1),
            items: vec![UseItem::Wildcard],
            attributes: Some(attrs),
            span: span(),
        };
        Module::new(vec![Item::Use(use_decl)])
    }

    fn attr_with_generator(entries: &[(&str, AttrValue)]) -> ImportAttributes {
        let mut gen_obj: IndexMap<String, AttrValue> = IndexMap::default();
        for (k, v) in entries {
            gen_obj.insert((*k).to_string(), v.clone());
        }
        let mut entries_map: IndexMap<String, AttrValue> = IndexMap::default();
        entries_map.insert("generator".to_string(), AttrValue::Object(gen_obj));
        ImportAttributes {
            entries: entries_map,
        }
    }

    #[test]
    fn extracts_invocation_from_inline_clause() {
        let attrs =
            attr_with_generator(&[("module", AttrValue::String("ns:gen@1.0.0".to_string()))]);
        let module = module_with_use("./schema.proto", attrs);
        let mut mods: IndexMap<String, Module> = IndexMap::default();
        mods.insert("src/main.wado".to_string(), module);

        let result = collect_inline_invocations(
            mods.iter().map(|(k, v)| (k.as_str(), v)),
            &IndexMap::default(),
            "",
        )
        .unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].from.as_str(), "schema.proto");
        assert!(matches!(&result[0].module, GeneratorModule::Spec(s) if s == "ns:gen@1.0.0"));
        assert_eq!(result[0].decl_site.module, "src/main.wado");
    }

    #[test]
    fn missing_module_field_rejected() {
        let attrs = attr_with_generator(&[]);
        let module = module_with_use("./x.proto", attrs);
        let mut mods: IndexMap<String, Module> = IndexMap::default();
        mods.insert("src/main.wado".to_string(), module);

        let errs = collect_inline_invocations(
            mods.iter().map(|(k, v)| (k.as_str(), v)),
            &IndexMap::default(),
            "",
        )
        .unwrap_err();
        assert!(
            errs.iter()
                .any(|d| d.message.contains("requires a `module` field"))
        );
    }

    #[test]
    fn dedup_merges_identical_clauses() {
        let mk = || {
            let attrs =
                attr_with_generator(&[("module", AttrValue::String("ns:gen@1.0.0".to_string()))]);
            module_with_use("./schema.proto", attrs)
        };
        let mut mods: IndexMap<String, Module> = IndexMap::default();
        mods.insert("src/a.wado".to_string(), mk());
        mods.insert("src/b.wado".to_string(), mk());

        let result = collect_inline_invocations(
            mods.iter().map(|(k, v)| (k.as_str(), v)),
            &IndexMap::default(),
            "",
        )
        .unwrap();
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn conflict_detected_when_same_from_different_module() {
        let a = module_with_use(
            "./schema.proto",
            attr_with_generator(&[("module", AttrValue::String("ns:a@1.0.0".to_string()))]),
        );
        let b = module_with_use(
            "./schema.proto",
            attr_with_generator(&[("module", AttrValue::String("ns:b@1.0.0".to_string()))]),
        );
        let mut mods: IndexMap<String, Module> = IndexMap::default();
        mods.insert("src/a.wado".to_string(), a);
        mods.insert("src/b.wado".to_string(), b);

        let errs = collect_inline_invocations(
            mods.iter().map(|(k, v)| (k.as_str(), v)),
            &IndexMap::default(),
            "",
        )
        .unwrap_err();
        assert!(errs.iter().any(|d| d.message.contains("disagree")));
    }

    #[test]
    fn relative_module_specifier_resolved_against_importing_file() {
        let attrs =
            attr_with_generator(&[("module", AttrValue::String("../gen.wado".to_string()))]);
        let module = module_with_use("./x.proto", attrs);
        let mut mods: IndexMap<String, Module> = IndexMap::default();
        mods.insert("src/main.wado".to_string(), module);

        let result = collect_inline_invocations(
            mods.iter().map(|(k, v)| (k.as_str(), v)),
            &IndexMap::default(),
            "",
        )
        .unwrap();
        assert_eq!(result.len(), 1);
        match &result[0].module {
            GeneratorModule::LocalPath(p) => assert_eq!(p.as_str(), "gen.wado"),
            other => panic!("expected LocalPath, got {other:?}"),
        }
    }

    #[test]
    fn output_dir_override_is_respected() {
        let attrs = attr_with_generator(&[
            ("module", AttrValue::String("ns:gen@1.0.0".to_string())),
            ("output_dir", AttrValue::String("src/generated".to_string())),
        ]);
        let module = module_with_use("./schema.proto", attrs);
        let mut mods: IndexMap<String, Module> = IndexMap::default();
        mods.insert("src/main.wado".to_string(), module);

        let result = collect_inline_invocations(
            mods.iter().map(|(k, v)| (k.as_str(), v)),
            &IndexMap::default(),
            "",
        )
        .unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].output_dir.as_str(), "src/generated");
    }

    #[test]
    fn output_dir_non_string_rejected() {
        let attrs = attr_with_generator(&[
            ("module", AttrValue::String("ns:gen@1.0.0".to_string())),
            ("output_dir", AttrValue::Int(42)),
        ]);
        let module = module_with_use("./schema.proto", attrs);
        let mut mods: IndexMap<String, Module> = IndexMap::default();
        mods.insert("src/main.wado".to_string(), module);

        let errs = collect_inline_invocations(
            mods.iter().map(|(k, v)| (k.as_str(), v)),
            &IndexMap::default(),
            "",
        )
        .unwrap_err();
        assert!(errs.iter().any(|d| {
            d.message
                .contains("`generator.output_dir` must be a string")
        }));
    }

    #[test]
    fn invocation_index_redirects_on_match() {
        let mut idx = InvocationIndex::new();
        idx.insert(
            "src/main.wado",
            "./grammar.g4",
            "file:///abs/build/kiln/proto/grammar.wado",
        );
        assert_eq!(
            idx.redirect("src/main.wado", "./grammar.g4"),
            Some("file:///abs/build/kiln/proto/grammar.wado"),
        );
        assert_eq!(idx.redirect("src/main.wado", "./other.g4"), None);
    }

    #[test]
    fn invocation_index_normalizes_from_path() {
        let mut idx = InvocationIndex::new();
        idx.insert(
            "src/main.wado",
            "./grammar.g4",
            "file:///abs/build/kiln/x/g.wado",
        );
        assert_eq!(
            idx.redirect("src/main.wado", "grammar.g4"),
            Some("file:///abs/build/kiln/x/g.wado"),
        );
    }

    #[test]
    fn invocation_index_does_not_leak_across_files() {
        let mut idx = InvocationIndex::new();
        idx.insert("src/main.wado", "./grammar.g4", "file:///abs/g.wado");
        assert_eq!(
            idx.redirect("src/main.wado", "./grammar.g4"),
            Some("file:///abs/g.wado"),
        );
        // A different file does not pick up the redirect — that file would
        // need its own inline `with { ... }` clause.
        assert_eq!(idx.redirect("tests/other.wado", "./grammar.g4"), None);
    }

    #[test]
    fn strip_manifest_root_prefix_strips_when_inside() {
        let p = strip_manifest_root_prefix("project/sub", "project/sub/dir/gen.wado");
        assert_eq!(p, "dir/gen.wado");
    }

    #[test]
    fn strip_manifest_root_prefix_emits_dotdots_when_above() {
        let p = strip_manifest_root_prefix(
            "wasm-size/sqlite_highlight",
            "package-gale/src/generator.wado",
        );
        assert_eq!(p, "../../package-gale/src/generator.wado");
    }

    #[test]
    fn strip_manifest_root_prefix_handles_partial_overlap() {
        let p = strip_manifest_root_prefix("a/b/c", "a/x/y/gen.wado");
        assert_eq!(p, "../../x/y/gen.wado");
    }

    #[test]
    fn strip_manifest_root_prefix_empty_root_is_passthrough() {
        let p = strip_manifest_root_prefix("", "package/src/gen.wado");
        assert_eq!(p, "package/src/gen.wado");
    }
}
