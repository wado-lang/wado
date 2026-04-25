//! Inline-invocation collection for `use ... with { generator: { ... } }`.
//!
//! An inline clause declares a Kiln invocation directly at the import site
//! instead of inside `wado.toml`. This module walks the parsed [`UseDecl`]s,
//! extracts the `generator: {...}` object from
//! [`crate::ast::ImportAttributes`], and builds matching
//! [`crate::kiln::Invocation`]s. The result can be merged with manifest-lowered
//! invocations before [`crate::kiln::plan::build_plan`] runs.
//!
//! Two clauses with identical `(module, from, inputs, output_dir,
//! options_canonical)` tuples are merged into a single invocation; this is the
//! same dedup contract as the manifest path. Two clauses that share `from` but
//! disagree on any other field produce a diagnostic citing both spans.

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
/// `<from>` path matches a Kiln invocation's primary source to the
/// invocation's generated entry module.
///
/// Built once per compilation unit from the merged manifest + inline
/// invocation set, after the pipeline has populated `build/kiln/…` so the
/// entry module's on-disk location is known. For consume-only mode (no
/// pipeline run), the index is built from the last recorded lockfile entry's
/// `output.entry = true` path — that path lives on disk already or the
/// cache check failed.
///
/// Lookups are keyed by `(declaring_file_normalized, from_path_normalized)`.
/// Two clauses declared in different files can redirect to the same entry
/// iff the manifest + inline merge keeps them under a single invocation.
/// Scope at which an [`InvocationIndex`] entry takes effect.
///
/// Drop-in replacement for the former "empty-string `decl_file` means
/// any importer" sentinel: manifest-declared invocations use
/// [`DeclScope::Any`] (redirect any importer that imports the
/// matching `from`), while inline-declared invocations use
/// [`DeclScope::LocalTo`] with the declaring file path and only
/// redirect that specific file's imports.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum DeclScope {
    /// Match any importer. Used for manifest-declared invocations.
    Any,
    /// Match only the named importing file (normalized path). Used
    /// for inline `use ... with { generator: { ... } }` clauses.
    LocalTo(String),
}

#[derive(Debug, Default, Clone)]
pub struct InvocationIndex {
    /// Entries mapping `(scope, from_path)` → entry module URI. The URI
    /// is an opaque resource identifier (typically a `file:` URI
    /// produced by the CLI pipeline) that the loader hands verbatim to
    /// `CompilerHost::load_source` — it is not normalized as a path.
    entries: IndexMap<(DeclScope, String), String>,
}

impl InvocationIndex {
    /// Create an empty index.
    #[must_use]
    pub fn new() -> Self {
        Self {
            entries: IndexMap::default(),
        }
    }

    /// Record that a `use ... from "<from>"` within `scope` redirects to
    /// `entry_uri`. The `from` path is normalized for matching; the
    /// `entry_uri` is stored opaquely.
    ///
    /// Silently overwrites a prior entry with the same key. The caller is
    /// responsible for ensuring the set of recorded invocations is
    /// conflict-free (see [`merge_manifest_and_inline`]).
    pub fn insert(&mut self, scope: DeclScope, from: &str, entry_uri: &str) {
        let from = InvocationPath::normalize(from).as_str().to_string();
        self.entries.insert((scope, from), entry_uri.to_string());
    }

    /// Look up the entry URI for a `(decl_file, from)` pair. Returns the
    /// opaque URI of the entry module, or `None` if no invocation
    /// matches.
    ///
    /// The `(LocalTo(decl_file), from)` key is tried first; on miss the
    /// lookup falls back to `(Any, from)` so manifest-scoped invocations
    /// redirect any importer.
    #[must_use]
    pub fn redirect(&self, decl_file: &str, from: &str) -> Option<&str> {
        let from = InvocationPath::normalize(from).as_str().to_string();
        if let Some(entry) = self
            .entries
            .get(&(DeclScope::LocalTo(decl_file.to_string()), from.clone()))
        {
            return Some(entry.as_str());
        }
        self.entries
            .get(&(DeclScope::Any, from))
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
pub fn collect_inline_invocations(
    modules: &IndexMap<String, Module>,
    descriptors: &IndexMap<String, OptionsDescriptor>,
    manifest_root: &str,
) -> Result<Vec<Invocation>, Vec<Diagnostic>> {
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
                    by_from.insert(from_key, (invocation, module_path.clone()));
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

/// Merge manifest-declared invocations with inline-collected ones.
///
/// Cross-source `from` conflicts — a manifest invocation and an inline
/// clause both targeting the same primary source with different
/// `(module, inputs, output_dir, options_canonical)` — surface as error
/// diagnostics. Identical tuples are deduplicated.
///
/// # Errors
/// Every cross-source conflict is reported. Returns `Err` when any error
/// diagnostic is emitted.
pub fn merge_manifest_and_inline(
    manifest: Vec<Invocation>,
    inline: Vec<Invocation>,
) -> Result<Vec<Invocation>, Vec<Diagnostic>> {
    let mut diagnostics: Vec<Diagnostic> = Vec::new();
    let mut out: Vec<Invocation> = Vec::with_capacity(manifest.len() + inline.len());
    let mut by_tuple: IndexMap<String, usize> = IndexMap::default();
    let mut by_from: IndexMap<String, usize> = IndexMap::default();

    for inv in manifest.into_iter().chain(inline) {
        let tuple_key = identity_key(&inv);
        if by_tuple.contains_key(&tuple_key) {
            continue;
        }
        let from_key = inv.from.as_str().to_string();
        if let Some(&idx) = by_from.get(&from_key) {
            let prior = &out[idx];
            diagnostics.push(Diagnostic {
                severity: Severity::Error,
                code: Code::GeneratorOptionsInvalid,
                message: format!(
                    "kiln: generator invocations for `from = \"{}\"` disagree between {} and {}",
                    inv.from.as_str(),
                    prior.decl_site,
                    inv.decl_site,
                ),
                span: None,
            });
            continue;
        }
        by_tuple.insert(tuple_key, out.len());
        by_from.insert(from_key, out.len());
        out.push(inv);
    }

    if diagnostics.iter().any(|d| d.severity == Severity::Error) {
        return Err(diagnostics);
    }
    Ok(out)
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
    // Anything else is rejected. The TOML manifest's
    // `[build.generators.<name>]` declaration keeps its own
    // wado.toml-rooted shape (`module = { path = "..." }`); the inline
    // form intentionally does *not* mirror it because the author is
    // already writing path-relative imports a few characters earlier
    // (`use ... from "./schema.g4"`).
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

    let output_dir = InvocationPath::normalize(&format!(
        "{DEFAULT_INLINE_OUTPUT_DIR_PREFIX}/{synthetic_id}"
    ));

    Ok(Invocation {
        decl_site: DeclSite::Inline {
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
    // resulting `LocalPath` matches the manifest TOML form's invariant
    // ("path is relative to the manifest root"), which the provider relies
    // on to find the file on disk and to build a stable cache key.
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
/// result matches the manifest TOML form's invariant: paths in
/// [`GeneratorModule::LocalPath`] are relative to the manifest root, and
/// the provider resolves them as `manifest_root.join(path)`.
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

        let result = collect_inline_invocations(&mods, &IndexMap::default(), "").unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].from.as_str(), "schema.proto");
        assert!(matches!(&result[0].module, GeneratorModule::Spec(s) if s == "ns:gen@1.0.0"));
        assert!(
            matches!(&result[0].decl_site, DeclSite::Inline { module, .. } if module == "src/main.wado")
        );
    }

    #[test]
    fn missing_module_field_rejected() {
        let attrs = attr_with_generator(&[]);
        let module = module_with_use("./x.proto", attrs);
        let mut mods: IndexMap<String, Module> = IndexMap::default();
        mods.insert("src/main.wado".to_string(), module);

        let errs = collect_inline_invocations(&mods, &IndexMap::default(), "").unwrap_err();
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

        let result = collect_inline_invocations(&mods, &IndexMap::default(), "").unwrap();
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

        let errs = collect_inline_invocations(&mods, &IndexMap::default(), "").unwrap_err();
        assert!(errs.iter().any(|d| d.message.contains("disagree")));
    }

    #[test]
    fn relative_module_specifier_resolved_against_importing_file() {
        let attrs =
            attr_with_generator(&[("module", AttrValue::String("../gen.wado".to_string()))]);
        let module = module_with_use("./x.proto", attrs);
        let mut mods: IndexMap<String, Module> = IndexMap::default();
        mods.insert("src/main.wado".to_string(), module);

        let result = collect_inline_invocations(&mods, &IndexMap::default(), "").unwrap();
        assert_eq!(result.len(), 1);
        match &result[0].module {
            GeneratorModule::LocalPath(p) => assert_eq!(p.as_str(), "gen.wado"),
            other => panic!("expected LocalPath, got {other:?}"),
        }
    }

    #[test]
    fn merge_reports_cross_source_conflict() {
        let manifest_inv = Invocation {
            decl_site: DeclSite::Manifest {
                name: "proto".to_string(),
            },
            module: GeneratorModule::Spec("ns:a@1.0.0".to_string()),
            from: InvocationPath::normalize("./schema.proto"),
            inputs: vec![],
            output_dir: InvocationPath::normalize("build/kiln/proto"),
            options_canonical: vec![],
            raw_options: None,
        };
        let inline_inv = Invocation {
            decl_site: DeclSite::Inline {
                module: "src/main.wado".to_string(),
                synthetic_id: "kiln-deadbeef".to_string(),
            },
            module: GeneratorModule::Spec("ns:b@1.0.0".to_string()),
            from: InvocationPath::normalize("./schema.proto"),
            inputs: vec![],
            output_dir: InvocationPath::normalize("build/kiln/kiln-deadbeef"),
            options_canonical: vec![],
            raw_options: None,
        };
        let errs = merge_manifest_and_inline(vec![manifest_inv], vec![inline_inv]).unwrap_err();
        assert!(errs.iter().any(|d| d.message.contains("disagree between")));
    }

    #[test]
    fn invocation_index_redirects_on_match() {
        let mut idx = InvocationIndex::new();
        idx.insert(
            DeclScope::LocalTo("src/main.wado".to_string()),
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
            DeclScope::LocalTo("src/main.wado".to_string()),
            "./grammar.g4",
            "file:///abs/build/kiln/x/g.wado",
        );
        assert_eq!(
            idx.redirect("src/main.wado", "grammar.g4"),
            Some("file:///abs/build/kiln/x/g.wado"),
        );
    }

    #[test]
    fn invocation_index_any_scope_matches_every_importer() {
        let mut idx = InvocationIndex::new();
        idx.insert(DeclScope::Any, "./grammar.g4", "file:///abs/g.wado");
        assert_eq!(
            idx.redirect("src/main.wado", "grammar.g4"),
            Some("file:///abs/g.wado"),
        );
        assert_eq!(
            idx.redirect("tests/other.wado", "./grammar.g4"),
            Some("file:///abs/g.wado"),
        );
    }

    #[test]
    fn invocation_index_local_scope_takes_precedence_over_any() {
        let mut idx = InvocationIndex::new();
        idx.insert(DeclScope::Any, "./grammar.g4", "file:///abs/manifest.wado");
        idx.insert(
            DeclScope::LocalTo("src/main.wado".to_string()),
            "./grammar.g4",
            "file:///abs/local.wado",
        );
        assert_eq!(
            idx.redirect("src/main.wado", "./grammar.g4"),
            Some("file:///abs/local.wado"),
        );
        assert_eq!(
            idx.redirect("tests/other.wado", "./grammar.g4"),
            Some("file:///abs/manifest.wado"),
        );
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

    #[test]
    fn merge_dedups_identical_invocations() {
        let a = Invocation {
            decl_site: DeclSite::Manifest {
                name: "proto".to_string(),
            },
            module: GeneratorModule::Spec("ns:a@1.0.0".to_string()),
            from: InvocationPath::normalize("./schema.proto"),
            inputs: vec![],
            output_dir: InvocationPath::normalize("build/kiln/proto"),
            options_canonical: vec![],
            raw_options: None,
        };
        let merged = merge_manifest_and_inline(vec![a.clone()], vec![a]).unwrap();
        assert_eq!(merged.len(), 1);
    }
}
