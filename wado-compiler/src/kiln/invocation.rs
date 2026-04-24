//! Canonical, compiler-internal representation of a Kiln generator invocation.
//!
//! The manifest layer ([`wado_manifest::GeneratorInvocation`]) and the inline
//! `use ... with { generator: ... }` AST both lower into [`Invocation`], so the
//! DAG, cache and pipeline logic in this module treat both sources
//! identically. See WEP 2026-04-12 §"Declaration sites".

use std::fmt;

/// A forward-slash, NFC-normalized file path relative to the project root.
///
/// Wrapped to keep cache-key hashing consistent regardless of where the path
/// was typed (manifest vs. inline) or how the OS separates path components.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct InvocationPath(pub String);

impl InvocationPath {
    /// Normalize a raw manifest / source path into the canonical form.
    ///
    /// - Backslashes are turned into forward slashes (Windows inputs).
    /// - `./` prefix is stripped.
    /// - Trailing slashes are trimmed.
    /// - Empty segments between separators collapse.
    ///
    /// Full NFC normalization of the string contents is done by
    /// [`crate::kiln::cache`] when the path is written into a cache key; here
    /// we only handle the structural cases that actually change identity.
    #[must_use]
    pub fn normalize(raw: &str) -> Self {
        let mut s = raw.replace('\\', "/");
        if let Some(rest) = s.strip_prefix("./") {
            s = rest.to_string();
        }
        while s.ends_with('/') {
            s.pop();
        }
        let is_absolute = s.starts_with('/');
        let parts: Vec<&str> = s.split('/').filter(|p| !p.is_empty()).collect();
        let joined = parts.join("/");
        if is_absolute {
            Self(format!("/{joined}"))
        } else {
            Self(joined)
        }
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
pub enum DeclSite {
    /// Declared in the root `wado.toml` under `[build.generators.<name>]`.
    Manifest { name: String },
    /// Declared inline via `use ... from "<from>" with { generator: {...} }`.
    Inline {
        /// Relative path of the Wado file containing the clause.
        module: String,
        /// Synthesized id derived from the canonical invocation tuple.
        synthetic_id: String,
    },
}

impl fmt::Display for DeclSite {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DeclSite::Manifest { name } => write!(f, "[build.generators.{name}]"),
            DeclSite::Inline {
                module,
                synthetic_id,
            } => write!(f, "{module} (inline: {synthetic_id})"),
        }
    }
}

/// A reference to the generator module source.
///
/// Lowered from [`wado_manifest::GeneratorModuleRef`] so the compiler layer
/// does not need to reason about dependency sources.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GeneratorModule {
    /// `"ns:name@version"`, with optional `/<submodule>` suffix.
    Spec(String),
    /// Resolved local path, already joined against the manifest root.
    LocalPath(InvocationPath),
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
    /// Primary schema file.
    pub from: InvocationPath,
    /// Supplementary schema files, preserving declaration order.
    pub inputs: Vec<InvocationPath>,
    /// Resolved output directory (default: `build/kiln/<name>`).
    pub output_dir: InvocationPath,
    /// Canonical byte encoding of the `options` object.
    ///
    /// M3 uses a provisional TOML canonicalization; M4 replaces this with the
    /// Component-Model lifted encoding produced by
    /// [`crate::kiln::options::encode_canonical`]. The boundary is isolated
    /// to one function (`cache::encode_options_canonical_provisional`) so
    /// cache keys never flip when M4 ships unless the user-facing options
    /// actually change.
    pub options_canonical: Vec<u8>,
}

impl Invocation {
    /// The tuple used for dedup and cycle detection.
    ///
    /// Does not include `decl_site`: two declarations with identical invocation
    /// tuples but different sites must merge (per WEP line 371).
    #[must_use]
    pub fn identity_tuple(&self) -> (&GeneratorModule, &str, &[InvocationPath], &str, &[u8]) {
        (
            &self.module,
            self.from.as_str(),
            self.inputs.as_slice(),
            self.output_dir.as_str(),
            self.options_canonical.as_slice(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn decl_site_display_manifest() {
        let site = DeclSite::Manifest {
            name: "proto".to_string(),
        };
        assert_eq!(format!("{site}"), "[build.generators.proto]");
    }

    #[test]
    fn identity_tuple_ignores_decl_site() {
        let inv_a = Invocation {
            decl_site: DeclSite::Manifest {
                name: "a".to_string(),
            },
            module: GeneratorModule::Spec("ns:x@1.0.0".to_string()),
            from: InvocationPath::normalize("s.proto"),
            inputs: vec![],
            output_dir: InvocationPath::normalize("build/kiln/a"),
            options_canonical: vec![],
        };
        let inv_b = Invocation {
            decl_site: DeclSite::Inline {
                module: "main.wado".to_string(),
                synthetic_id: "kiln-deadbeef".to_string(),
            },
            ..inv_a.clone()
        };
        assert_eq!(inv_a.identity_tuple(), inv_b.identity_tuple());
    }
}
