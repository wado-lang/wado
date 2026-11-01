//! A filesystem `CompilerHost` that answers from stubs what a real host would
//! ask its environment, for compiling the e2e fixtures outside `wado`.

use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};

use indexmap::IndexMap;
use wado_compiler::kiln::GENERATOR_WORLD_FQ;
use wado_compiler::remote::{Integrity, is_remote};
use wado_compiler::{
    CompilerHost, DependencyIndex, Diagnostic, GeneratorRequest, GeneratorResponse,
    GeneratorRunnerError, ModuleDir, PackageDir, ProbeRequest, SourceError, SourceReader,
};
use wado_lsp::FilesystemCompilerHost;
use wado_lsp::host::dependency_key;
use wado_lsp::host::discovery::{
    nearest_manifest_dir, normalize_path, resolve_member_manifest, slash_separated,
};
use wado_manifest::{DependencySource, MANIFEST_FILENAME, Manifest};

use crate::kiln::component_cache::ComponentCache;
use crate::kiln::runtime::{generate_for, probe_for};

/// What a [`StubHost`] answers in place of the environment.
#[derive(Debug, Default, Clone)]
pub struct HostStubs {
    /// The compile-time environment `#[param(from_env = ...)]` reads.
    pub env: IndexMap<String, String>,
    /// The `[dependencies]` a bare `use ... from "name"` binds to: name → the
    /// dependency's `[package].lib` path, relative to the host's base.
    pub dependencies: IndexMap<String, String>,
    /// The directories, relative to the host's base, that stand where a
    /// `wado.toml` would.
    pub package_roots: Vec<String>,
    /// The `[build-dependencies]` a clause's `module: "lib:name"` names: the
    /// nickname → the generator's `.wado` file, relative to the host's base.
    pub build_dependencies: IndexMap<String, String>,
    /// What each URL serves: the URL → a file, relative to the host's base.
    pub remote: IndexMap<String, String>,
}

/// A [`FilesystemCompilerHost`] that answers the dependency index and the
/// environment from [`HostStubs`] rather than from `wado.toml` and the process,
/// and runs Kiln generators as `wado` does.
#[derive(Debug)]
pub struct StubHost {
    filesystem: FilesystemCompilerHost,
    stubs: HostStubs,
    packages: Packages,
}

/// Where a [`StubHost`] learns which package a module is in and what it
/// depends on.
#[derive(Debug, Clone, Copy)]
enum Packages {
    /// From [`HostStubs`], as a fixture states them.
    Stubbed,
    /// From the `wado.toml` files on disk, as `wado` reads them.
    Manifests,
}

/// The generator components every [`StubHost`] shares, so a run of many
/// fixtures compiles each generator once.
static GENERATORS: LazyLock<ComponentCache> = LazyLock::new(ComponentCache::default);

impl StubHost {
    /// A host loading sources relative to `base_path`, with nothing stubbed.
    #[must_use]
    pub fn new(base_path: PathBuf) -> Self {
        wado_lsp::install_stderr_trace_sink();
        Self {
            filesystem: FilesystemCompilerHost::new(base_path),
            stubs: HostStubs::default(),
            packages: Packages::Stubbed,
        }
    }

    /// A host loading sources relative to `base_path` whose packages are the
    /// `wado.toml` files on disk. The nearest one's path `[build-dependencies]`
    /// name the generators its clauses may run, each by its package's
    /// `[world]."core:kiln/generator"` entry.
    ///
    /// # Panics
    ///
    /// When that manifest does not parse, or a path build dependency declares no
    /// generator: the caller chose this tree as a package.
    #[must_use]
    pub fn reading_manifests(base_path: PathBuf) -> Self {
        let build_dependencies = manifest_generators(&base_path, &base_path);
        Self {
            stubs: HostStubs {
                build_dependencies,
                ..HostStubs::default()
            },
            packages: Packages::Manifests,
            ..Self::new(base_path)
        }
    }

    /// This host, answering from `stubs` what a real host would ask its
    /// environment.
    #[must_use]
    pub fn with_stubs(mut self, stubs: HostStubs) -> Self {
        self.stubs = stubs;
        self
    }

    /// What this host answers in place of the environment.
    #[must_use]
    pub fn stubs(&self) -> &HostStubs {
        &self.stubs
    }

    /// Whether the `wado.toml` files on disk say which package a module is in.
    #[must_use]
    pub fn reads_manifests(&self) -> bool {
        matches!(self.packages, Packages::Manifests)
    }

    /// The host the clauses of the generator in `generator_dir` run on, loading
    /// sources relative to `base_path`. A stubbed host keeps its stubs; one
    /// reading manifests resolves the clauses' nicknames through the
    /// generator's own package, as `wado` does.
    #[must_use]
    pub fn for_generator_clauses(&self, base_path: PathBuf, generator_dir: &Path) -> Self {
        match self.packages {
            Packages::Stubbed => Self::new(base_path).with_stubs(self.stubs.clone()),
            Packages::Manifests => Self {
                stubs: HostStubs {
                    build_dependencies: manifest_generators(generator_dir, &base_path),
                    ..HostStubs::default()
                },
                packages: Packages::Manifests,
                ..Self::new(base_path)
            },
        }
    }

    /// The host a generator at `base_path` compiles on. A fixture's stubs are
    /// the fixture's, not the generator's, so a stubbed host passes none on; a
    /// package's generator compiles in its own package.
    #[must_use]
    pub fn for_generator(&self, base_path: PathBuf) -> Self {
        match self.packages {
            Packages::Stubbed => Self::new(base_path),
            Packages::Manifests => Self::reading_manifests(base_path),
        }
    }

    /// The diagnostics emitted since this host was created.
    #[must_use]
    pub fn diagnostics(&self) -> Vec<Diagnostic> {
        self.filesystem.diagnostics()
    }

    /// The file `path` names: itself, or the one stubbed for a URL.
    fn local<'p>(&'p self, path: &'p str) -> Result<&'p str, SourceError> {
        if !is_remote(path) {
            return Ok(path);
        }
        self.stubs
            .remote
            .get(path)
            .map(String::as_str)
            .ok_or_else(|| SourceError::NotFound {
                path: path.to_string(),
            })
    }
}

impl CompilerHost for StubHost {
    async fn load_source(&self, path: &str) -> Result<Vec<u8>, SourceError> {
        self.filesystem.load_source(path).await
    }

    async fn source_exists(&self, path: &str) -> bool {
        self.filesystem.source_exists(path).await
    }

    async fn open_source(
        &self,
        path: &str,
        integrity: Option<Integrity>,
    ) -> Result<Arc<dyn SourceReader>, SourceError> {
        self.filesystem
            .open_source(self.local(path)?, integrity)
            .await
    }

    async fn load_source_prefix(
        &self,
        path: &str,
        extent: Option<u64>,
    ) -> Result<Vec<u8>, SourceError> {
        self.filesystem
            .load_source_prefix(self.local(path)?, extent)
            .await
    }

    fn emit_diagnostic(&self, diagnostic: Diagnostic) {
        assert_diagnostic_is_attributed(&diagnostic);
        self.filesystem.collect_diagnostic(diagnostic);
    }

    fn env_var(&self, name: &str) -> Option<String> {
        self.stubs.env.get(name).cloned()
    }

    fn stdlib_module_read(&self, import_path: &str) {
        self.filesystem.stdlib_module_read(import_path);
    }

    /// In the nearest stubbed root above the module, compared lexically: a
    /// fixture directory holds no symlink. A stubbed package has no
    /// `[package]`.
    fn module_dir(&self, path: &str) -> ModuleDir {
        if let Packages::Manifests = self.packages {
            return self.filesystem.module_dir(path);
        }
        let base = self.filesystem.base_path();
        let file = normalize_path(&base.join(path));
        let dir = file.parent().expect("a module path names a file");
        let package = self
            .stubs
            .package_roots
            .iter()
            .map(|root| (root, normalize_path(&base.join(root))))
            .filter(|(_, abs)| dir.starts_with(abs))
            .max_by_key(|(_, abs)| abs.components().count())
            .map(|(root, abs)| PackageDir {
                root: root.clone(),
                coordinate: None,
                dir: slash_separated(dir.strip_prefix(abs).expect("filtered on the prefix")),
            });
        ModuleDir {
            package,
            ..ModuleDir::unpackaged(path)
        }
    }

    /// A stubbed package declares no dependencies of its own.
    fn package_index(&self, root: &str) -> DependencyIndex {
        match self.packages {
            Packages::Stubbed => DependencyIndex::default(),
            Packages::Manifests => self.filesystem.package_index(root),
        }
    }

    fn dependency_index(&self) -> DependencyIndex {
        if let Packages::Manifests = self.packages {
            return self.filesystem.dependency_index();
        }
        let mut index = DependencyIndex::default();
        for (name, lib) in &self.stubs.dependencies {
            index.resolved.insert(name.clone(), lib.clone());
            index
                .packages
                .insert(lib.clone(), DependencyIndex::default());
        }
        index
    }

    async fn run_generator(
        &self,
        component_wasm: &[u8],
        request: GeneratorRequest,
    ) -> Result<GeneratorResponse, GeneratorRunnerError> {
        let (engine, component) = GENERATORS.get_or_compile(component_wasm)?;
        generate_for(self, &engine, &component, request).await
    }

    async fn probe_generator(
        &self,
        component_wasm: &[u8],
        request: &ProbeRequest,
    ) -> Result<Vec<Option<u64>>, GeneratorRunnerError> {
        let (engine, component) = GENERATORS.get_or_compile(component_wasm)?;
        probe_for(self, &engine, &component, request).await
    }
}

/// A located diagnostic names the file it is in — what a per-document consumer
/// selects on, and without which the LSP drops it. Checked at the host, so the
/// whole fixture corpus enforces it. A span-less diagnostic is about the
/// compilation rather than a place in it, and is exempt.
fn assert_diagnostic_is_attributed(diagnostic: &Diagnostic) {
    if let Some(span) = diagnostic.span.as_ref() {
        assert!(
            !span.file.is_empty(),
            "diagnostic carries a span but no file: {} ({:?}) at {}:{}\n\
             emit it through `Elaborator::emit` / `Logger::error_in`, not `Logger::error`",
            diagnostic.message,
            diagnostic.code,
            span.line,
            span.column,
        );
    }
}

/// The generator each path `[build-dependencies]` entry of the manifest
/// nearest `dir` names, keyed by its nickname, as a path relative to `base`.
/// No manifest above `dir` names none.
fn manifest_generators(dir: &Path, base: &Path) -> IndexMap<String, String> {
    let Some(dir) = nearest_manifest_dir(dir) else {
        return IndexMap::new();
    };
    let manifest = read_manifest(&dir);
    manifest
        .build_dependencies
        .iter()
        .filter_map(|(nickname, dependency)| match &dependency.source {
            DependencySource::Path { path, .. } => Some((nickname, dir.join(path))),
            DependencySource::Git { .. }
            | DependencySource::Registry { .. }
            | DependencySource::Workspace => None,
        })
        .map(|(nickname, root)| {
            let entry = read_manifest(&root)
                .world_entry(GENERATOR_WORLD_FQ)
                .unwrap_or_else(|| {
                    panic!(
                        "{} declares no `{GENERATOR_WORLD_FQ}` world",
                        root.display()
                    )
                })
                .to_string();
            (nickname.clone(), dependency_key(&root.join(entry), base))
        })
        .collect()
}

fn read_manifest(dir: &Path) -> Manifest {
    let path = dir.join(MANIFEST_FILENAME);
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
    resolve_member_manifest(dir, &text)
        .unwrap_or_else(|e| panic!("{} does not parse: {e}", path.display()))
}
