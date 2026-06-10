//! Module identity (`ModuleSource`) and its interner.
//!
//! `ModuleSource` is the canonical identity for a Wado module — the
//! "where did this code come from" half of `symbol-coordinate = (ModuleSource,
//! AstId)`. It is **not** a name-mangling concept: rendering it into a
//! mangled symbol is the job of `crate::name`.
//!
//! # Pointer-identity interning
//!
//! `ModuleSource` is used pervasively as an `IndexMap` key during
//! monomorphization and elaborator lookups. To make `clone` / `eq` /
//! `hash` all O(1), every string field is canonicalised into an
//! `Arc<str>` shared via [`ModuleSourceInterner`], which wraps the
//! generic [`StringInterner`] from [`crate::intern`].
//!
//! Well-known names (the targets of zero-arg constructors like
//! [`ModuleSource::prelude`]) live in `LazyLock<Arc<str>>` statics so
//! they can be constructed without an interner reference. The interner
//! adopts these statics on construction, ensuring that
//! `interner.core("prelude") == ModuleSource::prelude()` (ptr-equal).

use crate::intern::{InternedStr, StringInterner};
use std::fmt;
use std::sync::{Arc, LazyLock};

/// Sentinel `Arc<str>` for `ModuleSource::default()` placeholders.
/// Distinct identity from any real interned core name (no real module
/// has empty content).
static PLACEHOLDER_NAME: LazyLock<Arc<str>> = LazyLock::new(|| Arc::<str>::from(""));

static CORE_PRELUDE: LazyLock<Arc<str>> = LazyLock::new(|| Arc::<str>::from("prelude"));
static CORE_BUILTIN: LazyLock<Arc<str>> = LazyLock::new(|| Arc::<str>::from("builtin"));
static CORE_INTERNAL: LazyLock<Arc<str>> = LazyLock::new(|| Arc::<str>::from("internal"));
static CORE_ALLOCATOR: LazyLock<Arc<str>> = LazyLock::new(|| Arc::<str>::from("allocator"));
/// Shared between [`ModuleSource::cli`] (core:cli) and
/// [`ModuleSource::wasi_cli`] (wasi:cli) — both literal contents are
/// `"cli"`, and the interner deduplicates by content. Keeping a single
/// canonical `Arc` ensures both constructors return values that are
/// pointer-equal to `interner.intern("cli")`.
static NAME_CLI: LazyLock<Arc<str>> = LazyLock::new(|| Arc::<str>::from("cli"));
static CORE_PRELUDE_STRING: LazyLock<Arc<str>> =
    LazyLock::new(|| Arc::<str>::from("prelude/string.wado"));
static CORE_PRELUDE_LIST: LazyLock<Arc<str>> =
    LazyLock::new(|| Arc::<str>::from("prelude/list.wado"));
static CORE_PRELUDE_ARRAY: LazyLock<Arc<str>> =
    LazyLock::new(|| Arc::<str>::from("prelude/array.wado"));
static CORE_PRELUDE_FORMAT: LazyLock<Arc<str>> =
    LazyLock::new(|| Arc::<str>::from("prelude/format.wado"));
static CORE_PRELUDE_INT128: LazyLock<Arc<str>> =
    LazyLock::new(|| Arc::<str>::from("prelude/int128.wado"));
static CORE_PRELUDE_PRIMITIVE: LazyLock<Arc<str>> =
    LazyLock::new(|| Arc::<str>::from("prelude/primitive.wado"));
static CORE_PRELUDE_TYPES: LazyLock<Arc<str>> =
    LazyLock::new(|| Arc::<str>::from("prelude/types.wado"));
static CORE_PRELUDE_TRAITS: LazyLock<Arc<str>> =
    LazyLock::new(|| Arc::<str>::from("prelude/traits.wado"));
static CORE_PRELUDE_RANGE: LazyLock<Arc<str>> = LazyLock::new(|| Arc::<str>::from("prelude/range"));
static CORE_SERDE: LazyLock<Arc<str>> = LazyLock::new(|| Arc::<str>::from("serde"));

// Well-known WASI interface names embedded in the compiler.
// (`wasi_cli`'s arc is `NAME_CLI` above, shared with `core:cli`.)
static WASI_CLOCKS: LazyLock<Arc<str>> = LazyLock::new(|| Arc::<str>::from("clocks"));
static WASI_FILESYSTEM: LazyLock<Arc<str>> = LazyLock::new(|| Arc::<str>::from("filesystem"));
static WASI_HTTP: LazyLock<Arc<str>> = LazyLock::new(|| Arc::<str>::from("http"));

// Synthetic entry-point filenames used by from_path / loader.
static ENTRY_FILENAME_ENTRY: LazyLock<Arc<str>> = LazyLock::new(|| Arc::<str>::from("<entry>"));
static ENTRY_FILENAME_STDIN: LazyLock<Arc<str>> = LazyLock::new(|| Arc::<str>::from("<stdin>"));
static ENTRY_FILENAME_UNINITIALIZED: LazyLock<Arc<str>> =
    LazyLock::new(|| Arc::<str>::from("<uninitialized>"));

fn well_known_arcs() -> Vec<Arc<str>> {
    let mut arcs: Vec<Arc<str>> = vec![
        PLACEHOLDER_NAME.clone(),
        CORE_PRELUDE.clone(),
        CORE_BUILTIN.clone(),
        CORE_INTERNAL.clone(),
        CORE_ALLOCATOR.clone(),
        NAME_CLI.clone(),
        CORE_PRELUDE_STRING.clone(),
        CORE_PRELUDE_LIST.clone(),
        CORE_PRELUDE_ARRAY.clone(),
        CORE_PRELUDE_FORMAT.clone(),
        CORE_PRELUDE_INT128.clone(),
        CORE_PRELUDE_PRIMITIVE.clone(),
        CORE_PRELUDE_TYPES.clone(),
        CORE_PRELUDE_TRAITS.clone(),
        CORE_PRELUDE_RANGE.clone(),
        CORE_SERDE.clone(),
        WASI_CLOCKS.clone(),
        WASI_FILESYSTEM.clone(),
        WASI_HTTP.clone(),
        ENTRY_FILENAME_ENTRY.clone(),
        ENTRY_FILENAME_STDIN.clone(),
        ENTRY_FILENAME_UNINITIALIZED.clone(),
    ];
    arcs.extend(STDLIB_NAME_ARCS.iter().cloned());
    arcs
}

/// Canonical `Arc<str>` values for every stdlib module's `ModuleSource`
/// payload.
///
/// Derived from [`crate::stdlib::ALL_CORE_MODULES`],
/// [`crate::stdlib::ALL_WASI_MODULES`] and
/// [`crate::stdlib::ALL_CORE_WASM_ASSETS`] — the single source of truth
/// for the stdlib module set.  Every [`ModuleSourceInterner`] adopts
/// these, so `ModuleSource` values for stdlib modules compare
/// pointer-equal across independently constructed interners. This is
/// the foundation that lets the stdlib TIR cache key its `IndexMaps` by
/// `ModuleSource`.
///
/// For each variant, the interned payload is the portion of the
/// import path that lands inside the `ModuleSource` variant:
///
/// * [`ModuleSource::Core`] holds the path stripped of its `core:`
///   prefix (e.g. `"prelude"`, `"prelude/types.wado"`).
/// * [`ModuleSource::Wasi`] holds the path stripped of its `wasi:`
///   prefix.
/// * [`ModuleSource::Wasm`] holds the full canonical path including
///   the `core:` prefix (matching what the loader passes to
///   [`ModuleSourceInterner::wasm`]).
static STDLIB_NAME_ARCS: LazyLock<Vec<Arc<str>>> = LazyLock::new(|| {
    let mut arcs: Vec<Arc<str>> = Vec::new();
    for (path, _src) in crate::stdlib::ALL_CORE_MODULES {
        let name = path.strip_prefix("core:").unwrap_or(path);
        arcs.push(Arc::<str>::from(name));
    }
    for (path, _src) in crate::stdlib::ALL_WASI_MODULES {
        let interface = path.strip_prefix("wasi:").unwrap_or(path);
        arcs.push(Arc::<str>::from(interface));
    }
    for (path, _bytes) in crate::stdlib::ALL_CORE_WASM_ASSETS {
        // The loader interns the full canonical path for `Wasm`
        // variants (no prefix stripping); see `ModuleLoader::handle_wasm_import`.
        arcs.push(Arc::<str>::from(*path));
    }
    arcs
});

/// Interner for `ModuleSource` payloads. Wraps a generic
/// [`StringInterner`] and adopts every well-known
/// `LazyLock<Arc<str>>` static (`PLACEHOLDER_NAME`, `CORE_PRELUDE`, ...)
/// at construction. As a result, calls like `interner.core("prelude")`
/// return the same `Arc` as the static — and therefore the same
/// [`InternedStr`] as [`ModuleSource::prelude`].
#[derive(Debug)]
pub struct ModuleSourceInterner {
    strings: StringInterner,
}

impl ModuleSourceInterner {
    pub fn new() -> Self {
        Self {
            strings: StringInterner::with_well_known_arcs(well_known_arcs()),
        }
    }

    pub fn intern(&mut self, s: &str) -> InternedStr {
        self.strings.intern(s)
    }

    pub fn core(&mut self, name: &str) -> ModuleSource {
        ModuleSource::Core {
            name: self.intern(name),
        }
    }
    pub fn wasi(&mut self, interface: &str) -> ModuleSource {
        ModuleSource::Wasi {
            interface: self.intern(interface),
        }
    }
    pub fn local(&mut self, path: &str) -> ModuleSource {
        ModuleSource::Local {
            path: self.intern(path),
        }
    }
    pub fn remote(&mut self, url: &str) -> ModuleSource {
        ModuleSource::Remote {
            url: self.intern(url),
        }
    }
    pub fn redirected(&mut self, uri: &str) -> ModuleSource {
        ModuleSource::Redirected {
            uri: self.intern(uri),
        }
    }
    pub fn wasm(&mut self, path: &str, kind: WasmAssetKind) -> ModuleSource {
        ModuleSource::Wasm {
            path: self.intern(path),
            kind,
        }
    }
    pub fn entry_point(&mut self, filename: &str) -> ModuleSource {
        ModuleSource::EntryPoint {
            filename: self.intern(filename),
        }
    }

    /// Convert from the legacy `&[String]` module path representation.
    pub fn from_path(&mut self, segments: &[String]) -> ModuleSource {
        match segments {
            // Legacy: empty path represents entry module.
            [] => ModuleSource::entry_point_synthetic(),
            [first] if first.starts_with("./") || first.starts_with("../") => self.local(first),
            [first, rest @ ..] if first == "core" => self.core(&rest.join("/")),
            [first, rest @ ..] if first == "wasi" => self.wasi(&rest.join("/")),
            segments => self.local(&segments.join("/")),
        }
    }
}

impl Default for ModuleSourceInterner {
    fn default() -> Self {
        Self::new()
    }
}

/// Format of a wasm asset imported via `use ... with { type: "..." }`.
///
/// Phase 1 supports only core wasm (no Component Model) — see
/// `docs/wep-2026-01-10-wasm-import.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WasmAssetKind {
    /// `.wat` text format. Parsed via `wat::parse_bytes` before further
    /// processing. Used by `lib/core/libm.wat` and by user code that
    /// writes `with { type: "wat" }`.
    Wat,
    /// Raw `.wasm` binary. Used by user code that writes
    /// `with { type: "wasm" }`.
    Wasm,
}

impl WasmAssetKind {
    /// Format string used in `with { type: "..." }` attributes.
    #[must_use]
    pub fn type_attr(&self) -> &'static str {
        match self {
            Self::Wat => "wat",
            Self::Wasm => "wasm",
        }
    }
}

/// Source location of a module.
///
/// This enum provides a structured representation of module paths,
/// replacing raw `Vec<String>` for better type safety and clearer semantics.
///
/// String payloads are [`InternedStr`]; construct values via the
/// well-known zero-arg constructors (e.g. [`ModuleSource::prelude`])
/// or through [`ModuleSourceInterner`].
///
/// # Examples
///
/// ```ignore
/// // Well-known sources need no interner.
/// let prelude = ModuleSource::prelude();           // core:prelude
/// let cli     = ModuleSource::cli();               // core:cli
/// let wasi    = ModuleSource::wasi_cli();          // wasi:cli
///
/// // Arbitrary content goes through the interner so identity is
/// // canonicalised (`Arc::ptr_eq` on the inner string).
/// let mut interner = ModuleSourceInterner::new();
/// let local  = interner.local("./geometry.wado");
/// let wasi_x = interner.wasi("io");
/// ```
///
/// Note: Two `EntryPoint` variants are considered equal regardless of their
/// `filename` field. This ensures that types defined in the entry module
/// are consistent across different compilation phases.
#[derive(Debug, Clone)]
pub enum ModuleSource {
    /// Core library module (e.g., `core:prelude`, `core:cli`, `core:internal`, `core:builtin`)
    Core {
        /// Module name within core (e.g., "prelude", "cli", "internal", "builtin")
        name: InternedStr,
    },
    /// WASI module (e.g., `wasi:cli`, `wasi:io`)
    Wasi {
        /// Interface name (e.g., "cli", "io", "filesystem")
        interface: InternedStr,
    },
    /// Local module relative to project root
    Local {
        /// Relative path (e.g., "./geometry.wado", "./utils/helper.wado")
        path: InternedStr,
    },
    /// Remote module loaded via HTTP/HTTPS
    Remote {
        /// Full URL (e.g., "<https://example.com/lib.wado>")
        url: InternedStr,
    },
    /// Entry point module (the main file being compiled)
    EntryPoint {
        /// Filename of the entry point (e.g., "hello.wado", "<stdin>", "<entry>")
        filename: InternedStr,
    },
    /// Module loaded through a Kiln invocation redirect.
    ///
    /// The contained `uri` is opaque to most of the compiler — it is
    /// passed verbatim to `CompilerHost::load_source`, which decides
    /// how to interpret it. The CLI's `FilesystemCompilerHost` accepts
    /// `file:` URIs and reads the absolute path; in-memory hosts use
    /// the URI as a key. Using a URI keeps `wado-compiler` free of
    /// `std::path` (and therefore `wasm32-unknown-unknown`-friendly).
    ///
    /// Created by [`crate::loader::ModuleLoader::resolve_import`] when
    /// an import target matches an entry in the
    /// [`crate::kiln::InvocationIndex`]; never written by user source.
    Redirected {
        /// Absolute URI (typically `file:///abs/path/to/file.wado`).
        uri: InternedStr,
    },
    /// Wasm asset imported via `use ... from "<path>" with { type: "wat"|"wasm" }`.
    ///
    /// `path` is the canonical identifier for the asset, computed by
    /// [`crate::loader::ModuleLoader::resolve_import`]:
    /// - `core:libm.wat` for stdlib-bundled wat next to a core module.
    /// - `./geometry.wat` (or normalized form) for entry-relative imports.
    /// - The full `wasi:` / `core:` path with `.wat`/`.wasm` extension when
    ///   the importing module is a core/wasi module.
    ///
    /// The asset is loaded as raw bytes and parsed by the wasm-import
    /// loader path; the resulting Wado module exposes one extern fn per
    /// requested export. See `docs/wep-2026-01-10-wasm-import.md`.
    Wasm {
        /// Canonical path identifier (used as the unique module key and
        /// as the namespace component of the synthesized
        /// `#[canonical("wasm:<path>", "<export>")]` attributes).
        path: InternedStr,
        /// `wat` or `wasm` source format.
        kind: WasmAssetKind,
    },
}

impl PartialEq for ModuleSource {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Core { name: a }, Self::Core { name: b }) => a == b,
            (Self::Wasi { interface: a }, Self::Wasi { interface: b }) => a == b,
            (Self::Local { path: a }, Self::Local { path: b }) => a == b,
            (Self::Remote { url: a }, Self::Remote { url: b }) => a == b,
            (Self::Redirected { uri: a }, Self::Redirected { uri: b }) => a == b,
            (
                Self::Wasm {
                    path: a,
                    kind: kind_a,
                },
                Self::Wasm {
                    path: b,
                    kind: kind_b,
                },
            ) => a == b && kind_a == kind_b,
            // Entry points are equal regardless of filename
            (Self::EntryPoint { .. }, Self::EntryPoint { .. }) => true,
            _ => false,
        }
    }
}

impl Eq for ModuleSource {}

impl std::hash::Hash for ModuleSource {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        // Use discriminant to differentiate variants
        std::mem::discriminant(self).hash(state);
        match self {
            Self::Core { name } => name.hash(state),
            Self::Wasi { interface } => interface.hash(state),
            Self::Local { path } => path.hash(state),
            Self::Remote { url } => url.hash(state),
            Self::Redirected { uri } => uri.hash(state),
            Self::Wasm { path, kind } => {
                path.hash(state);
                kind.hash(state);
            }
            // Entry points hash the same regardless of filename
            Self::EntryPoint { .. } => {}
        }
    }
}

impl Default for ModuleSource {
    /// Placeholder value — replaced by the link phase with the real module source.
    fn default() -> Self {
        Self::Core {
            name: InternedStr::from_arc(PLACEHOLDER_NAME.clone()),
        }
    }
}

/// Generate a zero-arg `ModuleSource` constructor that adopts a
/// well-known `LazyLock<Arc<str>>` static. Keeps the constructor body
/// regular so the only per-name input is the variant + field + static.
macro_rules! well_known_module_sources {
    (
        $(
            $(#[$meta:meta])*
            $vis:vis fn $fn_name:ident() = $variant:ident { $field:ident: $arc:ident }
        ),* $(,)?
    ) => {
        $(
            $(#[$meta])*
            #[must_use]
            $vis fn $fn_name() -> Self {
                Self::$variant { $field: InternedStr::from_arc($arc.clone()) }
            }
        )*
    };
}

impl ModuleSource {
    well_known_module_sources! {
        /// `core:prelude` — the prelude module.
        pub fn prelude() = Core { name: CORE_PRELUDE },
        /// `core:prelude/string.wado` — the String type.
        pub fn string() = Core { name: CORE_PRELUDE_STRING },
        /// `core:prelude/list.wado` — the List type.
        pub fn list() = Core { name: CORE_PRELUDE_LIST },
        /// `core:prelude/array.wado` — the raw GC `Array<T>` type.
        pub fn array() = Core { name: CORE_PRELUDE_ARRAY },
        /// `core:prelude/format.wado` — format trait helpers.
        pub fn format() = Core { name: CORE_PRELUDE_FORMAT },
        /// `core:prelude/int128.wado` — 128-bit integer types.
        pub fn int128() = Core { name: CORE_PRELUDE_INT128 },
        /// `core:prelude/primitive.wado` — primitive type methods.
        pub fn primitive() = Core { name: CORE_PRELUDE_PRIMITIVE },
        /// `core:prelude/types.wado` — core type definitions.
        pub fn types() = Core { name: CORE_PRELUDE_TYPES },
        /// `core:prelude/traits.wado` — builtin trait definitions.
        pub fn traits() = Core { name: CORE_PRELUDE_TRAITS },
        /// `core:prelude/range` — range types.
        pub fn range() = Core { name: CORE_PRELUDE_RANGE },
        /// `core:internal` — compiler internal functions.
        pub fn internal() = Core { name: CORE_INTERNAL },
        /// `core:allocator` — linear memory allocator (compiled into "mem" Wasm module).
        pub fn allocator() = Core { name: CORE_ALLOCATOR },
        /// `core:builtin` — builtin wasm instruction mappings.
        pub fn builtin() = Core { name: CORE_BUILTIN },
        /// `core:cli` — CLI output functions.
        pub fn cli() = Core { name: NAME_CLI },
        /// `core:serde` — serde framework.
        pub fn serde() = Core { name: CORE_SERDE },

        /// `wasi:cli` — CLI interface root.
        pub fn wasi_cli() = Wasi { interface: NAME_CLI },
        /// `wasi:clocks` — clocks interface root.
        pub fn wasi_clocks() = Wasi { interface: WASI_CLOCKS },
        /// `wasi:filesystem` — filesystem interface root.
        pub fn wasi_filesystem() = Wasi { interface: WASI_FILESYSTEM },
        /// `wasi:http` — http interface root.
        pub fn wasi_http() = Wasi { interface: WASI_HTTP },

        /// Synthetic `<entry>` placeholder used by `from_path(&[])`.
        pub fn entry_point_synthetic() = EntryPoint { filename: ENTRY_FILENAME_ENTRY },
        /// `<uninitialized>` sentinel for elaborator bootstrap.
        pub fn entry_point_uninitialized() = EntryPoint { filename: ENTRY_FILENAME_UNINITIALIZED },
        /// `<stdin>` placeholder.
        pub fn entry_point_stdin() = EntryPoint { filename: ENTRY_FILENAME_STDIN },
    }

    /// Convert to the legacy `Vec<String>` module path representation.
    ///
    /// This enables gradual migration while maintaining compatibility.
    #[must_use]
    pub fn to_path(&self) -> Vec<String> {
        match self {
            Self::Core { name } => vec!["core".to_string(), name.to_string()],
            Self::Wasi { interface } => vec!["wasi".to_string(), interface.to_string()],
            Self::Local { path } => vec![path.to_string()],
            Self::Remote { url } => vec![url.to_string()],
            Self::EntryPoint { filename } => vec![filename.to_string()],
            Self::Redirected { uri } => vec![uri.to_string()],
            Self::Wasm { path, .. } => vec![path.to_string()],
        }
    }

    /// Check if this is a wasm-asset module (`.wat`/`.wasm` import).
    #[must_use]
    pub fn is_wasm_asset(&self) -> bool {
        matches!(self, Self::Wasm { .. })
    }

    /// Namespace key used by `#[canonical("wasm:<path>", ...)]` attributes
    /// synthesized for this wasm asset. Returns `None` for non-wasm
    /// module sources.
    #[must_use]
    pub fn wasm_canonical_namespace(&self) -> Option<String> {
        match self {
            Self::Wasm { path, .. } => Some(format!("wasm:{path}")),
            _ => None,
        }
    }

    /// Check if this is a core module.
    #[must_use]
    pub fn is_core(&self) -> bool {
        matches!(self, Self::Core { .. })
    }

    /// Check if this is a WASI module.
    #[must_use]
    pub fn is_wasi(&self) -> bool {
        matches!(self, Self::Wasi { .. })
    }

    /// Check if this is a local module.
    #[must_use]
    pub fn is_local(&self) -> bool {
        matches!(self, Self::Local { .. })
    }

    /// Check if this is a remote module.
    #[must_use]
    pub fn is_remote(&self) -> bool {
        matches!(self, Self::Remote { .. })
    }

    /// Check if this is the core/internal module.
    #[must_use]
    pub fn is_core_internal(&self) -> bool {
        matches!(self, Self::Core { name } if name == "internal")
    }

    /// Check if this is the core/builtin module.
    #[must_use]
    pub fn is_core_builtin(&self) -> bool {
        matches!(self, Self::Core { name } if name == "builtin")
    }

    /// Check if this is the core/prelude module.
    #[must_use]
    pub fn is_core_prelude(&self) -> bool {
        matches!(self, Self::Core { name } if name == "prelude")
    }

    /// Check if this is the entry point module.
    #[must_use]
    pub fn is_entry_point(&self) -> bool {
        matches!(self, Self::EntryPoint { .. })
    }

    /// Check if this looks like an effect module (single `PascalCase` name).
    /// Effects are represented as Local paths with a single element like "Stdout".
    #[must_use]
    pub fn is_effect_like(&self) -> bool {
        if self.is_entry_point() {
            return false;
        }
        let path = self.to_path();
        path.len() == 1
            && path[0]
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_uppercase())
            && !path[0].contains('/')
            && !path[0].contains('.')
    }

    /// Get the interface name if this is an interface-like module.
    #[must_use]
    pub fn interface_name(&self) -> Option<String> {
        if self.is_effect_like() {
            let path = self.to_path();
            path.into_iter().next()
        } else {
            None
        }
    }

    /// Convert to a path string format used for method name mangling.
    ///
    /// Returns `self.to_path().join("/")`:
    /// - `EntryPoint { filename }` → `"{filename}"`
    /// - `Local { path }` → `"{path}"`
    /// - `Core { name }` → `"core/{name}"`
    /// - `Wasi { interface }` → `"wasi/{interface}"`
    /// - `Remote { url }` → `"{url}"`
    #[must_use]
    pub fn to_path_string(&self) -> String {
        self.to_path().join("/")
    }

    /// Create a module-qualified name using `//` as separator.
    ///
    /// The `//` separator cannot appear in file paths, making it safe
    /// for disambiguating same-named types from different modules.
    ///
    /// Examples:
    /// - `ModuleSource::prelude().qualify_name("Option")` → `"core:prelude//Option"`
    /// - `ModuleSource::local("./geometry.wado").qualify_name("Point")` → `"./geometry.wado//Point"`
    /// - `interner.entry_point("main.wado").qualify_name("Foo")` → `"main.wado//Foo"`
    #[must_use]
    pub fn qualify_name(&self, name: &str) -> String {
        format!("{self}//{name}")
    }

    /// Return a filename suitable for diagnostic messages.
    ///
    /// Returns an empty string for entry points without real filenames
    /// (e.g., `<stdin>`, `<entry>`) so that `Logger::apply_file_context`
    /// can fill in the correct file from the logger's current file context.
    #[must_use]
    pub fn diagnostic_filename(&self) -> String {
        match self {
            Self::EntryPoint { filename } => {
                if filename.starts_with('<') {
                    String::new() // synthetic names like <stdin>, <entry>
                } else {
                    filename.to_string()
                }
            }
            other => other.to_string(),
        }
    }
}

impl fmt::Display for ModuleSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Core { name } => write!(f, "core:{name}"),
            Self::Wasi { interface } => write!(f, "wasi:{interface}"),
            Self::Local { path } => write!(f, "{path}"),
            Self::Remote { url } => write!(f, "{url}"),
            Self::EntryPoint { filename } => {
                write!(f, "{filename}")
            }
            Self::Redirected { uri } => write!(f, "{uri}"),
            Self::Wasm { path, .. } => write!(f, "{path}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_module_source_from_path_core() {
        let mut interner = ModuleSourceInterner::new();
        let source = interner.from_path(&["core".to_string(), "prelude".to_string()]);
        assert!(matches!(source, ModuleSource::Core { ref name } if name == "prelude"));

        let source = interner.from_path(&["core".to_string(), "cli".to_string()]);
        assert!(matches!(source, ModuleSource::Core { ref name } if name == "cli"));

        let source = interner.from_path(&["core".to_string(), "internal".to_string()]);
        assert!(source.is_core_internal());
    }

    #[test]
    fn test_module_source_from_path_wasi() {
        let mut interner = ModuleSourceInterner::new();
        let source = interner.from_path(&["wasi".to_string(), "cli".to_string()]);
        assert!(matches!(source, ModuleSource::Wasi { ref interface } if interface == "cli"));

        let source = interner.from_path(&["wasi".to_string(), "io".to_string()]);
        assert!(source.is_wasi());
    }

    #[test]
    fn test_module_source_from_path_local() {
        let mut interner = ModuleSourceInterner::new();
        let source = interner.from_path(&["./geometry.wado".to_string()]);
        assert!(matches!(source, ModuleSource::Local { ref path } if path == "./geometry.wado"));

        let source = interner.from_path(&["../lib.wado".to_string()]);
        assert!(source.is_local());
    }

    #[test]
    fn test_module_source_from_path_entry_point() {
        // Legacy: empty path represents entry module
        let mut interner = ModuleSourceInterner::new();
        let source = interner.from_path(&[]);
        assert!(source.is_entry_point());
    }

    #[test]
    fn test_module_source_to_path() {
        let mut interner = ModuleSourceInterner::new();
        let source = ModuleSource::prelude();
        assert_eq!(source.to_path(), vec!["core", "prelude"]);

        let source = interner.wasi("cli");
        assert_eq!(source.to_path(), vec!["wasi", "cli"]);

        let source = interner.local("./geometry.wado");
        assert_eq!(source.to_path(), vec!["./geometry.wado"]);

        let source = interner.entry_point("test.wado");
        assert_eq!(source.to_path(), vec!["test.wado"]);
    }

    #[test]
    fn test_module_source_display() {
        let mut interner = ModuleSourceInterner::new();
        assert_eq!(ModuleSource::prelude().to_string(), "core:prelude");
        assert_eq!(ModuleSource::cli().to_string(), "core:cli");
        assert_eq!(interner.wasi("cli").to_string(), "wasi:cli");
        assert_eq!(
            interner.local("./geometry.wado").to_string(),
            "./geometry.wado"
        );
        assert_eq!(interner.entry_point("hello.wado").to_string(), "hello.wado");
    }

    #[test]
    fn test_module_source_helpers() {
        let mut interner = ModuleSourceInterner::new();
        let core = ModuleSource::internal();
        assert!(core.is_core());
        assert!(core.is_core_internal());
        assert!(!core.is_wasi());
        assert!(!core.is_local());

        let builtin = ModuleSource::builtin();
        assert!(builtin.is_core_builtin());

        let prelude = ModuleSource::prelude();
        assert!(prelude.is_core_prelude());

        let wasi = interner.wasi("cli");
        assert!(wasi.is_wasi());
        assert!(!wasi.is_core());

        let local = interner.local("./file.wado");
        assert!(local.is_local());
        assert!(!local.is_core());
    }

    #[test]
    fn test_module_source_qualify_name() {
        let mut interner = ModuleSourceInterner::new();
        assert_eq!(
            ModuleSource::prelude().qualify_name("Option"),
            "core:prelude//Option"
        );
        assert_eq!(
            interner.local("./geometry.wado").qualify_name("Point"),
            "./geometry.wado//Point"
        );
        assert_eq!(
            interner.wasi("cli").qualify_name("Stdout"),
            "wasi:cli//Stdout"
        );
        assert_eq!(
            interner.entry_point("main.wado").qualify_name("Foo"),
            "main.wado//Foo"
        );
    }

    #[test]
    fn test_module_source_roundtrip() {
        // Test that from_path and to_path are inverses (for supported formats)
        let paths = vec![
            vec!["core".to_string(), "prelude".to_string()],
            vec!["wasi".to_string(), "cli".to_string()],
            vec!["./geometry.wado".to_string()],
        ];

        let mut interner = ModuleSourceInterner::new();
        for path in paths {
            let source = interner.from_path(&path);
            assert_eq!(source.to_path(), path, "Roundtrip failed for {path:?}");
        }
    }

    /// Every well-known zero-arg constructor must produce a value
    /// that compares equal to the matching interner-built one. If a
    /// `LazyLock<Arc<str>>` static contains content that duplicates
    /// another well-known arc, only the first inserted is adopted by
    /// the interner — and the duplicate's pointer drifts out of
    /// canonical identity. This test guards against that regression
    /// (see e.g. `NAME_CLI` shared between `cli()` and `wasi_cli()`).
    #[test]
    fn well_known_constructors_match_interner() {
        let mut i = ModuleSourceInterner::new();
        let cases: Vec<(ModuleSource, ModuleSource)> = vec![
            (ModuleSource::prelude(), i.core("prelude")),
            (ModuleSource::builtin(), i.core("builtin")),
            (ModuleSource::internal(), i.core("internal")),
            (ModuleSource::allocator(), i.core("allocator")),
            (ModuleSource::cli(), i.core("cli")),
            (ModuleSource::string(), i.core("prelude/string.wado")),
            (ModuleSource::list(), i.core("prelude/list.wado")),
            (ModuleSource::format(), i.core("prelude/format.wado")),
            (ModuleSource::int128(), i.core("prelude/int128.wado")),
            (ModuleSource::primitive(), i.core("prelude/primitive.wado")),
            (ModuleSource::types(), i.core("prelude/types.wado")),
            (ModuleSource::traits(), i.core("prelude/traits.wado")),
            (ModuleSource::range(), i.core("prelude/range")),
            (ModuleSource::serde(), i.core("serde")),
            (ModuleSource::wasi_cli(), i.wasi("cli")),
            (ModuleSource::wasi_clocks(), i.wasi("clocks")),
            (ModuleSource::wasi_filesystem(), i.wasi("filesystem")),
            (ModuleSource::wasi_http(), i.wasi("http")),
            (
                ModuleSource::entry_point_synthetic(),
                i.entry_point("<entry>"),
            ),
            (ModuleSource::entry_point_stdin(), i.entry_point("<stdin>")),
            (
                ModuleSource::entry_point_uninitialized(),
                i.entry_point("<uninitialized>"),
            ),
        ];
        for (lhs, rhs) in cases {
            assert_eq!(lhs, rhs, "well-known {lhs} != interner-built {rhs}");
        }
    }
}
