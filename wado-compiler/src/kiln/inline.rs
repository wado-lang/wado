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

use super::cache::{empty_options_canonical, encode_options_canonical, hex_digest};
use super::invocation::{DeclSite, GeneratorModule, Invocation, InvocationPath};
use super::options::OptionsDescriptor;
use super::options_check::{CanonicalOptions, validate};

/// Default output-directory prefix for inline invocations: each clause lands
/// under `build/kiln/<synthetic_id>` unless it declares its own `output_dir`.
pub const DEFAULT_INLINE_OUTPUT_DIR_PREFIX: &str = "build/kiln";

/// Elaborator-side lookup table that redirects a `use ... from "<from>"` whose
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
    /// (e.g. the elaborator) can short-circuit the redirect check.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Iterate over recorded entries as `(decl_file, from, entry_uri)`.
    ///
    /// For tooling that runs generators against a file's real path but then
    /// resolves that file under a different module identity (e.g. the
    /// synthetic entry of a `wado query --symbol` request): read the entries
    /// out, rewrite `decl_file` to the identity the loader will use, and
    /// rebuild a fresh index with [`insert`](Self::insert).
    pub fn entries(&self) -> impl Iterator<Item = (&str, &str, &str)> {
        self.entries
            .iter()
            .map(|((decl, from), uri)| (decl.as_str(), from.as_str(), uri.as_str()))
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

    // The literal source string keys the loader redirect (frame-independent);
    // `from` is the same path resolved relative to the declaring file.
    let source = InvocationPath::normalize(&use_decl.source);
    let from = resolve_or_reject(
        module_path,
        &use_decl.source,
        manifest_root,
        "from",
        use_decl,
        &mut errors,
    );
    let inputs = match cfg.get("inputs") {
        None => Vec::new(),
        Some(AttrValue::Array(items)) => items
            .iter()
            .enumerate()
            .filter_map(|(i, v)| match v {
                AttrValue::String(s) => Some(resolve_or_reject(
                    module_path,
                    s,
                    manifest_root,
                    &format!("generator.inputs[{i}]"),
                    use_decl,
                    &mut errors,
                )),
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
        Some(AttrValue::String(s)) => Some(resolve_or_reject(
            module_path,
            s,
            manifest_root,
            "generator.output_dir",
            use_decl,
            &mut errors,
        )),
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
        source,
        inputs,
        output_dir,
        options_canonical,
        raw_options: cfg.get("options").cloned(),
    })
}

/// Resolve a `./` or `../` path written in a `use` clause relative to the
/// declaring file (`module_path`), then re-anchor it to the manifest root so
/// the provider/pipeline can read it as `manifest_root.join(path)`. Callers
/// must ensure `raw` is `./`/`../`-prefixed (the `module` form gates on the
/// prefix; `from`/`inputs`/`output_dir` go through [`resolve_or_reject`]).
fn resolve_decl_relative(module_path: &str, raw: &str, manifest_root: &str) -> InvocationPath {
    let resolved = crate::name::resolve_module_path(module_path, raw);
    let manifest_relative = strip_manifest_root_prefix(manifest_root, &resolved);
    InvocationPath::normalize(&manifest_relative)
}

/// Resolve a `from` / `inputs` / `output_dir` path, enforcing the Wado path
/// convention: a path relative to the declaring file must be written with a
/// `./` or `../` prefix (matching local module imports). A bare path is
/// rejected — it is reserved for a future manifest-root sigil. On rejection an
/// error diagnostic is pushed and the raw path is returned as a placeholder
/// (the caller bails because the diagnostics list is non-empty).
fn resolve_or_reject(
    module_path: &str,
    raw: &str,
    manifest_root: &str,
    field: &str,
    use_decl: &UseDecl,
    errors: &mut Vec<Diagnostic>,
) -> InvocationPath {
    if raw.starts_with("./") || raw.starts_with("../") {
        return resolve_decl_relative(module_path, raw, manifest_root);
    }
    errors.push(Diagnostic {
        severity: Severity::Error,
        code: Code::GeneratorOptionsInvalid,
        message: format!(
            "kiln: `{field}` must be a relative path starting with `./` or `../`, got `{raw}`"
        ),
        span: Some(span_of(module_path, use_decl)),
    });
    InvocationPath::normalize(raw)
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
///   build-dependency elaborator lands.
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
        return Some(GeneratorModule::LocalPath(resolve_decl_relative(
            module_path,
            spec,
            manifest_root,
        )));
    }
    // Namespaced specifier (`ns:name@ver`, `core:foo`, `wasi:foo`, …).
    // The compiler does not interpret the body here — the build-dep
    // elaborator / provider does.
    if let Some(colon) = spec.find(':')
        && colon > 0
        && spec[..colon]
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Some(GeneratorModule::Spec(spec.to_string()));
    }
    // Bare `[build-dependencies]` key (`module: "gale"`). Resolved by the
    // host against the manifest's build-dependency graph.
    if !spec.is_empty()
        && spec
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Some(GeneratorModule::BuildDep(spec.to_string()));
    }
    errors.push(Diagnostic {
        severity: Severity::Error,
        code: Code::GeneratorOptionsInvalid,
        message: format!(
            "kiln: `generator.module` must be a relative path (\"./generator.wado\"), \
             a `[build-dependencies]` name (\"gale\"), or a namespaced spec \
             (\"ns:name@ver\"), got `{spec}`"
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
/// walk up out of `manifest_root` before descending into `resolved`.
///
/// An empty or `.` `manifest_root` returns the path verbatim. `.` segments are
/// dropped on both sides, so a `.` root (compiling an entry next to
/// `wado.toml`) does not emit a spurious leading `..` that would put output
/// outside the project (e.g. `output_dir: "generated"` → `../generated`).
fn strip_manifest_root_prefix(manifest_root: &str, resolved: &str) -> String {
    let is_segment = |p: &&str| !p.is_empty() && *p != ".";
    let root_parts: Vec<&str> = manifest_root.split('/').filter(is_segment).collect();
    if root_parts.is_empty() {
        return resolved.to_string();
    }
    let resolved_parts: Vec<&str> = resolved.split('/').filter(is_segment).collect();
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
        // The descriptor is filled in later by the CLI provider.
        return empty_options_canonical();
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
            return empty_options_canonical();
        }
    };
    encode_options_canonical(&canonical)
}

fn module_key(module: &GeneratorModule) -> String {
    match module {
        GeneratorModule::Spec(s) => format!("spec:{s}"),
        GeneratorModule::LocalPath(p) => format!("path:{}", p.as_str()),
        GeneratorModule::BuildDep(s) => format!("builddep:{s}"),
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
            id: AstId::fresh(),
            visibility: crate::ast::Visibility::Private,
            source: source.to_string(),
            source_span: span(),
            source_id: AstId::fresh(),
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
        // `from` is resolved relative to the declaring file (`src/main.wado`).
        assert_eq!(result[0].from.as_str(), "src/schema.proto");
        // The literal source string is preserved for the loader redirect key.
        assert_eq!(result[0].source.as_str(), "schema.proto");
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
    fn output_dir_override_relative_with_dot_slash() {
        // `./generated` resolves relative to the declaring file (`src/main.wado`).
        let attrs = attr_with_generator(&[
            ("module", AttrValue::String("ns:gen@1.0.0".to_string())),
            ("output_dir", AttrValue::String("./generated".to_string())),
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
        assert_eq!(result[0].source.as_str(), "schema.proto");
    }

    #[test]
    fn bare_output_dir_is_rejected() {
        // A bare (non-`./`/`../`) path is rejected; relative paths must be
        // explicit, matching local module imports.
        let attrs = attr_with_generator(&[
            ("module", AttrValue::String("ns:gen@1.0.0".to_string())),
            (
                "output_dir",
                AttrValue::String("tests/generated/calc".to_string()),
            ),
        ]);
        let module = module_with_use("./schema.proto", attrs);
        let mut mods: IndexMap<String, Module> = IndexMap::default();
        mods.insert("tests/deep/nested/calc_test.wado".to_string(), module);

        let errs = collect_inline_invocations(
            mods.iter().map(|(k, v)| (k.as_str(), v)),
            &IndexMap::default(),
            "",
        )
        .unwrap_err();
        assert!(errs.iter().any(|d| {
            d.message.contains("generator.output_dir")
                && d.message
                    .contains("must be a relative path starting with `./` or `../`")
        }));
    }

    #[test]
    fn bare_from_is_rejected() {
        let attrs =
            attr_with_generator(&[("module", AttrValue::String("ns:gen@1.0.0".to_string()))]);
        // `from` without a `./` prefix.
        let module = module_with_use("schema.proto", attrs);
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
                .any(|d| d.message.contains("`from` must be a relative path"))
        );
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

    #[test]
    fn strip_manifest_root_prefix_dot_root_is_passthrough() {
        // `.` (entry sits next to `wado.toml`) must behave like an empty root,
        // not emit a spurious `..` that escapes the project.
        assert_eq!(strip_manifest_root_prefix(".", "generated"), "generated");
        assert_eq!(strip_manifest_root_prefix(".", "src/x.wado"), "src/x.wado");
        assert_eq!(strip_manifest_root_prefix("./", "schema.g4"), "schema.g4");
        // A `.` root passes the resolved path through verbatim (the caller
        // runs it through `InvocationPath::normalize`, which strips `./`).
        assert_eq!(
            strip_manifest_root_prefix(".", "./schema.g4"),
            "./schema.g4"
        );
    }

    #[test]
    fn output_dir_override_relative_to_root_entry() {
        // Regression: entry at the manifest root → manifest_root is `.`; a
        // `./`-relative `output_dir` must stay inside the project, not resolve
        // to `../` (the `.` segment must not become a spurious `..`).
        let attrs = attr_with_generator(&[
            ("module", AttrValue::String("ns:gen@1.0.0".to_string())),
            ("output_dir", AttrValue::String("./generated".to_string())),
        ]);
        let module = module_with_use("./schema.g4", attrs);
        let mut mods: IndexMap<String, Module> = IndexMap::default();
        mods.insert("main.wado".to_string(), module);

        let result = collect_inline_invocations(
            mods.iter().map(|(k, v)| (k.as_str(), v)),
            &IndexMap::default(),
            ".",
        )
        .unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].output_dir.as_str(), "generated");
        assert_eq!(result[0].from.as_str(), "schema.g4");
    }
}
