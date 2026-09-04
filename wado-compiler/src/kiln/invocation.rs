//! Canonical, compiler-internal representation of a Kiln generator invocation.
//!
//! All invocations come from inline `use ... with { generator: ... }` clauses;
//! the manifest no longer declares any. See WEP 2026-04-12 §"Use-site syntax".

use std::fmt;

/// The `core:kiln/generator` world FQ name: what a generator package declares
/// in `[world]`, and the target world its source is analyzed under.
pub const GENERATOR_WORLD_FQ: &str = "core:kiln/generator";

/// The parts of a `module:` build-dependency specifier: the
/// `[build-dependencies]` lookup key (`ns:name` or `lib:nick`), an optional
/// pinned `@version`, and an optional `/submodule` path. The coordinate and
/// version never contain `/`, so the first `/` starts the submodule; the `@`
/// split only applies to a segment carrying a `:`.
pub struct SpecParts<'a> {
    pub key: &'a str,
    pub version: Option<&'a str>,
    pub submodule: Option<&'a str>,
}

/// Parse a `module:` specifier into its [`SpecParts`].
#[must_use]
pub fn parse_spec(spec: &str) -> SpecParts<'_> {
    let (head, submodule) = match spec.split_once('/') {
        Some((h, s)) => (h, Some(s)),
        None => (spec, None),
    };
    let (key, version) = match head.split_once('@') {
        Some((k, v)) if k.contains(':') => (k, Some(v)),
        _ => (head, None),
    };
    SpecParts {
        key,
        version,
        submodule,
    }
}

/// The `[build-dependencies]` lookup key for a `module:` specifier.
#[must_use]
pub fn spec_key(spec: &str) -> &str {
    parse_spec(spec).key
}

/// A forward-slash, NFC-normalized file path relative to the project root.
///
/// Wrapped to keep cache-key hashing consistent regardless of where the path
/// was typed (manifest vs. inline) or how the OS separates path components.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct InvocationPath(pub String);

impl InvocationPath {
    /// Normalize a raw manifest / source path: backslashes become forward
    /// slashes, a `./` prefix and trailing slashes go, and empty segments
    /// collapse. Only the structural cases that change identity — full NFC
    /// normalization happens in [`crate::kiln::cache`] at cache-key time.
    #[must_use]
    pub fn normalize(raw: &str) -> Self {
        let mut s = raw.replace('\\', "/");
        if let Some(rest) = s.strip_prefix("./") {
            s = rest.to_string();
        }
        while s.ends_with('/') {
            s.pop();
        }
        let parts: Vec<&str> = s.split('/').filter(|p| !p.is_empty()).collect();
        Self(parts.join("/"))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for InvocationPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Where a generator invocation was declared in the source tree.
///
/// Used for diagnostics; never part of the cache key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeclSite {
    /// Relative path of the Wado file containing the inline `with` clause.
    pub module: String,
    /// Synthesized id derived from the canonical invocation tuple.
    pub synthetic_id: String,
}

impl fmt::Display for DeclSite {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} (inline: {})", self.module, self.synthetic_id)
    }
}

/// A reference to the generator module source.
///
/// Comes from the `module: "..."` string at the inline `with` clause.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GeneratorModule {
    /// A `[build-dependencies]` specifier — an open coordinate
    /// (`"ns:name[@version]"`) or a `"lib:nickname"` indirection, with an
    /// optional `/<submodule>` suffix. Resolved by the host against
    /// `[build-dependencies]`, dispatching on the matched entry's source: a
    /// path entry is rewritten to [`GeneratorModule::LocalPath`] (compiled from
    /// source) before it reaches the provider; a registry entry is pulled as a
    /// prebuilt component. Bare names are rejected at lowering.
    Spec(GeneratorSpec),
    /// Resolved local path, already joined against the consuming file's directory.
    LocalPath(InvocationPath),
}

/// A `[build-dependencies]` specifier plus any inline registry source supplied
/// at the `use` site for single-file mode (no `wado.toml`). The compiler stores
/// the fields opaquely; the host resolves them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeneratorSpec {
    /// The specifier string (`ns:name[@ver]`, `lib:nick`, with an optional
    /// `/submodule` suffix).
    pub spec: String,
    /// Inline semver requirement (`generator.version`); single-file only.
    pub version: Option<String>,
    /// Inline registry URL (`generator.registry`); single-file only.
    pub registry: Option<String>,
}

impl From<&str> for GeneratorSpec {
    /// A plain specifier with no inline source (the common case).
    fn from(spec: &str) -> Self {
        Self {
            spec: spec.to_string(),
            version: None,
            registry: None,
        }
    }
}

impl From<String> for GeneratorSpec {
    fn from(spec: String) -> Self {
        Self {
            spec,
            version: None,
            registry: None,
        }
    }
}

impl GeneratorSpec {
    /// A stable identity string. With no inline source it is just the specifier
    /// (so the common case keeps a clean `ns:name@ver` identity); an inline
    /// registry/version is folded in so two clauses with the same specifier but
    /// a different inline source resolve to distinct generators.
    #[must_use]
    pub fn identity(&self) -> String {
        match (&self.version, &self.registry) {
            (None, None) => self.spec.clone(),
            (v, r) => format!(
                "{}|{}|{}",
                self.spec,
                v.as_deref().unwrap_or(""),
                r.as_deref().unwrap_or(""),
            ),
        }
    }
}

/// Compiler-internal canonical form of a Kiln generator invocation.
///
/// Two [`Invocation`]s are considered equivalent — and may be deduplicated by
/// [`crate::kiln::plan`] — iff their `module`, `from`, `inputs`, `output_dir`
/// and `options_canonical` are byte-equal.
#[derive(Debug, Clone)]
pub struct Invocation {
    pub decl_site: DeclSite,
    pub module: GeneratorModule,
    /// Primary schema file, resolved relative to the declaring `.wado` file
    /// (project-root-relative). Drives input loading, the cache key, and the
    /// default `output_dir`.
    pub from: InvocationPath,
    /// The literal `from "<source>"` path as written, only normalized (not
    /// resolved against the declaring file). Used solely as the loader-redirect
    /// key: the loader matches `(decl_file, import_source)` against it without
    /// resolving, so it must stay frame-independent. Excluded from
    /// [`Invocation::identity_tuple`].
    pub source: InvocationPath,
    /// Supplementary schema files, resolved relative to the declaring `.wado`
    /// file, preserving declaration order.
    pub inputs: Vec<InvocationPath>,
    /// Resolved output directory (default: `build/kiln/<synthesized-id>`).
    pub output_dir: InvocationPath,
    /// Validated, typed options for this invocation. Crosses to the generator
    /// as a typed WIT argument (the host builds a Component-Model value from
    /// it); its canonical byte encoding ([`Self::options_canonical`]) feeds the
    /// invocation cache key. Empty until the generator's `OptionsDescriptor`
    /// is known (inline when it is available, otherwise the driver's
    /// typed-encode pass).
    pub options: crate::kiln::options_check::CanonicalOptions,
    /// Raw `options` `AttrValue` recovered from the inline
    /// `with { generator: { options: { ... } } }` clause. Stashed so the
    /// driver's typed-encode pass can re-validate against the generator's
    /// `OptionsDescriptor` once it becomes available without re-reading or
    /// re-parsing the source. `None` when no `options` clause was supplied.
    /// Excluded from [`Invocation::identity_tuple`] — equivalence is decided
    /// by `options_canonical` alone.
    pub raw_options: Option<crate::ast::AttrValue>,
    /// Span of the inline `options:` key, or of the whole `use` clause when
    /// the clause wrote none: what the driver blames when it re-validates
    /// `raw_options`, long after the AST is gone. Diagnostics only; excluded
    /// from [`Invocation::identity_tuple`].
    pub options_span: crate::token::Span,
}

impl Invocation {
    /// Canonical byte encoding of [`Self::options`], for the invocation cache
    /// key and dedup identity. Deterministic and injective; not an interchange
    /// format (nothing decodes it — see
    /// [`crate::kiln::cache::encode_options_canonical`]).
    #[must_use]
    pub fn options_canonical(&self) -> Vec<u8> {
        crate::kiln::cache::encode_options_canonical(&self.options)
    }

    /// The tuple used for dedup and cycle detection.
    ///
    /// Does not include `decl_site`: two clauses in the same file with
    /// identical invocation tuples merge into one invocation.
    #[must_use]
    pub fn identity_tuple(&self) -> (&GeneratorModule, &str, &[InvocationPath], &str, Vec<u8>) {
        (
            &self.module,
            self.from.as_str(),
            self.inputs.as_slice(),
            self.output_dir.as_str(),
            self.options_canonical(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_spec_splits_version_and_submodule() {
        let bare = parse_spec("wado-lang:gale");
        assert_eq!(bare.key, "wado-lang:gale");
        assert_eq!(bare.version, None);
        assert_eq!(bare.submodule, None);

        let versioned = parse_spec("wado-lang:gale@0.0.9");
        assert_eq!(versioned.key, "wado-lang:gale");
        assert_eq!(versioned.version, Some("0.0.9"));
        assert_eq!(versioned.submodule, None);

        let sub = parse_spec("example:proto-codegen@1.2/generator");
        assert_eq!(sub.key, "example:proto-codegen");
        assert_eq!(sub.version, Some("1.2"));
        assert_eq!(sub.submodule, Some("generator"));

        let sub_no_ver = parse_spec("lib:gen/sub");
        assert_eq!(sub_no_ver.key, "lib:gen");
        assert_eq!(sub_no_ver.version, None);
        assert_eq!(sub_no_ver.submodule, Some("sub"));
    }

    #[test]
    fn normalize_strips_dot_slash() {
        assert_eq!(InvocationPath::normalize("./foo/bar").as_str(), "foo/bar");
    }

    #[test]
    fn normalize_collapses_backslashes() {
        assert_eq!(
            InvocationPath::normalize("foo\\bar\\baz").as_str(),
            "foo/bar/baz"
        );
    }

    #[test]
    fn normalize_trims_trailing_slash() {
        assert_eq!(InvocationPath::normalize("foo/bar/").as_str(), "foo/bar");
    }

    #[test]
    fn normalize_collapses_empty_segments() {
        assert_eq!(InvocationPath::normalize("foo//bar").as_str(), "foo/bar");
    }

    #[test]
    fn decl_site_display_inline() {
        let site = DeclSite {
            module: "main.wado".to_string(),
            synthetic_id: "kiln-deadbeef".to_string(),
        };
        assert_eq!(format!("{site}"), "main.wado (inline: kiln-deadbeef)");
    }

    #[test]
    fn identity_tuple_ignores_decl_site() {
        let inv_a = Invocation {
            decl_site: DeclSite {
                module: "a.wado".to_string(),
                synthetic_id: "kiln-aaaa".to_string(),
            },
            module: GeneratorModule::Spec("ns:x@1.0.0".into()),
            from: InvocationPath::normalize("s.proto"),
            source: InvocationPath::normalize("./s.proto"),
            inputs: vec![],
            output_dir: InvocationPath::normalize("build/kiln/a"),
            options: crate::kiln::options_check::CanonicalOptions::default(),
            raw_options: None,
            options_span: crate::token::Span::default(),
        };
        let inv_b = Invocation {
            decl_site: DeclSite {
                module: "b.wado".to_string(),
                synthetic_id: "kiln-bbbb".to_string(),
            },
            ..inv_a.clone()
        };
        assert_eq!(inv_a.identity_tuple(), inv_b.identity_tuple());
    }
}
