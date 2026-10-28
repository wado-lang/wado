//! Module identity and its interner. `ModuleSource` is the "where did this code
//! come from" half of `symbol-coordinate = (ModuleSource, AstId)`; rendering one
//! into a mangled symbol is `crate::name`'s job. Every string field is
//! canonicalised into an `Arc<str>` through [`ModuleSourceInterner`], so `clone`
//! / `eq` / `hash` are O(1) and well-known names need no interner at all.

use crate::ast::{AstIdSpace, Module};
use crate::compiler_host::{CompilerHost, DependencyIndex, DependencyManifest};
use crate::hashmap;
use crate::intern::{InternedStr, StringInterner};
use crate::name::{DeclaredHead, effect_namespace, module_parent_dir};
use crate::path::is_cwd_relative;
use crate::primitive::PrimitiveType;
use crate::stdlib::{ALL_CORE_WASM_ASSETS, BINDING_MODULE_PATHS, CORE_MODULE_PATHS};
use crate::token::Span;
use std::fmt;
use std::sync::{Arc, LazyLock};
use unicode_normalization::UnicodeNormalization;

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
static CORE_PRELUDE_HALF: LazyLock<Arc<str>> =
    LazyLock::new(|| Arc::<str>::from("prelude/half.wado"));
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
        CORE_PRELUDE_HALF.clone(),
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
/// `Core` and `Binding` store the prefix-stripped path, `Wasm` the full one.
static STDLIB_NAME_ARCS: LazyLock<Vec<Arc<str>>> = LazyLock::new(|| {
    let mut arcs: Vec<Arc<str>> = Vec::new();
    for path in CORE_MODULE_PATHS {
        let name = path.strip_prefix("core:").unwrap_or(path);
        arcs.push(Arc::<str>::from(name));
    }
    for path in BINDING_MODULE_PATHS {
        let (_, interface) = CmNamespace::split_specifier(path)
            .unwrap_or_else(|| panic!("bundled binding module `{path}` has no reserved namespace"));
        arcs.push(Arc::<str>::from(interface));
    }
    for (path, _bytes) in ALL_CORE_WASM_ASSETS {
        // The loader interns the full canonical path for `Wasm`
        // variants (no prefix stripping); see `ModuleLoader::handle_wasm_import`.
        arcs.push(Arc::<str>::from(*path));
    }
    arcs
});

/// What placing a module asks the host: the [`CompilerHost`] methods of the
/// same names.
pub trait PackageHost {
    fn package_root(&self, path: &str) -> Option<String>;
    fn package_index(&self, root: &str) -> DependencyIndex;
}

impl<H: CompilerHost> PackageHost for H {
    fn package_root(&self, path: &str) -> Option<String> {
        CompilerHost::package_root(self, path)
    }

    fn package_index(&self, root: &str) -> DependencyIndex {
        CompilerHost::package_index(self, root)
    }
}

/// A [`PackageHost`] answering roots from `root` and indexes from `indexes`,
/// empty for a root it does not list.
#[cfg(test)]
pub(crate) struct TestPackages<F> {
    pub root: F,
    pub indexes: hashmap::IndexMap<String, DependencyIndex>,
}

#[cfg(test)]
impl<F: Fn(&str) -> Option<String>> PackageHost for TestPackages<F> {
    fn package_root(&self, path: &str) -> Option<String> {
        (self.root)(path)
    }

    fn package_index(&self, root: &str) -> DependencyIndex {
        self.indexes.get(root).cloned().unwrap_or_default()
    }
}

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
    dependencies: DependencyIndex,
    /// Module path → its elected package root, so `pkg` is a function of the
    /// path and equal modules agree on their package.
    package_roots: hashmap::IndexMap<InternedStr, InternedStr>,
    /// Generated module URI → the package importing it, one for all its importers.
    redirect_packages: hashmap::IndexMap<InternedStr, PackageId>,
    /// The package of the entry point and the local modules inside its root.
    root_package: PackageId,
    /// The entry's package root, as the host answers it. `None` where the entry
    /// is in no package, and so shares its scope with every module in none.
    root_dir: Option<String>,
    /// Each path dependency's package root → its entry module.
    dependency_roots: hashmap::IndexMap<String, String>,
    /// The root of each path dependency that a path reached before any
    /// manifest declared it, by lib: the package keeps the identity its first
    /// modules were given.
    reached_first: hashmap::IndexMap<String, InternedStr>,
    /// Each local module the host has placed, and its package. A module in a
    /// path dependency's root is placed in that dependency.
    local_packages: hashmap::IndexMap<InternedStr, PackageId>,
    /// Each package a placed module lies in that no manifest declares, by its
    /// root, and the index its own `wado.toml` declares.
    undeclared_packages: hashmap::IndexMap<InternedStr, DependencyIndex>,
    /// What a module in no package declares: nothing.
    no_dependencies: DependencyIndex,
    /// Each directory a placed module lies in, and the package root the host
    /// answered for it.
    dir_roots: hashmap::IndexMap<String, Option<String>>,
}

impl ModuleSourceInterner {
    pub fn new() -> Self {
        Self {
            strings: StringInterner::with_well_known_arcs(well_known_arcs()),
            dependencies: DependencyIndex::default(),
            package_roots: hashmap::IndexMap::default(),
            redirect_packages: hashmap::IndexMap::default(),
            root_package: PackageId::Root,
            root_dir: None,
            dependency_roots: hashmap::IndexMap::default(),
            reached_first: hashmap::IndexMap::default(),
            local_packages: hashmap::IndexMap::default(),
            undeclared_packages: hashmap::IndexMap::default(),
            no_dependencies: DependencyIndex::default(),
            dir_roots: hashmap::IndexMap::default(),
        }
    }

    pub fn set_dependencies(&mut self, dependencies: DependencyIndex) {
        self.dependencies = dependencies;
    }

    /// Put the entry point and its local modules in `core:*`'s package, as a
    /// stdlib test is.
    pub fn join_core_package(&mut self) {
        self.root_package = PackageId::Core;
    }

    /// The entry module of each path dependency, at any depth.
    pub fn dependency_libs(&self) -> impl Iterator<Item = &str> {
        let direct = self.dependencies.resolved.values();
        let transitive = self.dependencies.packages.keys();
        direct.chain(transitive).map(String::as_str)
    }

    /// Bound the package of the entry at `entry` and of each path dependency at
    /// the root `package_root` answers for it. A single-file dependency has no
    /// manifest, so it claims no root: the tree around it is not its package.
    pub fn bound_packages(&mut self, entry: &str, host: &impl PackageHost) {
        let libs: Vec<String> = self.dependency_libs().map(str::to_string).collect();
        self.bound_dependency_roots(libs, host);
        self.root_dir = host.package_root(entry);
    }

    /// Claim the root of each package among `libs` that has a manifest.
    fn bound_dependency_roots(&mut self, libs: Vec<String>, host: &impl PackageHost) {
        for lib in libs {
            if self.package(&lib).manifest.is_some()
                && let Some(root) = host.package_root(&lib)
            {
                let interned = self.intern(&root);
                if self.undeclared_packages.contains_key(&interned) {
                    self.reached_first.insert(lib.clone(), interned);
                }
                self.dependency_roots.insert(root, lib);
            }
        }
    }

    /// The local module at `path`, placed in its package first if it waits on
    /// one. The host is asked for the root of a directory no placed module lies
    /// in, so once per directory, and for the index of a package no manifest
    /// declares, once per package.
    pub fn place(&mut self, path: &str, host: &impl PackageHost) -> ModuleSource {
        let interned = self.intern(path);
        if !self.local_packages.contains_key(&interned) {
            let root = match self.dir_roots.get(module_parent_dir(path)) {
                Some(root) => root.clone(),
                None => host.package_root(path),
            };
            self.place_local(path, root, host);
        }
        self.local(path)
    }

    /// Place the local module at `path` in the package rooted at
    /// `package_root`, or with the modules in no package where it is `None`.
    fn place_local(&mut self, path: &str, package_root: Option<String>, host: &impl PackageHost) {
        self.dir_roots
            .insert(module_parent_dir(path).to_string(), package_root.clone());
        let package = if package_root == self.root_dir {
            self.root_package.clone()
        } else if let Some(root) = package_root {
            // An undeclared package can claim a root for a package it depends
            // on after modules of that root were placed: those keep it Path.
            let interned = self.intern(&root);
            let undeclared = self.undeclared_packages.contains_key(&interned);
            match self.dependency_roots.get(&root).cloned() {
                Some(lib) if !undeclared => PackageId::Dependency(self.intern(&lib)),
                _ => PackageId::Path(self.undeclared_package(&root, host)),
            }
        } else {
            PackageId::Unpackaged
        };
        let path = self.intern(path);
        self.local_packages.insert(path, package);
    }

    /// The package rooted at `root` that no manifest declares, indexed on
    /// first sight along with every package it reaches.
    fn undeclared_package(&mut self, root: &str, host: &impl PackageHost) -> InternedStr {
        let root = self.intern(root);
        if !self.undeclared_packages.contains_key(&root) {
            let mut index = host.package_index(&root);
            let mut reached = Vec::new();
            for (lib, package) in std::mem::take(&mut index.packages) {
                if !self.dependencies.packages.contains_key(&lib) {
                    self.dependencies.packages.insert(lib.clone(), package);
                    reached.push(lib);
                }
            }
            self.undeclared_packages.insert(root.clone(), index);
            self.bound_dependency_roots(reached, host);
        }
        root
    }

    /// A dependency's entry module: it is its own package root.
    pub fn dependency(&mut self, path: &str) -> ModuleSource {
        self.dependency_module(path, path)
    }

    /// A non-entry module of the dependency package rooted at `pkg`, elected
    /// by [`Self::elect_package_root`] so one file reached two ways stays one
    /// module in one package.
    ///
    /// A package a path reached first stays the [`PackageId::Path`] its first
    /// modules were placed in, so each module of it is a placed local one.
    pub fn dependency_module(&mut self, pkg: &str, path: &str) -> ModuleSource {
        let path = self.intern(path);
        if let Some(root) = self.reached_first.get(pkg) {
            let package = PackageId::Path(root.clone());
            self.local_packages.insert(path.clone(), package.clone());
            return ModuleSource::Local { path, package };
        }
        let pkg = self.elect_package_root(&path, pkg);
        ModuleSource::Dependency { pkg, path }
    }

    /// Whether the local module at `path` has been placed in its package.
    #[must_use]
    pub fn is_placed(&self, path: &InternedStr) -> bool {
        self.local_packages.contains_key(path)
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

    /// The dependencies a bare-name import in `importer` resolves against: its
    /// own package's, and none where it lies in no package but the entry does.
    fn dependencies_of(&self, importer: &ModuleSource) -> &DependencyIndex {
        match importer.package_id() {
            PackageId::Dependency(lib) => self.package(lib.as_str()),
            PackageId::Path(root) => &self.undeclared_packages[&root],
            PackageId::Unpackaged => &self.no_dependencies,
            PackageId::Root | PackageId::Core | PackageId::Binding(_) | PackageId::Remote(_) => {
                &self.dependencies
            }
        }
    }

    /// The index of the source dependency whose entry module is `lib`.
    fn package(&self, lib: &str) -> &DependencyIndex {
        self.dependencies
            .packages
            .get(lib)
            .expect("a host indexes the package of every source dependency it resolves")
    }

    /// Resolve a bare dependency name imported by `importer` to its entry
    /// module `ModuleSource`, if its package declares it in `[dependencies]`
    /// and it resolved.
    pub fn resolve_dependency(
        &mut self,
        importer: &ModuleSource,
        name: &str,
    ) -> Option<ModuleSource> {
        let path = self.dependencies_of(importer).resolved.get(name)?.clone();
        Some(self.dependency(&path))
    }

    /// Resolve the file `file` of the source dependency `name`, imported by
    /// `importer` as `"<name>/<file>"`, where the dependency's
    /// `[package].exports` lists it. Compared after NFC, case exactly. `None`
    /// where `importer`'s package declares no dependency `name`.
    pub fn resolve_exported_file(
        &mut self,
        importer: &ModuleSource,
        name: &str,
        file: &str,
    ) -> Option<Result<ModuleSource, ExportRefusal>> {
        let dependencies = self.dependencies_of(importer);
        let Some(lib) = dependencies.resolved.get(name).cloned() else {
            return dependencies
                .components
                .contains_key(name)
                .then_some(Err(ExportRefusal::Component));
        };
        let exports = &self.package(&lib).exports;
        let file: String = file.nfc().collect();
        let Some(path) = exports.get(&file).cloned() else {
            let lowered = file.to_lowercase();
            let spelled_alike = exports.keys().find(|e| e.to_lowercase() == lowered);
            return Some(Err(ExportRefusal::NotExported {
                spelled_alike: spelled_alike.cloned(),
            }));
        };
        Some(Ok(self.dependency_module(&lib, &path)))
    }

    /// Resolve a coordinate imported by `importer` to a *prebuilt component*
    /// dependency's `ModuleSource` (a registry dependency fetched as a
    /// `.wasm`), imported across the CM boundary like a `with { type: "wasm" }`
    /// asset.
    pub fn resolve_component_dependency(
        &mut self,
        importer: &ModuleSource,
        name: &str,
    ) -> Option<ModuleSource> {
        let path = self.dependencies_of(importer).components.get(name)?.clone();
        Some(self.wasm(&path, WasmAssetKind::Wasm))
    }

    /// The reason a dependency `importer`'s package *declares* could not be
    /// resolved, if any.
    #[must_use]
    pub fn unresolved_dependency(&self, importer: &ModuleSource, name: &str) -> Option<&str> {
        let unresolved = &self.dependencies_of(importer).unresolved;
        unresolved.get(name).map(String::as_str)
    }

    /// The `wado.toml` `importer`'s dependencies were read from, if one governs
    /// its package.
    #[must_use]
    pub fn dependency_manifest(&self, importer: &ModuleSource) -> Option<&DependencyManifest> {
        self.dependencies_of(importer).manifest.as_ref()
    }

    pub fn intern(&mut self, s: &str) -> InternedStr {
        self.strings.intern(s)
    }

    pub fn core(&mut self, name: &str) -> ModuleSource {
        ModuleSource::Core {
            name: self.intern(name),
        }
    }
    pub fn binding(&mut self, namespace: CmNamespace, interface: &str) -> ModuleSource {
        ModuleSource::Binding {
            namespace,
            interface: self.intern(interface),
        }
    }
    pub fn wasi(&mut self, interface: &str) -> ModuleSource {
        self.binding(CmNamespace::Wasi, interface)
    }
    pub fn local(&mut self, path: &str) -> ModuleSource {
        let path = self.intern(path);
        let package = match self.local_packages.get(&path) {
            Some(PackageId::Dependency(lib)) => {
                let lib = lib.clone();
                return self.dependency_module(&lib, &path);
            }
            Some(package) => package.clone(),
            None => self.root_package.clone(),
        };
        ModuleSource::Local { path, package }
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
    /// A Kiln-generated module, in the package of the first module importing it.
    pub fn redirected(&mut self, uri: &str, importer: &ModuleSource) -> ModuleSource {
        let uri = self.intern(uri);
        let package = self
            .redirect_packages
            .entry(uri.clone())
            .or_insert_with(|| importer.package_id())
            .clone();
        ModuleSource::Redirected { uri, package }
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
            package: self.root_package.clone(),
        }
    }

    /// Convert from the legacy `&[String]` module path representation.
    pub fn from_path(&mut self, segments: &[String]) -> ModuleSource {
        match segments {
            // Legacy: empty path represents entry module.
            [] => ModuleSource::entry_point_synthetic(),
            [first] if is_cwd_relative(first) => self.local(first),
            [first, rest @ ..] if first == "core" => self.core(&rest.join("/")),
            [first, rest @ ..] if first == "dep" => self.dependency(&rest.join("/")),
            all @ [first, rest @ ..] => match CmNamespace::from_prefix(first) {
                Some(namespace) => self.binding(namespace, &rest.join("/")),
                None => self.local(&all.join("/")),
            },
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

/// Why a `"<package>/<file>"` reference names no module.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExportRefusal {
    /// The package is a prebuilt component, which carries no files.
    Component,
    /// The package's `[package].exports` lists no such file, whether or not it
    /// is there. `spelled_alike` is an entry differing from it in case alone.
    NotExported { spelled_alike: Option<String> },
}

/// A CM package namespace the compiler ships bindings for. Reserved because it
/// is bundled: `wep-2026-06-17-package-module-syntax.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CmNamespace {
    /// `wasi:*` — the WASI P3 bindings under `lib/wasi/`.
    Wasi,
}

impl CmNamespace {
    /// The specifier prefix, without the colon.
    #[must_use]
    pub fn prefix(self) -> &'static str {
        match self {
            Self::Wasi => "wasi",
        }
    }

    /// The namespace a specifier prefix names, or `None` for an open one.
    #[must_use]
    pub fn from_prefix(prefix: &str) -> Option<Self> {
        match prefix {
            "wasi" => Some(Self::Wasi),
            _ => None,
        }
    }

    /// Split `"wasi:cli/stdout.wado"` into its namespace and interface path.
    #[must_use]
    pub fn split_specifier(specifier: &str) -> Option<(Self, &str)> {
        let (prefix, rest) = specifier.split_once(':')?;
        Some((Self::from_prefix(prefix)?, rest))
    }

    /// The namespace and package a CM source names: `"wasi:filesystem/types@0.3.0"`
    /// → `(Wasi, "filesystem")`. A package is unique only inside its namespace,
    /// so the two answer together. `None` outside the bundled namespaces.
    #[must_use]
    pub fn split_package(source: &str) -> Option<(Self, &str)> {
        let (namespace, rest) = Self::split_specifier(source)?;
        let package_end = rest.find(['/', '@']).unwrap_or(rest.len());
        Some((namespace, &rest[..package_end]))
    }
}

impl fmt::Display for CmNamespace {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.prefix())
    }
}

/// `true` for a specifier in a namespace the compiler bundles: `core:` or any
/// [`CmNamespace`]. Such a specifier resolves out of the embedded stdlib, never
/// off the host filesystem.
#[must_use]
pub fn is_bundled_specifier(specifier: &str) -> bool {
    specifier.starts_with("core:") || CmNamespace::split_specifier(specifier).is_some()
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
    /// A bundled CM binding module (e.g., `wasi:cli`). Bodiless
    /// declarations that lower to component imports, unlike [`Self::Core`].
    Binding {
        /// The reserved namespace the module belongs to.
        namespace: CmNamespace,
        /// Interface path within the namespace (e.g., "cli", "cli/stdout.wado")
        interface: InternedStr,
    },
    /// Local module relative to project root
    Local {
        /// Relative path (e.g., "./geometry.wado", "./utils/helper.wado")
        path: InternedStr,
        /// The entry's package, unless the module lies outside the entry's
        /// package root. Not part of identity, which is `path`.
        package: PackageId,
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
        /// [`PackageId::Root`], or [`PackageId::Core`] for a stdlib test. Not
        /// part of identity.
        package: PackageId,
    },
    /// Module loaded through a Kiln invocation redirect, created when an import
    /// matches the [`crate::kiln::InvocationIndex`] and never written by user
    /// source. The `uri` is opaque to the compiler, passed verbatim to
    /// `CompilerHost::load_source`. A URI rather than a path keeps
    /// `wado-compiler` free of `std::path`, and so `wasm32`-friendly.
    Redirected {
        /// Absolute URI (typically `file:///abs/path/to/file.wado`).
        uri: InternedStr,
        /// The importing module's package. Not part of identity, which is `uri`.
        package: PackageId,
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
    /// The namespace an `E::op()` call is rendered in: the interface (or
    /// resource) it names, which has no module of its own.
    Effect(Box<DeclaredHead>),
}

/// The package a [`ModuleSource`] belongs to. Visibility enforcement keys on
/// this: `internal` items reach any module with the same `PackageId`; `pub`
/// items reach across package boundaries. `core` and each bundled CM namespace
/// are their own independent package; the entry point and the local modules
/// inside its package root form the `Root` package being compiled, unless the
/// host serves the stdlib's `core` tree ([`CompilerHost::serves_core`]), where
/// they join `Core`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum PackageId {
    Core,
    Binding(CmNamespace),
    /// Entry point, the local modules inside its package root, and wasm assets
    /// bundled into the same component.
    Root,
    Dependency(InternedStr),
    Remote(InternedStr),
    /// A local module's package other than the entry's, named by the root
    /// [`CompilerHost::package_root`] answers for it.
    Path(InternedStr),
    /// The local modules in no package, where the entry is in one. They share
    /// one scope, as they do with an entry in no package, which is `Root`.
    Unpackaged,
}

impl ModuleSource {
    #[must_use]
    pub fn package_id(&self) -> PackageId {
        match self {
            Self::Core { .. } => PackageId::Core,
            Self::Binding { namespace, .. } => PackageId::Binding(*namespace),
            Self::Local { package, .. } | Self::EntryPoint { package, .. } => package.clone(),
            Self::Wasm { .. } => PackageId::Root,
            Self::Redirected { package, .. } => package.clone(),
            Self::Dependency { pkg, .. } => PackageId::Dependency(pkg.clone()),
            Self::Remote { pkg, .. } => PackageId::Remote(pkg.clone()),
            Self::Effect(head) => head.module().package_id(),
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
            (
                Self::Binding {
                    namespace: ns_a,
                    interface: a,
                },
                Self::Binding {
                    namespace: ns_b,
                    interface: b,
                },
            ) => ns_a == ns_b && a == b,
            (Self::Local { path: a, .. }, Self::Local { path: b, .. }) => a == b,
            (Self::Dependency { path: a, .. }, Self::Dependency { path: b, .. }) => a == b,
            (Self::Remote { url: a, .. }, Self::Remote { url: b, .. }) => a == b,
            (Self::Redirected { uri: a, .. }, Self::Redirected { uri: b, .. }) => a == b,
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
            (Self::Effect(a), Self::Effect(b)) => a == b,
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
            Self::Binding {
                namespace,
                interface,
            } => {
                namespace.hash(state);
                interface.hash(state);
            }
            Self::Local { path, .. } => path.hash(state),
            Self::Dependency { path, .. } => path.hash(state),
            Self::Remote { url, .. } => url.hash(state),
            Self::Redirected { uri, .. } => uri.hash(state),
            Self::Wasm { path, kind } => {
                path.hash(state);
                kind.hash(state);
            }
            Self::Effect(head) => head.hash(state),
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
/// regular so the only per-name input is the variant + field + static,
/// plus whatever plain fields the variant carries alongside.
macro_rules! well_known_module_sources {
    (
        $(
            $(#[$meta:meta])*
            $vis:vis fn $fn_name:ident() = $variant:ident {
                $field:ident: $arc:ident $(, $fixed:ident: $value:expr)*
            }
        ),* $(,)?
    ) => {
        $(
            $(#[$meta])*
            #[must_use]
            $vis fn $fn_name() -> Self {
                Self::$variant {
                    $field: InternedStr::from_arc($arc.clone()),
                    $($fixed: $value,)*
                }
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
        /// `core:prelude/half.wado` — the `f16` and `bf16` methods.
        pub fn half() = Core { name: CORE_PRELUDE_HALF },
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
        pub fn wasi_cli() = Binding { interface: NAME_CLI, namespace: CmNamespace::Wasi },
        /// `wasi:clocks` — clocks interface root.
        pub fn wasi_clocks() = Binding { interface: WASI_CLOCKS, namespace: CmNamespace::Wasi },
        /// `wasi:filesystem` — filesystem interface root.
        pub fn wasi_filesystem() = Binding {
            interface: WASI_FILESYSTEM, namespace: CmNamespace::Wasi
        },
        /// `wasi:http` — http interface root.
        pub fn wasi_http() = Binding { interface: WASI_HTTP, namespace: CmNamespace::Wasi },

        /// Synthetic `<entry>` placeholder used by `from_path(&[])`.
        pub fn entry_point_synthetic() = EntryPoint {
            filename: ENTRY_FILENAME_ENTRY, package: PackageId::Root
        },
        /// `<uninitialized>` sentinel for elaborator bootstrap.
        pub fn entry_point_uninitialized() = EntryPoint {
            filename: ENTRY_FILENAME_UNINITIALIZED, package: PackageId::Root
        },
        /// `<stdin>` placeholder.
        pub fn entry_point_stdin() = EntryPoint {
            filename: ENTRY_FILENAME_STDIN, package: PackageId::Root
        },
    }

    /// The module holding `prim`'s impls. `v128` answers `primitive.wado` even
    /// though `core:simd` writes its impls, which a lookup reaches by name.
    #[must_use]
    pub fn of_primitive(prim: PrimitiveType) -> Self {
        match prim {
            PrimitiveType::F16 | PrimitiveType::Bf16 => Self::half(),
            PrimitiveType::I8
            | PrimitiveType::I16
            | PrimitiveType::I32
            | PrimitiveType::I64
            | PrimitiveType::U8
            | PrimitiveType::U16
            | PrimitiveType::U32
            | PrimitiveType::U64
            | PrimitiveType::F32
            | PrimitiveType::F64
            | PrimitiveType::V128
            | PrimitiveType::Bool
            | PrimitiveType::Char => Self::primitive(),
        }
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
            Self::Binding {
                namespace,
                interface,
            } => vec![namespace.prefix().to_string(), interface.to_string()],
            Self::Local { path, .. } => vec![path.to_string()],
            Self::Dependency { path, .. } => vec!["dep".to_string(), path.to_string()],
            Self::Remote { url, .. } => vec![url.to_string()],
            Self::EntryPoint { filename, .. } => vec![entry_basename(filename).to_string()],
            Self::Redirected { uri, .. } => vec![uri.to_string()],
            Self::Wasm { path, .. } => vec![path.to_string()],
            Self::Effect { .. } => vec![self.to_string()],
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

    /// Check if this is a bundled CM binding module (`wasi:*`).
    #[must_use]
    pub fn is_binding(&self) -> bool {
        matches!(self, Self::Binding { .. })
    }

    /// Whether this is Wado source that binds its CM types through `#[cm]`, as a
    /// dependency or Kiln-generated module does and the stdlib or a Wasm asset does not.
    #[must_use]
    pub fn is_program(&self) -> bool {
        !self.is_core() && !self.is_binding() && !self.is_wasm_asset()
    }

    /// Whether a user wrote this module: the entry point and what it
    /// transitively imports, as against the stdlib. Stdlib is the `Core` /
    /// `Wasi` / `Wasm` variants plus the bundled `.wado` files the loader
    /// registers as `Local` with a scheme-prefixed path.
    #[must_use]
    pub fn is_user_authored(&self) -> bool {
        match self {
            Self::EntryPoint { .. }
            | Self::Remote { .. }
            | Self::Dependency { .. }
            | Self::Redirected { .. } => true,
            Self::Local { path, .. } => {
                let path = path.as_str();
                !(path.starts_with("core:")
                    || path.starts_with("wasm:")
                    || CmNamespace::split_specifier(path).is_some())
            }
            _ => false,
        }
    }

    /// Whether the entry package owns this module: the entry point and the
    /// local modules it reaches inside its package root. A dependency,
    /// `core:` / `wasi:`, a remote, a Kiln-generated module and a local module
    /// of another package are all someone else's source. These are the modules
    /// a lint reports in.
    #[must_use]
    pub fn is_entry_package(&self) -> bool {
        match self {
            Self::EntryPoint { .. } => true,
            Self::Local { package, .. } => {
                !matches!(package, PackageId::Path(_) | PackageId::Unpackaged)
                    && self.is_user_authored()
            }
            _ => false,
        }
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

    /// Whether declarations here are compiler intrinsics rather than compiled
    /// source: `core:builtin` and the bundled wasm assets.
    #[must_use]
    pub fn is_builtin(&self) -> bool {
        self.is_core_builtin() || self.is_wasm_asset()
    }

    /// Whether this is `core:prelude` or one of the modules implementing it.
    #[must_use]
    pub fn is_prelude(&self) -> bool {
        matches!(self, Self::Core { name } if name == "prelude" || name.starts_with("prelude/"))
    }

    /// Check if this is the entry point module.
    #[must_use]
    pub fn is_entry_point(&self) -> bool {
        matches!(self, Self::EntryPoint { .. })
    }

    /// The namespace of an operation of `effect`.
    #[must_use]
    pub fn effect(effect: &DeclaredHead) -> Self {
        Self::Effect(Box::new(effect.clone()))
    }

    /// The interface an `E::op()` call's namespace names.
    #[must_use]
    pub fn effect_interface(&self) -> Option<&DeclaredHead> {
        match self {
            Self::Effect(head) => Some(head),
            _ => None,
        }
    }

    /// Convert to a path string format used for method name mangling.
    ///
    /// Returns `self.to_path().join("/")`:
    /// - `EntryPoint { filename }` → `"{basename}"`
    /// - `Local { path }` → `"{path}"`
    /// - `Core { name }` → `"core/{name}"`
    /// - `Binding { namespace, interface }` → `"{namespace}/{interface}"`
    /// - `Remote { url }` → `"{url}"`
    #[must_use]
    pub fn to_path_string(&self) -> String {
        self.to_path().join("/")
    }

    /// Create a module-qualified name using `//` as separator.
    ///
    /// A URL module may spell `//` itself, so only
    /// [`crate::name::unqualified_name`] reads the name back.
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
            Self::EntryPoint { filename, .. } => {
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
            Self::Binding {
                namespace,
                interface,
            } => write!(f, "{namespace}:{interface}"),
            Self::Local { path, .. } => write!(f, "{path}"),
            Self::Dependency { path, .. } => write!(f, "dep:{path}"),
            Self::Remote { url, .. } => write!(f, "{url}"),
            // Symbol identity, not a file path: the entry's qualifier is its
            // base name (its path relative to its own directory), so WIR names
            // stay stable across invocations and machines — the compile path is
            // absolute under the test harness, relative on the CLI. The real
            // path for diagnostics comes from `source_path`.
            Self::EntryPoint { filename, .. } => write!(f, "{}", entry_basename(filename)),
            Self::Redirected { uri, .. } => write!(f, "{uri}"),
            Self::Wasm { path, .. } => write!(f, "{path}"),
            Self::Effect(head) => write!(f, "{}", effect_namespace(head.module(), head.rendered())),
        }
    }
}

/// Which module's parse minted each [`AstIdSpace`], over a loaded set: what
/// turns a span or an id back into the module whose text it indexes. A body
/// walked away from its text (a trait's default reified per impl, a callee
/// spliced into its caller) holds spans of another module than its own.
#[derive(Debug, Clone, Default)]
pub struct ParseModules(hashmap::IndexMap<AstIdSpace, ModuleSource>);

impl ParseModules {
    pub fn of<'m>(modules: impl IntoIterator<Item = (&'m ModuleSource, &'m Module)>) -> Self {
        let mut spaces = hashmap::IndexMap::default();
        for (source, module) in modules {
            let claimed = spaces.insert(module.ast_id_space(), source.clone());
            assert!(
                claimed.is_none(),
                "one module is one `AstIdSpace`, but {source} shares one with {}",
                claimed.expect("just checked")
            );
        }
        Self(spaces)
    }

    /// Add another load's parses. A space is minted once, so none is in both.
    pub fn extend(&mut self, other: &Self) {
        self.0.extend(
            other
                .0
                .iter()
                .map(|(space, source)| (*space, source.clone())),
        );
    }

    /// The module `space` was parsed from, or `None` for a synthesized node,
    /// which no module wrote.
    pub fn module_of_space(&self, space: AstIdSpace) -> Option<&ModuleSource> {
        self.0.get(&space)
    }

    /// The module whose text `span` indexes, or `holder` for a synthesized
    /// span.
    pub fn module_of_span<'m>(&'m self, span: Span, holder: &'m ModuleSource) -> &'m ModuleSource {
        self.module_of_space(span.space).unwrap_or(holder)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compiler_host::DependencyIndex;
    use std::assert_matches;

    #[test]
    fn every_bundled_namespace_is_a_bundled_specifier() {
        assert!(is_bundled_specifier("core:libm.wat"));
        assert!(is_bundled_specifier("wasi:cli/stdout.wado"));
        assert!(!is_bundled_specifier("wado-lang:web/dom.wat"));
        assert!(!is_bundled_specifier("./libm.wat"));
        assert!(!is_bundled_specifier("dep:../greet/src/lib.wado"));
        assert!(!is_bundled_specifier("https://example.com/x.wasm"));
    }

    #[test]
    fn a_cm_package_comes_from_the_source_in_every_bundled_namespace() {
        assert_eq!(
            CmNamespace::split_package("wasi:filesystem/types@0.3.0"),
            Some((CmNamespace::Wasi, "filesystem"))
        );
        // `core:` is not a `CmNamespace`; the kiln lookups own those.
        assert_eq!(CmNamespace::split_package("core:kiln/types@0.1.0"), None);
        assert_eq!(CmNamespace::split_package("my:pkg/iface"), None);
    }

    #[test]
    fn a_prebuilt_component_exports_no_file() {
        let mut interner = ModuleSourceInterner::new();
        let mut index = DependencyIndex::default();
        index
            .components
            .insert("acme:geo".to_string(), "geo.wasm".to_string());
        interner.set_dependencies(index);
        let entry = interner.entry_point("main.wado");
        assert_eq!(
            interner.resolve_exported_file(&entry, "acme:geo", "src/shapes.wado"),
            Some(Err(ExportRefusal::Component)),
        );
        assert_eq!(
            interner.resolve_exported_file(&entry, "acme:other", "src/shapes.wado"),
            None,
        );
    }

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
    fn a_generated_module_joins_its_first_importers_package() {
        let mut interner = ModuleSourceInterner::new();
        let dep = interner.dependency("../greet/src/lib.wado");
        let entry = interner.entry_point("main.wado");
        let uri = "build/kiln/greet/schema.wado";
        assert_eq!(
            interner.redirected(uri, &dep).package_id(),
            dep.package_id()
        );
        assert_eq!(
            interner.redirected(uri, &entry).package_id(),
            dep.package_id()
        );
        assert_eq!(
            interner.redirected(uri, &entry),
            interner.redirected(uri, &dep)
        );
    }

    #[test]
    fn resolve_dependency_uses_registered_path() {
        let mut interner = ModuleSourceInterner::new();
        let mut index = DependencyIndex::default();
        index
            .resolved
            .insert("greet".to_string(), "../greet/src/lib.wado".to_string());
        index.unresolved.insert(
            "broken".to_string(),
            "declares no [package].lib".to_string(),
        );
        let mut greet = DependencyIndex::default();
        greet
            .resolved
            .insert("leaf".to_string(), "../leaf/lib.wado".to_string());
        index
            .packages
            .insert("../greet/src/lib.wado".to_string(), greet);
        interner.set_dependencies(index);
        let entry = interner.entry_point("main.wado");
        let greet = interner.dependency("../greet/src/lib.wado");
        assert_eq!(
            interner.resolve_dependency(&entry, "greet"),
            Some(greet.clone())
        );
        assert_eq!(interner.resolve_dependency(&entry, "missing"), None);
        // Declared-but-unresolved entries surface their reason instead.
        assert_eq!(interner.resolve_dependency(&entry, "broken"), None);
        assert_eq!(
            interner.unresolved_dependency(&entry, "broken"),
            Some("declares no [package].lib")
        );
        // A dependency resolves against its own `[dependencies]` alone.
        assert_eq!(
            interner.resolve_dependency(&greet, "leaf"),
            Some(interner.dependency("../leaf/lib.wado"))
        );
        assert_eq!(interner.resolve_dependency(&entry, "leaf"), None);
        assert_eq!(interner.resolve_dependency(&greet, "greet"), None);
    }

    #[test]
    fn test_module_source_from_path_core() {
        let mut interner = ModuleSourceInterner::new();
        let source = interner.from_path(&["core".to_string(), "prelude".to_string()]);
        assert_matches!(source, ModuleSource::Core { ref name } if name == "prelude");

        let source = interner.from_path(&["core".to_string(), "cli".to_string()]);
        assert_matches!(source, ModuleSource::Core { ref name } if name == "cli");

        let source = interner.from_path(&["core".to_string(), "rt".to_string()]);
        assert!(source.is_core_rt());
    }

    #[test]
    fn test_module_source_from_path_wasi() {
        let mut interner = ModuleSourceInterner::new();
        let source = interner.from_path(&["wasi".to_string(), "cli".to_string()]);
        assert_matches!(
            source,
            ModuleSource::Binding {
                namespace: CmNamespace::Wasi,
                ref interface
            } if interface == "cli"
        );

        let source = interner.from_path(&["wasi".to_string(), "io".to_string()]);
        assert!(source.is_binding());
    }

    #[test]
    fn test_module_source_from_path_local() {
        let mut interner = ModuleSourceInterner::new();
        let source = interner.from_path(&["./geometry.wado".to_string()]);
        assert_matches!(source, ModuleSource::Local { ref path, .. } if path == "./geometry.wado");

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
        assert!(!core.is_binding());
        assert!(!core.is_local());

        let builtin = ModuleSource::builtin();
        assert!(builtin.is_core_builtin());

        let prelude = ModuleSource::prelude();
        assert!(prelude.is_prelude());
        assert!(ModuleSource::primitive().is_prelude());

        let wasi = interner.wasi("cli");
        assert!(wasi.is_binding());
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
        assert_eq!(wasi.package_id(), PackageId::Binding(CmNamespace::Wasi));
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

        let mut stdlib_test = ModuleSourceInterner::new();
        stdlib_test.join_core_package();
        let test_entry = stdlib_test.entry_point("zlib_test.wado");
        let test_local = stdlib_test.local("./helper.wado");
        assert!(test_entry.same_package(&rt), "a stdlib test joins core");
        assert!(test_local.same_package(&test_entry));
        assert!(!test_entry.same_package(&entry));

        let dep_a = interner.dependency("dep_a/lib.wado");
        let dep_a2 = interner.dependency("dep_a/lib.wado");
        let dep_b = interner.dependency("dep_b/lib.wado");
        assert!(dep_a.same_package(&dep_a2));
        assert!(!dep_a.same_package(&dep_b));
        assert!(!dep_a.same_package(&local_a));
    }

    #[test]
    fn a_local_module_outside_the_package_root_is_another_package() {
        let root_of = |path: &str| match path.rsplit_once('/').map(|(dir, _)| dir) {
            Some(".") => Some("/app".to_string()),
            Some("../other") => Some("/other".to_string()),
            _ => None,
        };
        let root = TestPackages {
            root: root_of,
            indexes: hashmap::IndexMap::default(),
        };
        let asked = std::cell::Cell::new(0);
        let mut other = DependencyIndex::default();
        other
            .resolved
            .insert("lib:leaf".to_string(), "../leaf/lib.wado".to_string());
        other
            .packages
            .insert("../leaf/lib.wado".to_string(), DependencyIndex::default());
        let counting = TestPackages {
            root: |p: &str| {
                asked.set(asked.get() + 1);
                root_of(p)
            },
            indexes: [("/other".to_string(), other)].into_iter().collect(),
        };
        let mut interner = ModuleSourceInterner::new();
        interner.bound_packages("./main.wado", &root);
        let a = interner.place("./a.wado", &counting);
        let o = interner.place("../other/o.wado", &counting);
        let p = interner.place("../other/p.wado", &counting);
        let s = interner.place("../loose/s.wado", &counting);
        let t = interner.place("../loose/t.wado", &counting);
        assert_eq!(interner.place("./a.wado", &counting), a);
        assert_eq!(asked.get(), 3, "once per directory");

        // Each resolves against its own package's manifest, and one in no
        // package against none.
        assert!(interner.resolve_dependency(&o, "lib:leaf").is_some());
        assert!(interner.resolve_dependency(&s, "lib:leaf").is_none());
        assert!(interner.resolve_dependency(&a, "lib:leaf").is_none());

        let entry = interner.entry_point("main.wado");
        assert!(a.same_package(&entry));
        assert!(!o.same_package(&entry));
        assert!(o.same_package(&p));
        assert!(!s.same_package(&entry));
        assert!(s.same_package(&t));
        assert!(a.is_entry_package() && !o.is_entry_package() && !s.is_entry_package());

        let mut unpackaged = ModuleSourceInterner::new();
        unpackaged.bound_packages("../loose/main.wado", &root);
        let loose = unpackaged.place("../loose/s.wado", &root);
        let other = unpackaged.place("../other/o.wado", &root);
        let entry = unpackaged.entry_point("main.wado");
        assert!(loose.same_package(&entry) && loose.is_entry_package());
        assert!(!other.same_package(&entry));

        // A dependency whose package has a manifest claims its root; a
        // single-file one, which has none, claims nothing around it.
        let with_dep = |lib: &str, manifest: Option<DependencyManifest>| {
            let mut interner = ModuleSourceInterner::new();
            let mut index = DependencyIndex::default();
            index
                .resolved
                .insert("lib:other".to_string(), lib.to_string());
            let package = DependencyIndex {
                manifest,
                ..DependencyIndex::default()
            };
            index.packages.insert(lib.to_string(), package);
            interner.set_dependencies(index);
            interner.bound_packages("./main.wado", &root);
            interner
        };
        let manifest = DependencyManifest {
            path: "/other/wado.toml".to_string(),
            error: None,
        };
        let mut packaged = with_dep("../other/lib.wado", Some(manifest));
        let o = packaged.place("../other/o.wado", &root);
        assert!(o.same_package(&packaged.dependency("../other/lib.wado")));
        assert!(matches!(o, ModuleSource::Dependency { .. }));
        let mut single = with_dep("../other/single.wado", None);
        let o = single.place("../other/o.wado", &root);
        assert!(!o.same_package(&single.dependency("../other/single.wado")));
        assert!(matches!(o, ModuleSource::Local { .. }));
    }

    #[test]
    fn a_package_keeps_the_identity_it_was_first_placed_under() {
        let manifest = |path: &str| DependencyIndex {
            manifest: Some(DependencyManifest {
                path: path.to_string(),
                error: None,
            }),
            ..DependencyIndex::default()
        };
        // `a` depends on `b`, so placing a module of `a` claims `b`'s root.
        let mut a = manifest("/a/wado.toml");
        a.resolved
            .insert("lib:b".to_string(), "../b/lib.wado".to_string());
        a.packages
            .insert("../b/lib.wado".to_string(), manifest("/b/wado.toml"));
        let host = TestPackages {
            root: |path: &str| match path.split('/').nth(1) {
                Some("." | "main.wado") => Some("/app".to_string()),
                Some(name) => Some(format!("/{name}")),
                None => None,
            },
            indexes: [
                ("/a".to_string(), a),
                ("/b".to_string(), manifest("/b/wado.toml")),
            ]
            .into_iter()
            .collect(),
        };
        let mut interner = ModuleSourceInterner::new();
        interner.bound_packages("./main.wado", &host);
        let x = interner.place("../b/x.wado", &host);
        interner.place("../a/y.wado", &host);
        let z = interner.place("../b/sub/z.wado", &host);
        assert!(x.same_package(&z));
        assert!(matches!(z, ModuleSource::Local { .. }));
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
            (ModuleSource::half(), i.core("prelude/half.wado")),
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
