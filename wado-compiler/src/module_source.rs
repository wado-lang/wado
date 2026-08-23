//! Module identity and its interner. `ModuleSource` is the "where did this code
//! come from" half of `symbol-coordinate = (ModuleSource, AstId)`; rendering one
//! into a mangled symbol is `crate::name`'s job. Every string field is
//! canonicalised into an `Arc<str>` through [`ModuleSourceInterner`], so `clone`
//! / `eq` / `hash` are O(1) and well-known names need no interner at all.

use crate::intern::{InternedStr, StringInterner};
use std::fmt;
use std::sync::{Arc, LazyLock};

/// Sentinel `Arc<str>` for `ModuleSource::default()` placeholders.
/// Distinct identity from any real interned core name (no real module
/// has empty content).
static PLACEHOLDER_NAME: LazyLock<Arc<str>> = LazyLock::new(|| Arc::<str>::from(""));

static CORE_PRELUDE: LazyLock<Arc<str>> = LazyLock::new(|| Arc::<str>::from("prelude"));
static CORE_BUILTIN: LazyLock<Arc<str>> = LazyLock::new(|| Arc::<str>::from("builtin"));
static CORE_RT: LazyLock<Arc<str>> = LazyLock::new(|| Arc::<str>::from("rt"));
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
static CORE_PRELUDE_BYTES: LazyLock<Arc<str>> =
    LazyLock::new(|| Arc::<str>::from("prelude/bytes.wado"));
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
        CORE_RT.clone(),
        CORE_ALLOCATOR.clone(),
        NAME_CLI.clone(),
        CORE_PRELUDE_STRING.clone(),
        CORE_PRELUDE_LIST.clone(),
        CORE_PRELUDE_ARRAY.clone(),
        CORE_PRELUDE_BYTES.clone(),
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

/// Canonical `Arc<str>` payloads for every stdlib module, derived from the
/// `crate::stdlib` module lists. Every [`ModuleSourceInterner`] adopts them, so
/// stdlib `ModuleSource` values compare pointer-equal across independently
/// constructed interners — what lets the stdlib TIR cache key by `ModuleSource`.
/// `Core` and `Wasi` store the prefix-stripped path, `Wasm` the full one.
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
    /// Resolved `[dependencies]`, consulted for bare-name `use` clauses, plus
    /// declared-but-unresolved entries (with reasons) for precise errors.
    /// Empty for single-file compilation.
    dependencies: crate::compiler_host::DependencyIndex,
    /// Module path → its elected package root, so `pkg` is a function of the
    /// path and equal modules agree on their package.
    package_roots: crate::hashmap::IndexMap<InternedStr, InternedStr>,
}

impl ModuleSourceInterner {
    pub fn new() -> Self {
        Self {
            strings: StringInterner::with_well_known_arcs(well_known_arcs()),
            dependencies: crate::compiler_host::DependencyIndex::default(),
            package_roots: crate::hashmap::IndexMap::default(),
        }
    }

    pub fn set_dependencies(&mut self, dependencies: crate::compiler_host::DependencyIndex) {
        self.dependencies = dependencies;
    }

    /// A dependency's entry module: it is its own package root.
    pub fn dependency(&mut self, path: &str) -> ModuleSource {
        self.dependency_module(path, path)
    }

    /// A non-entry module of the dependency package rooted at `pkg`, elected
    /// by [`Self::elect_package_root`] so one file reached two ways stays one
    /// module in one package.
    pub fn dependency_module(&mut self, pkg: &str, path: &str) -> ModuleSource {
        let path = self.intern(path);
        let pkg = self.elect_package_root(&path, pkg);
        ModuleSource::Dependency { pkg, path }
    }

    /// The package root for `path`. A module is reachable under more than one
    /// candidate — a sibling package importing it relatively offers its own —
    /// so the one whose tree contains it wins, never whichever came first.
    fn elect_package_root(&mut self, path: &InternedStr, candidate: &str) -> InternedStr {
        let better = match self.package_roots.get(path) {
            None => true,
            Some(current) => {
                shared_dir_len(path, candidate) > shared_dir_len(path, current.as_str())
            }
        };
        if better {
            let interned = self.intern(candidate);
            self.package_roots.insert(path.clone(), interned.clone());
            return interned;
        }
        self.package_roots[path].clone()
    }

    /// Resolve a bare dependency name to its entry module `ModuleSource`, if
    /// declared in `[dependencies]` and successfully resolved.
    pub fn resolve_dependency(&mut self, name: &str) -> Option<ModuleSource> {
        let path = self.dependencies.resolved.get(name)?.clone();
        Some(self.dependency(&path))
    }

    /// Resolve a coordinate to a *prebuilt component* dependency's `ModuleSource`
    /// (a registry dependency fetched as a `.wasm`), imported across the CM
    /// boundary like a `with { type: "wasm" }` asset.
    pub fn resolve_component_dependency(&mut self, name: &str) -> Option<ModuleSource> {
        let path = self.dependencies.components.get(name)?.clone();
        Some(self.wasm(&path, WasmAssetKind::Wasm))
    }

    /// The reason a *declared* dependency could not be resolved, if any.
    #[must_use]
    pub fn unresolved_dependency(&self, name: &str) -> Option<&str> {
        self.dependencies.unresolved.get(name).map(String::as_str)
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
    /// A remote package's entry module: it is its own package root.
    pub fn remote(&mut self, url: &str) -> ModuleSource {
        self.remote_module(url, url)
    }

    /// A non-entry module of the remote package rooted at `pkg`.
    /// See [`Self::dependency_module`].
    pub fn remote_module(&mut self, pkg: &str, url: &str) -> ModuleSource {
        let url = self.intern(url);
        let pkg = self.elect_package_root(&url, pkg);
        ModuleSource::Remote { pkg, url }
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
            [first, rest @ ..] if first == "dep" => self.dependency(&rest.join("/")),
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

/// Structured source location of a module. String payloads are [`InternedStr`]:
/// a well-known source comes from a zero-arg constructor
/// ([`ModuleSource::prelude`]), anything else from [`ModuleSourceInterner`],
/// which canonicalises identity down to `Arc::ptr_eq`. Two `EntryPoint`s compare
/// equal regardless of `filename`.
#[derive(Debug, Clone)]
pub enum ModuleSource {
    /// Core library module (e.g., `core:prelude`, `core:cli`, `core:rt`, `core:builtin`)
    Core {
        /// Module name within core (e.g., "prelude", "cli", "rt", "builtin")
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
    /// A module of a dependency package, resolved from a bare-name
    /// `use { … } from "<dep>"` against `[dependencies]`. Identity is `path`,
    /// so one file reached two ways stays one module. Distinct from
    /// [`ModuleSource::Local`] to carry the package boundary, but loaded alike.
    Dependency {
        /// The package root: the dependency's resolved `[package].lib`. Shared
        /// by every module of the package, and inherited by a relative import.
        pkg: InternedStr,
        path: InternedStr,
    },
    /// Remote module loaded via HTTP/HTTPS
    Remote {
        /// The package root: the URL the package is entered through, a remote
        /// package having no manifest to name one.
        pkg: InternedStr,
        /// Full URL (e.g., "<https://example.com/lib.wado>")
        url: InternedStr,
    },
    /// Entry point module (the main file being compiled)
    EntryPoint {
        /// Filename of the entry point (e.g., `"hello.wado"`, `"<stdin>"`, `"<entry>"`)
        filename: InternedStr,
    },
    /// Module loaded through a Kiln invocation redirect, created when an import
    /// matches the [`crate::kiln::InvocationIndex`] and never written by user
    /// source. The `uri` is opaque to the compiler, passed verbatim to
    /// `CompilerHost::load_source`. A URI rather than a path keeps
    /// `wado-compiler` free of `std::path`, and so `wasm32`-friendly.
    Redirected {
        /// Absolute URI (typically `file:///abs/path/to/file.wado`).
        uri: InternedStr,
    },
    /// Wasm asset imported via
    /// `use … from "<path>" with { type: "wat"|"wasm" }`. `path` is the canonical
    /// identifier `resolve_import` computed — `core:` / `wasi:`-prefixed for a
    /// stdlib importer, else relative to the importing module. Loaded as raw
    /// bytes; the resulting
    /// Wado module exposes one extern fn per requested export.
    Wasm {
        /// Canonical path identifier (used as the unique module key and
        /// as the namespace component of the synthesized
        /// `#[canonical("wasm:<path>", "<export>")]` attributes).
        path: InternedStr,
        /// `wat` or `wasm` source format.
        kind: WasmAssetKind,
    },
}

/// The package a [`ModuleSource`] belongs to. Visibility enforcement keys on
/// this: `internal` items reach any module with the same `PackageId`; `pub`
/// items reach across package boundaries. `core` and `wasi` are each their own
/// independent package; the entry point and its local modules form the `Root`
/// package being compiled.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum PackageId {
    Core,
    Wasi,
    /// Entry point, its local modules, Kiln redirects, and wasm assets bundled
    /// into the same component.
    Root,
    Dependency(InternedStr),
    Remote(InternedStr),
}

impl ModuleSource {
    #[must_use]
    pub fn package_id(&self) -> PackageId {
        match self {
            Self::Core { .. } => PackageId::Core,
            Self::Wasi { .. } => PackageId::Wasi,
            Self::Local { .. }
            | Self::EntryPoint { .. }
            | Self::Redirected { .. }
            | Self::Wasm { .. } => PackageId::Root,
            Self::Dependency { pkg, .. } => PackageId::Dependency(pkg.clone()),
            Self::Remote { pkg, .. } => PackageId::Remote(pkg.clone()),
        }
    }

    /// The reach test for `internal` visibility.
    #[must_use]
    pub fn same_package(&self, other: &Self) -> bool {
        self.package_id() == other.package_id()
    }
}

impl PartialEq for ModuleSource {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Core { name: a }, Self::Core { name: b }) => a == b,
            (Self::Wasi { interface: a }, Self::Wasi { interface: b }) => a == b,
            (Self::Local { path: a }, Self::Local { path: b }) => a == b,
            (Self::Dependency { path: a, .. }, Self::Dependency { path: b, .. }) => a == b,
            (Self::Remote { url: a, .. }, Self::Remote { url: b, .. }) => a == b,
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
            Self::Dependency { path, .. } => path.hash(state),
            Self::Remote { url, .. } => url.hash(state),
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
        /// `core:prelude/bytes.wado` — the `Byte*` newtypes (`ByteList`, …).
        pub fn bytes() = Core { name: CORE_PRELUDE_BYTES },
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
        /// `core:rt` — runtime support helpers (panic, assert, CM ABI glue).
        pub fn rt() = Core { name: CORE_RT },
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
    /// This is the module's **portable qualifier** — the identity used to
    /// namespace its symbols. An entry point contributes its base name (its
    /// path relative to its own directory), not the raw compile path, so
    /// symbol names stay stable across invocations and machines. The real
    /// on-disk path lives on [`ModuleSource::source_path`].
    #[must_use]
    pub fn to_path(&self) -> Vec<String> {
        match self {
            Self::Core { name } => vec!["core".to_string(), name.to_string()],
            Self::Wasi { interface } => vec!["wasi".to_string(), interface.to_string()],
            Self::Local { path } => vec![path.to_string()],
            Self::Dependency { path, .. } => vec!["dep".to_string(), path.to_string()],
            Self::Remote { url, .. } => vec![url.to_string()],
            Self::EntryPoint { filename } => vec![entry_basename(filename).to_string()],
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

    /// Whether the entry package owns this module: the entry point and the
    /// local modules it reaches. A dependency, `core:` / `wasi:`, a remote and
    /// a Kiln-generated module are all someone else's source.
    #[must_use]
    pub fn is_entry_package(&self) -> bool {
        matches!(self, Self::EntryPoint { .. } | Self::Local { .. })
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

    /// Check if this is the core/rt module.
    #[must_use]
    pub fn is_core_rt(&self) -> bool {
        matches!(self, Self::Core { name } if name == "rt")
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
    /// - `EntryPoint { filename }` → `"{basename}"`
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

    /// The module's real source location — the counterpart to the portable
    /// qualifier ([`Self::to_path`]), keeping the entry point's full compile path
    /// rather than its base name. Read where a real path must resolve a sibling
    /// file (`#include_str`) and as the filename in diagnostics. Empty for an
    /// entry point with no real filename, such as `<stdin>`.
    #[must_use]
    pub fn source_path(&self) -> String {
        match self {
            Self::EntryPoint { filename } => {
                if filename.starts_with('<') {
                    String::new() // synthetic names like <stdin>, <entry>
                } else {
                    filename.to_string()
                }
            }
            // The real, loadable filename — not the `dep:`-prefixed `Display`
            // (that is symbol identity). A `#include_str` in a dependency module
            // resolves against this, so it must be the same base-relative path
            // the module itself was loaded from, or the include would resolve
            // against the consumer's directory instead of the dependency's.
            Self::Dependency { path, .. } => path.to_string(),
            other => other.to_string(),
        }
    }
}

/// How many leading directory components `path` and `root` share. Comparing
/// whole components keeps `deps/xpkg2/` from counting as a prefix of
/// `deps/xpkg/`.
fn shared_dir_len(path: &str, root: &str) -> usize {
    let dirs = |s: &str| -> Vec<String> {
        let mut parts: Vec<String> = s.split('/').map(str::to_string).collect();
        parts.pop();
        parts
    };
    dirs(path)
        .iter()
        .zip(dirs(root).iter())
        .take_while(|(a, b)| a == b)
        .count()
}

/// The portable base name of an entry-point filename: its path relative to
/// its own directory. Shared by [`ModuleSource::to_path`] and [`Display`] so a
/// module's symbol qualifier is identical however it is rendered.
fn entry_basename(filename: &str) -> &str {
    filename.rsplit(['/', '\\']).next().unwrap_or(filename)
}

impl fmt::Display for ModuleSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Core { name } => write!(f, "core:{name}"),
            Self::Wasi { interface } => write!(f, "wasi:{interface}"),
            Self::Local { path } => write!(f, "{path}"),
            Self::Dependency { path, .. } => write!(f, "dep:{path}"),
            Self::Remote { url, .. } => write!(f, "{url}"),
            // Symbol identity, not a file path: the entry's qualifier is its
            // base name (its path relative to its own directory), so WIR names
            // stay stable across invocations and machines — the compile path is
            // absolute under the test harness, relative on the CLI. The real
            // path for diagnostics comes from `source_path`.
            Self::EntryPoint { filename } => write!(f, "{}", entry_basename(filename)),
            Self::Redirected { uri } => write!(f, "{uri}"),
            Self::Wasm { path, .. } => write!(f, "{path}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dependency_identity_is_the_resolved_path() {
        let mut interner = ModuleSourceInterner::new();
        let a = interner.dependency("../greet/src/lib.wado");
        let b = interner.dependency("../greet/src/lib.wado");
        let other = interner.dependency("../other/src/lib.wado");
        // Same resolved path = same package, regardless of the alias used to
        // reach it, so two aliases for one package unify.
        assert_eq!(a, b);
        assert_ne!(a, other);
        // Still distinct from a `Local` at the same path: the variant carries
        // the package boundary.
        let local = interner.local("../greet/src/lib.wado");
        assert_ne!(a, local);
        assert_eq!(a.qualify_name("hello"), "dep:../greet/src/lib.wado//hello");
    }

    #[test]
    fn resolve_dependency_uses_registered_path() {
        let mut interner = ModuleSourceInterner::new();
        let mut index = crate::compiler_host::DependencyIndex::default();
        index
            .resolved
            .insert("greet".to_string(), "../greet/src/lib.wado".to_string());
        index.unresolved.insert(
            "broken".to_string(),
            "declares no [package].lib".to_string(),
        );
        interner.set_dependencies(index);
        assert_eq!(
            interner.resolve_dependency("greet"),
            Some(interner.dependency("../greet/src/lib.wado"))
        );
        assert_eq!(interner.resolve_dependency("missing"), None);
        // Declared-but-unresolved entries surface their reason instead.
        assert_eq!(interner.resolve_dependency("broken"), None);
        assert_eq!(
            interner.unresolved_dependency("broken"),
            Some("declares no [package].lib")
        );
    }

    #[test]
    fn test_module_source_from_path_core() {
        let mut interner = ModuleSourceInterner::new();
        let source = interner.from_path(&["core".to_string(), "prelude".to_string()]);
        assert!(matches!(source, ModuleSource::Core { ref name } if name == "prelude"));

        let source = interner.from_path(&["core".to_string(), "cli".to_string()]);
        assert!(matches!(source, ModuleSource::Core { ref name } if name == "cli"));

        let source = interner.from_path(&["core".to_string(), "rt".to_string()]);
        assert!(source.is_core_rt());
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

        // An entry in a sub-directory contributes only its portable base name,
        // not the raw compile path; the full path lives on `source_path`.
        let source = interner.entry_point("/abs/pkg/src/main.wado");
        assert_eq!(source.to_path(), vec!["main.wado"]);
        assert_eq!(source.to_string(), "main.wado");
        assert_eq!(source.source_path(), "/abs/pkg/src/main.wado");
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
    fn dependency_source_path_is_the_loadable_path_not_the_dep_display() {
        // `Display` is symbol identity (`dep:`-prefixed); `source_path` is the
        // real, loadable filename a `#include_str` in the dependency resolves
        // against. Keeping the base-relative `../` (not `dep:../…`) is what makes
        // the include land in the dependency's directory, not the consumer's.
        let mut interner = ModuleSourceInterner::new();
        let source = interner.dependency("../dep/src/highlight/facade.wado");
        assert_eq!(source.to_string(), "dep:../dep/src/highlight/facade.wado");
        assert_eq!(source.source_path(), "../dep/src/highlight/facade.wado");
    }

    #[test]
    fn test_module_source_helpers() {
        let mut interner = ModuleSourceInterner::new();
        let core = ModuleSource::rt();
        assert!(core.is_core());
        assert!(core.is_core_rt());
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
    fn test_package_id_and_same_package() {
        let mut interner = ModuleSourceInterner::new();

        let rt = ModuleSource::rt();
        let cli = ModuleSource::cli();
        assert_eq!(rt.package_id(), PackageId::Core);
        assert!(rt.same_package(&cli), "all core:* share the core package");

        let wasi = interner.wasi("cli");
        assert_eq!(wasi.package_id(), PackageId::Wasi);
        assert!(
            !rt.same_package(&wasi),
            "core and wasi are separate packages"
        );

        let entry = ModuleSource::entry_point_uninitialized();
        let local_a = interner.local("./a.wado");
        let local_b = interner.local("./b.wado");
        assert_eq!(local_a.package_id(), PackageId::Root);
        assert!(local_a.same_package(&local_b));
        assert!(local_a.same_package(&entry));
        assert!(!local_a.same_package(&rt), "root and core are separate");

        let dep_a = interner.dependency("dep_a/lib.wado");
        let dep_a2 = interner.dependency("dep_a/lib.wado");
        let dep_b = interner.dependency("dep_b/lib.wado");
        assert!(dep_a.same_package(&dep_a2));
        assert!(!dep_a.same_package(&dep_b));
        assert!(!dep_a.same_package(&local_a));
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
            (ModuleSource::rt(), i.core("rt")),
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
