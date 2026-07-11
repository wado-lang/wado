//! Resolve a manifest's dependency graph into locked packages using `PubGrub`.
//!
//! Resolution is two phases. An async *prefetch* crawls the graph through the
//! [`DependencyProvider`] — listing candidate versions and fetching the manifest
//! of every version in range — and records the facts in memory. A sync *solve*
//! then runs `PubGrub` ([`pubgrub::OfflineDependencyProvider`]) over that in-memory
//! graph, which backtracks to find a set of versions satisfying every constraint
//! (or reports a precise derivation when none exists). Because `PubGrub` explores
//! multiple candidate versions, the prefetch fetches metadata for every in-range
//! version, not only the one finally selected.
//!
//! Registry and git deps are locked; path deps are flattened into their declarer
//! (traversed, never locked, per the WEP); `workspace` is not resolved yet.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;

use indexmap::IndexMap;
use pubgrub::{
    DefaultStringReporter, OfflineDependencyProvider, PubGrubError, Ranges, Reporter,
    resolve as pubgrub_solve,
};

use crate::lockfile::LockedPackage;
use crate::manifest::{DependencySource, GitPin, Manifest};
use crate::provider::{DependencyProvider, ProviderError};
use crate::version::{Version, VersionSpecifier};

#[derive(Debug, Clone)]
pub enum ResolveError {
    Provider(ProviderError),
    NoRegistry {
        dep: String,
    },
    InvalidRequirement {
        dep: String,
        requirement: String,
        reason: String,
    },
    UnsupportedSource {
        dep: String,
        kind: &'static str,
    },
    /// No set of versions satisfies every constraint. `report` is `PubGrub`'s
    /// derivation-chain explanation of the conflict.
    NoSolution {
        report: String,
    },
}

impl fmt::Display for ResolveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ResolveError::Provider(e) => write!(f, "{e}"),
            ResolveError::NoRegistry { dep } => write!(
                f,
                "dependency {dep:?}: no registry in scope (set [registries].default or a registry alias)"
            ),
            ResolveError::InvalidRequirement {
                dep,
                requirement,
                reason,
            } => write!(
                f,
                "dependency {dep:?}: invalid version requirement {requirement:?}: {reason}"
            ),
            ResolveError::UnsupportedSource { dep, kind } => {
                write!(f, "dependency {dep:?}: {kind} resolution is not yet supported")
            }
            ResolveError::NoSolution { report } => {
                write!(f, "dependency resolution failed:\n{report}")
            }
        }
    }
}

impl std::error::Error for ResolveError {}

/// `PubGrub` package identity: our lock id string (`registry+…` / `git+…`), plus a
/// synthetic root.
const ROOT: &str = "@root";
fn root_version() -> Version {
    Version::new(0, 0, 0)
}

/// A dependency edge: the child's lock id and the constraint on it. `None` is an
/// unconstrained edge — a git `ref` pin, which resolves to a single version.
type Edge = (String, Option<VersionSpecifier>);

/// How to fetch a package's versions and manifests.
#[derive(Clone)]
enum Fetch {
    Registry { url: String, package: String },
    Git { url: String, pin: GitPin, directory: Option<String> },
}

/// A resolved `(id, version)`'s facts, used to emit its [`LockedPackage`] and its
/// outgoing edges once `PubGrub` selects it.
#[derive(Default)]
struct VerData {
    edges: Vec<Edge>,
    world: IndexMap<String, String>,
    integrity: Option<String>,
    resolved_ref: Option<String>,
}

pub async fn resolve(
    manifest: &Manifest,
    provider: &impl DependencyProvider,
) -> Result<Vec<LockedPackage>, ResolveError> {
    let mut crawl = Crawl::new(provider);

    // Root edges: `[dependencies]` are runtime, `[dev-dependencies]` dev-only.
    let root_edges = crawl.expand(&manifest.dependencies, &manifest.registries, "").await?;
    let dev_edges = crawl
        .expand(&manifest.dev_dependencies, &manifest.registries, "")
        .await?;
    let nondev_roots: BTreeSet<String> = root_edges.iter().map(|(id, _)| id.clone()).collect();

    crawl.run().await?;

    let mut dp: OfflineDependencyProvider<String, Ranges<Version>> =
        OfflineDependencyProvider::new();
    let mut all_root_edges = root_edges;
    all_root_edges.extend(dev_edges);
    dp.add_dependencies(ROOT.to_string(), root_version(), crawl.ranges(&all_root_edges));
    for (id, versions) in &crawl.data {
        for (version, data) in versions {
            dp.add_dependencies(id.clone(), version.clone(), crawl.ranges(&data.edges));
        }
    }

    let solution = pubgrub_solve(&dp, ROOT.to_string(), root_version()).map_err(solve_error)?;
    let selected: BTreeMap<String, Version> = solution
        .into_iter()
        .filter(|(id, _)| id != ROOT)
        .collect();

    Ok(crawl.to_locked(&selected, &nondev_roots))
}

struct Crawl<'p, P> {
    provider: &'p P,
    /// Fetch descriptor per id (first declaration wins).
    sources: BTreeMap<String, Fetch>,
    /// Listed candidate versions per id.
    versions: BTreeMap<String, Vec<Version>>,
    /// Git tag/ref → commit SHA per id.
    shas: BTreeMap<String, BTreeMap<Version, String>>,
    /// Fetched facts: id → version → data.
    data: BTreeMap<String, BTreeMap<Version, VerData>>,
    /// `(id, version)` already fetched, so a broader requirement only fetches the delta.
    fetched: BTreeSet<(String, Version)>,
    /// Pending `(id, requirement)` fetch requests.
    queue: VecDeque<Edge>,
}

impl<'p, P: DependencyProvider> Crawl<'p, P> {
    fn new(provider: &'p P) -> Self {
        Self {
            provider,
            sources: BTreeMap::new(),
            versions: BTreeMap::new(),
            shas: BTreeMap::new(),
            data: BTreeMap::new(),
            fetched: BTreeSet::new(),
            queue: VecDeque::new(),
        }
    }

    /// Expand a manifest's dependency table into edges, flattening path deps into
    /// their declarer and enqueueing registry/git deps for crawling.
    async fn expand(
        &mut self,
        deps: &IndexMap<String, crate::manifest::Dependency>,
        registries: &IndexMap<String, String>,
        base: &str,
    ) -> Result<Vec<Edge>, ResolveError> {
        let mut edges = Vec::new();
        // Path deps flatten in; a stack avoids async recursion.
        let mut stack = vec![(deps.clone(), registries.clone(), base.to_string())];
        while let Some((deps, registries, base)) = stack.pop() {
            for (key, dep) in &deps {
                match &dep.source {
                    DependencySource::Registry {
                        registry,
                        package,
                        version,
                    } => {
                        let url = registry_url(&registries, registry.as_deref())
                            .ok_or_else(|| ResolveError::NoRegistry { dep: key.clone() })?;
                        let spec = parse_req(key, version)?;
                        let id = format!("registry+{url}/{package}");
                        self.sources.entry(id.clone()).or_insert(Fetch::Registry {
                            url,
                            package: package.clone(),
                        });
                        edges.push((id.clone(), Some(spec.clone())));
                        self.queue.push_back((id, Some(spec)));
                    }
                    DependencySource::Git {
                        url,
                        pin,
                        directory,
                    } => {
                        let id = format!("git+{url}/{key}");
                        let req = match pin {
                            GitPin::Version(v) => Some(parse_req(key, v)?),
                            GitPin::Ref(_) => None,
                        };
                        self.sources.entry(id.clone()).or_insert(Fetch::Git {
                            url: url.clone(),
                            pin: pin.clone(),
                            directory: directory.clone(),
                        });
                        edges.push((id.clone(), req.clone()));
                        self.queue.push_back((id, req));
                    }
                    DependencySource::Path { path, .. } => {
                        let dep_path = join_base(&base, path);
                        let m = self
                            .provider
                            .load_path_manifest(&dep_path)
                            .await
                            .map_err(ResolveError::Provider)?;
                        stack.push((m.dependencies, m.registries, dep_path));
                    }
                    DependencySource::Workspace => {
                        return Err(ResolveError::UnsupportedSource {
                            dep: key.clone(),
                            kind: "workspace",
                        });
                    }
                }
            }
        }
        Ok(edges)
    }

    /// Drain the queue: list each id's versions and fetch the manifest of every
    /// version in the requesting requirement's range.
    async fn run(&mut self) -> Result<(), ResolveError> {
        while let Some((id, req)) = self.queue.pop_front() {
            self.ensure_versions(&id).await?;
            let in_range: Vec<Version> = match &req {
                Some(spec) => self.versions[&id].iter().filter(|v| spec.matches(v)).cloned().collect(),
                None => self.versions[&id].clone(),
            };
            for version in in_range {
                if !self.fetched.insert((id.clone(), version.clone())) {
                    continue;
                }
                let data = self.fetch_verdata(&id, &version).await?;
                self.data.entry(id.clone()).or_default().insert(version, data);
            }
        }
        Ok(())
    }

    /// List a package's candidate versions once, recording git SHAs.
    async fn ensure_versions(&mut self, id: &str) -> Result<(), ResolveError> {
        if self.versions.contains_key(id) {
            return Ok(());
        }
        let source = self.sources[id].clone();
        match source {
            Fetch::Registry { url, package } => {
                let versions = self
                    .provider
                    .list_registry_versions(&url, &package)
                    .await
                    .map_err(ResolveError::Provider)?;
                self.versions.insert(id.to_string(), versions);
            }
            Fetch::Git { url, pin, directory } => match pin {
                GitPin::Version(_) => {
                    let tags = self
                        .provider
                        .list_git_tags(&url)
                        .await
                        .map_err(ResolveError::Provider)?;
                    let mut versions = Vec::new();
                    let mut shas = BTreeMap::new();
                    for tag in tags {
                        shas.insert(tag.version.clone(), tag.sha);
                        versions.push(tag.version);
                    }
                    self.shas.insert(id.to_string(), shas);
                    self.versions.insert(id.to_string(), versions);
                }
                GitPin::Ref(git_ref) => {
                    let sha = self
                        .provider
                        .resolve_git_ref(&url, &git_ref)
                        .await
                        .map_err(ResolveError::Provider)?;
                    let manifest = self
                        .provider
                        .fetch_git_manifest(&url, &sha, directory.as_deref())
                        .await
                        .map_err(ResolveError::Provider)?;
                    let version = package_version(&manifest);
                    self.shas
                        .insert(id.to_string(), BTreeMap::from([(version.clone(), sha)]));
                    self.versions.insert(id.to_string(), vec![version]);
                }
            },
        }
        Ok(())
    }

    /// Fetch one `(id, version)`'s manifest and record its edges + lock facts.
    async fn fetch_verdata(&mut self, id: &str, version: &Version) -> Result<VerData, ResolveError> {
        let source = self.sources[id].clone();
        match source {
            Fetch::Registry { url, package } => {
                let info = self
                    .provider
                    .fetch_registry_package(&url, &package, version)
                    .await
                    .map_err(ResolveError::Provider)?;
                let edges = self
                    .expand(&info.manifest.dependencies, &info.manifest.registries, "")
                    .await?;
                Ok(VerData {
                    edges,
                    world: world_of(&info.manifest),
                    integrity: Some(info.integrity),
                    resolved_ref: None,
                })
            }
            Fetch::Git { url, directory, .. } => {
                let sha = self.shas[id][version].clone();
                let manifest = self
                    .provider
                    .fetch_git_manifest(&url, &sha, directory.as_deref())
                    .await
                    .map_err(ResolveError::Provider)?;
                let edges = self
                    .expand(&manifest.dependencies, &manifest.registries, "")
                    .await?;
                Ok(VerData {
                    edges,
                    world: world_of(&manifest),
                    integrity: None,
                    resolved_ref: Some(sha),
                })
            }
        }
    }

    /// Convert edges into `PubGrub` `(id, Ranges)` constraints, intersecting
    /// duplicate edges to the same id. A range is the union of the child's
    /// candidate versions matching the requirement (an unconstrained edge is
    /// `full`), so it is exact against the versions actually available.
    fn ranges(&self, edges: &[Edge]) -> Vec<(String, Ranges<Version>)> {
        let mut merged: IndexMap<String, Ranges<Version>> = IndexMap::new();
        for (id, req) in edges {
            let range = match req {
                Some(spec) => self
                    .versions
                    .get(id)
                    .into_iter()
                    .flatten()
                    .filter(|v| spec.matches(v))
                    .cloned()
                    .map(Ranges::singleton)
                    .fold(Ranges::empty(), |acc, r| acc.union(&r)),
                None => Ranges::full(),
            };
            merged
                .entry(id.clone())
                .and_modify(|existing| *existing = existing.intersection(&range))
                .or_insert(range);
        }
        merged.into_iter().collect()
    }

    /// Emit the selected packages as locked entries. `nondev_roots` are the ids
    /// directly required at runtime; a package reachable from one is not dev.
    fn to_locked(
        &self,
        selected: &BTreeMap<String, Version>,
        nondev_roots: &BTreeSet<String>,
    ) -> Vec<LockedPackage> {
        let nondev = self.nondev_closure(selected, nondev_roots);
        let mut out: Vec<LockedPackage> = selected
            .iter()
            .map(|(id, version)| {
                let data = &self.data[id][version];
                let mut deps: Vec<String> = data
                    .edges
                    .iter()
                    .filter_map(|(cid, _)| selected.get(cid).map(|cv| format!("{cid}@{cv}")))
                    .collect();
                deps.sort();
                deps.dedup();
                LockedPackage {
                    id: id.clone(),
                    version: version.to_string(),
                    resolved_ref: data.resolved_ref.clone(),
                    integrity: data.integrity.clone(),
                    dev: !nondev.contains(id),
                    world: data.world.clone(),
                    deps,
                }
            })
            .collect();
        out.sort_by(|a, b| {
            a.id.cmp(&b.id)
                .then_with(|| version_order(&a.version, &b.version))
        });
        out
    }

    /// Ids reachable from a runtime root through the selected graph.
    fn nondev_closure(
        &self,
        selected: &BTreeMap<String, Version>,
        nondev_roots: &BTreeSet<String>,
    ) -> BTreeSet<String> {
        let mut reached = BTreeSet::new();
        let mut stack: Vec<String> = nondev_roots
            .iter()
            .filter(|id| selected.contains_key(*id))
            .cloned()
            .collect();
        while let Some(id) = stack.pop() {
            if !reached.insert(id.clone()) {
                continue;
            }
            if let Some(version) = selected.get(&id) {
                for (cid, _) in &self.data[&id][version].edges {
                    if selected.contains_key(cid) && !reached.contains(cid) {
                        stack.push(cid.clone());
                    }
                }
            }
        }
        reached
    }
}

type OfflineDp = OfflineDependencyProvider<String, Ranges<Version>>;

fn solve_error(err: PubGrubError<OfflineDp>) -> ResolveError {
    let report = match err {
        PubGrubError::NoSolution(tree) => DefaultStringReporter::report(&tree),
        // `OfflineDependencyProvider` is infallible, so these cannot occur.
        PubGrubError::ErrorRetrievingDependencies { .. }
        | PubGrubError::ErrorChoosingVersion { .. }
        | PubGrubError::ErrorInShouldCancel(_) => "internal resolver error".to_string(),
    };
    ResolveError::NoSolution { report }
}

fn parse_req(dep: &str, requirement: &str) -> Result<VersionSpecifier, ResolveError> {
    VersionSpecifier::parse(requirement).map_err(|e| ResolveError::InvalidRequirement {
        dep: dep.to_string(),
        requirement: requirement.to_string(),
        reason: e.to_string(),
    })
}

fn package_version(manifest: &Manifest) -> Version {
    manifest
        .package
        .as_ref()
        .and_then(|p| Version::parse(&p.version).ok())
        .unwrap_or_else(|| Version::new(0, 0, 0))
}

fn world_of(manifest: &Manifest) -> IndexMap<String, String> {
    manifest
        .world
        .iter()
        .map(|(fq, w)| (fq.clone(), w.entry.clone()))
        .collect()
}

// Order versions by semver, not lexically (so `1.9.0` precedes `1.10.0`).
fn version_order(a: &str, b: &str) -> std::cmp::Ordering {
    match (Version::parse(a), Version::parse(b)) {
        (Ok(av), Ok(bv)) => av.cmp(&bv),
        _ => a.cmp(b),
    }
}

// Join a path dep onto its declarer's dir; `..` is normalized by the filesystem.
fn join_base(base: &str, path: &str) -> String {
    if base.is_empty() {
        path.to_string()
    } else {
        format!("{}/{path}", base.trim_end_matches('/'))
    }
}

fn registry_url(registries: &IndexMap<String, String>, alias: Option<&str>) -> Option<String> {
    registries.get(alias.unwrap_or("default")).cloned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{GitTagInfo, InMemoryDependencyProvider, RegistryPackageInfo};
    use crate::version::Version;
    use std::future::Future;

    fn block_on<F: Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(future)
    }

    fn root(deps_toml: &str) -> Manifest {
        format!(
            r#"
[package]
name = "app"
version = "0.1.0"

[registries]
default = "https://wa.dev"

[dependencies]
{deps_toml}
"#
        )
        .parse()
        .unwrap()
    }

    fn leaf_info(name: &str, version: &str, integrity: &str) -> RegistryPackageInfo {
        RegistryPackageInfo {
            manifest: format!("[package]\nname = \"{name}\"\nversion = \"{version}\"\n")
                .parse()
                .unwrap(),
            integrity: integrity.to_string(),
        }
    }

    fn leaf_manifest(name: &str, version: &str) -> Manifest {
        format!("[package]\nname = \"{name}\"\nversion = \"{version}\"\n")
            .parse()
            .unwrap()
    }

    #[test]
    fn resolves_single_registry_dep() {
        block_on(async {
            let manifest = root(r#""mizchi:brotli" = { version = "^0.2.0" }"#);
            let mut provider = InMemoryDependencyProvider::new();
            provider.add_registry_package(
                "https://wa.dev",
                "mizchi:brotli",
                Version::parse("0.2.0").unwrap(),
                leaf_info("brotli", "0.2.0", "sha256:beef"),
            );

            let locked = resolve(&manifest, &provider).await.unwrap();
            assert_eq!(locked.len(), 1);
            let p = &locked[0];
            assert_eq!(p.id, "registry+https://wa.dev/mizchi:brotli");
            assert_eq!(p.version, "0.2.0");
            assert_eq!(p.integrity.as_deref(), Some("sha256:beef"));
            assert!(!p.dev);
            assert!(p.deps.is_empty());
        });
    }

    #[test]
    fn selects_highest_compatible_version() {
        block_on(async {
            let manifest = root(r#""ns:pkg" = { version = "^1.0.0" }"#);
            let mut provider = InMemoryDependencyProvider::new();
            for v in ["1.0.0", "1.2.0", "2.0.0"] {
                provider.add_registry_package(
                    "https://wa.dev",
                    "ns:pkg",
                    Version::parse(v).unwrap(),
                    leaf_info("pkg", v, &format!("sha256:{v}")),
                );
            }
            let locked = resolve(&manifest, &provider).await.unwrap();
            assert_eq!(locked.len(), 1);
            assert_eq!(locked[0].version, "1.2.0");
        });
    }

    #[test]
    fn backtracks_past_the_highest_version_to_satisfy_a_second_constraint() {
        block_on(async {
            // app -> a (-> c ">=1.0"), app -> b (-> c "=1.2"). Greedy would pick
            // the highest c (1.5) for a, then fail on b; `PubGrub` backtracks to 1.2.
            let manifest = root(
                r#""ns:a" = { version = "^1.0.0" }
"ns:b" = { version = "^1.0.0" }"#,
            );
            let dep_manifest = |name: &str, dep: &str| -> Manifest {
                format!(
                    "[package]\nname = \"{name}\"\nversion = \"1.0.0\"\n\n[registries]\ndefault = \"https://wa.dev\"\n\n[dependencies]\n{dep}\n"
                )
                .parse()
                .unwrap()
            };
            let mut provider = InMemoryDependencyProvider::new();
            provider.add_registry_package(
                "https://wa.dev",
                "ns:a",
                Version::parse("1.0.0").unwrap(),
                RegistryPackageInfo {
                    manifest: dep_manifest("a", r#""ns:c" = { version = "^1.0.0" }"#),
                    integrity: "sha256:a".to_string(),
                },
            );
            provider.add_registry_package(
                "https://wa.dev",
                "ns:b",
                Version::parse("1.0.0").unwrap(),
                RegistryPackageInfo {
                    manifest: dep_manifest("b", r#""ns:c" = { version = "~1.2.0" }"#),
                    integrity: "sha256:b".to_string(),
                },
            );
            for v in ["1.2.0", "1.5.0"] {
                provider.add_registry_package(
                    "https://wa.dev",
                    "ns:c",
                    Version::parse(v).unwrap(),
                    leaf_info("c", v, "sha256:c"),
                );
            }
            let locked = resolve(&manifest, &provider).await.unwrap();
            let c = locked
                .iter()
                .find(|p| p.id.ends_with("ns:c"))
                .expect("c present");
            assert_eq!(c.version, "1.2.0", "`PubGrub` should backtrack to 1.2.0");
        });
    }

    #[test]
    fn resolves_transitive_registry_deps() {
        block_on(async {
            let manifest = root(r#""ns:a" = { version = "^1.0.0" }"#);
            let mut provider = InMemoryDependencyProvider::new();
            let a_manifest: Manifest = r#"
[package]
name = "a"
version = "1.0.0"

[registries]
default = "https://wa.dev"

[dependencies]
"ns:b" = { version = "^1.0.0" }
"#
            .parse()
            .unwrap();
            provider.add_registry_package(
                "https://wa.dev",
                "ns:a",
                Version::parse("1.0.0").unwrap(),
                RegistryPackageInfo {
                    manifest: a_manifest,
                    integrity: "sha256:a".to_string(),
                },
            );
            provider.add_registry_package(
                "https://wa.dev",
                "ns:b",
                Version::parse("1.3.0").unwrap(),
                leaf_info("b", "1.3.0", "sha256:b"),
            );

            let locked = resolve(&manifest, &provider).await.unwrap();
            assert_eq!(locked.len(), 2, "{locked:?}");
            let a = locked
                .iter()
                .find(|p| p.id.ends_with("ns:a"))
                .expect("a present");
            assert_eq!(a.deps, vec!["registry+https://wa.dev/ns:b@1.3.0"]);
        });
    }

    #[test]
    fn incompatible_requirements_have_no_solution() {
        block_on(async {
            let manifest = root(
                r#""ns:a" = { version = "^1.0.0" }
"ns:b" = { version = "^1.0.0" }"#,
            );
            let dep_manifest = |name: &str, dep: &str| -> Manifest {
                format!(
                    "[package]\nname = \"{name}\"\nversion = \"1.0.0\"\n\n[registries]\ndefault = \"https://wa.dev\"\n\n[dependencies]\n{dep}\n"
                )
                .parse()
                .unwrap()
            };
            let mut provider = InMemoryDependencyProvider::new();
            provider.add_registry_package(
                "https://wa.dev",
                "ns:a",
                Version::parse("1.0.0").unwrap(),
                RegistryPackageInfo {
                    manifest: dep_manifest("a", r#""ns:c" = { version = "^1.0.0" }"#),
                    integrity: "sha256:a".to_string(),
                },
            );
            provider.add_registry_package(
                "https://wa.dev",
                "ns:b",
                Version::parse("1.0.0").unwrap(),
                RegistryPackageInfo {
                    manifest: dep_manifest("b", r#""ns:c" = { version = "=2.0.0" }"#),
                    integrity: "sha256:b".to_string(),
                },
            );
            for v in ["1.0.0", "2.0.0"] {
                provider.add_registry_package(
                    "https://wa.dev",
                    "ns:c",
                    Version::parse(v).unwrap(),
                    leaf_info("c", v, "sha256:c"),
                );
            }
            let err = resolve(&manifest, &provider).await.unwrap_err();
            assert!(matches!(err, ResolveError::NoSolution { .. }), "{err:?}");
        });
    }

    #[test]
    fn non_dev_requirement_downgrades_dev_mark() {
        block_on(async {
            let manifest: Manifest = r#"
[package]
name = "app"
version = "0.1.0"

[registries]
default = "https://wa.dev"

[dependencies]
"ns:x" = { version = "^1.0.0" }

[dev-dependencies]
"ns:x" = { version = "^1.0.0" }
"#
            .parse()
            .unwrap();
            let mut provider = InMemoryDependencyProvider::new();
            provider.add_registry_package(
                "https://wa.dev",
                "ns:x",
                Version::parse("1.0.0").unwrap(),
                leaf_info("x", "1.0.0", "sha256:x"),
            );
            let locked = resolve(&manifest, &provider).await.unwrap();
            assert_eq!(locked.len(), 1);
            assert!(!locked[0].dev, "a runtime dep must not be locked dev=true");
        });
    }

    #[test]
    fn dev_only_dep_is_marked_dev() {
        block_on(async {
            let manifest: Manifest = r#"
[package]
name = "app"
version = "0.1.0"

[registries]
default = "https://wa.dev"

[dev-dependencies]
"ns:x" = { version = "^1.0.0" }
"#
            .parse()
            .unwrap();
            let mut provider = InMemoryDependencyProvider::new();
            provider.add_registry_package(
                "https://wa.dev",
                "ns:x",
                Version::parse("1.0.0").unwrap(),
                leaf_info("x", "1.0.0", "sha256:x"),
            );
            let locked = resolve(&manifest, &provider).await.unwrap();
            assert_eq!(locked.len(), 1);
            assert!(locked[0].dev, "a dev-only dep must be locked dev=true");
        });
    }

    #[test]
    fn nested_path_dep_rebases_onto_declarer() {
        block_on(async {
            let manifest: Manifest = r#"
[package]
name = "app"
version = "0.1.0"

[dependencies]
"lib:a" = { path = "pkgs/a" }
"#
            .parse()
            .unwrap();
            let a: Manifest = r#"
[package]
name = "a"
version = "0.1.0"

[dependencies]
"lib:b" = { path = "../b" }
"#
            .parse()
            .unwrap();
            let b: Manifest = r#"
[package]
name = "b"
version = "0.1.0"

[registries]
default = "https://wa.dev"

[dependencies]
"ns:dep" = { version = "^1.0.0" }
"#
            .parse()
            .unwrap();
            let mut provider = InMemoryDependencyProvider::new();
            // a is at pkgs/a; its `../b` rebases to pkgs/a/../b.
            provider.add_path_manifest("pkgs/a", a);
            provider.add_path_manifest("pkgs/a/../b", b);
            provider.add_registry_package(
                "https://wa.dev",
                "ns:dep",
                Version::parse("1.0.0").unwrap(),
                leaf_info("dep", "1.0.0", "sha256:d"),
            );
            let locked = resolve(&manifest, &provider).await.unwrap();
            assert_eq!(locked.len(), 1, "{locked:?}");
            assert_eq!(locked[0].id, "registry+https://wa.dev/ns:dep");
        });
    }

    #[test]
    fn unsatisfiable_requirement_has_no_solution() {
        block_on(async {
            let manifest = root(r#""ns:pkg" = { version = "^2.0.0" }"#);
            let mut provider = InMemoryDependencyProvider::new();
            provider.add_registry_package(
                "https://wa.dev",
                "ns:pkg",
                Version::parse("1.0.0").unwrap(),
                leaf_info("pkg", "1.0.0", "sha256:x"),
            );
            let err = resolve(&manifest, &provider).await.unwrap_err();
            assert!(matches!(err, ResolveError::NoSolution { .. }), "{err:?}");
        });
    }

    #[test]
    fn path_dep_is_traversed_but_not_locked() {
        block_on(async {
            let manifest: Manifest = r#"
[package]
name = "app"
version = "0.1.0"

[dependencies]
"lib:shared" = { path = "../shared" }
"#
            .parse()
            .unwrap();
            let shared: Manifest = r#"
[package]
name = "shared"
version = "0.1.0"

[registries]
default = "https://wa.dev"

[dependencies]
"ns:dep" = { version = "^1.0.0" }
"#
            .parse()
            .unwrap();
            let mut provider = InMemoryDependencyProvider::new();
            provider.add_path_manifest("../shared", shared);
            provider.add_registry_package(
                "https://wa.dev",
                "ns:dep",
                Version::parse("1.0.0").unwrap(),
                leaf_info("dep", "1.0.0", "sha256:d"),
            );

            let locked = resolve(&manifest, &provider).await.unwrap();
            assert_eq!(locked.len(), 1, "{locked:?}");
            assert_eq!(locked[0].id, "registry+https://wa.dev/ns:dep");
        });
    }

    #[test]
    fn leaf_path_dep_locks_nothing() {
        block_on(async {
            let manifest: Manifest = r#"
[package]
name = "app"
version = "0.1.0"

[dependencies]
"lib:shared" = { path = "../shared" }
"#
            .parse()
            .unwrap();
            let mut provider = InMemoryDependencyProvider::new();
            provider.add_path_manifest("../shared", leaf_manifest("shared", "0.1.0"));

            let locked = resolve(&manifest, &provider).await.unwrap();
            assert!(locked.is_empty(), "{locked:?}");
        });
    }

    #[test]
    fn workspace_source_is_not_yet_supported() {
        block_on(async {
            let manifest: Manifest = r#"
[package]
name = "app"
version = "0.1.0"

[dependencies]
json = { workspace = true }
"#
            .parse()
            .unwrap();
            let provider = InMemoryDependencyProvider::new();
            let err = resolve(&manifest, &provider).await.unwrap_err();
            assert!(
                matches!(err, ResolveError::UnsupportedSource { kind: "workspace", .. }),
                "{err:?}"
            );
        });
    }

    #[test]
    fn resolves_version_pinned_git_dep() {
        block_on(async {
            let manifest: Manifest = r#"
[package]
name = "app"
version = "0.1.0"

[dependencies]
"user:router" = { git = "https://github.com/user/router.git", version = "^1.0.0" }
"#
            .parse()
            .unwrap();
            let mut provider = InMemoryDependencyProvider::new();
            for (v, sha) in [("1.0.0", "aaaa1111"), ("1.2.0", "bbbb2222"), ("2.0.0", "cccc3333")] {
                provider.add_git_tag(
                    "https://github.com/user/router.git",
                    GitTagInfo {
                        version: Version::parse(v).unwrap(),
                        sha: sha.to_string(),
                    },
                );
            }
            // `PubGrub` explores in-range versions, so both ^1 tags carry a manifest.
            provider.add_git_manifest(
                "https://github.com/user/router.git",
                "aaaa1111",
                leaf_manifest("router", "1.0.0"),
            );
            provider.add_git_manifest(
                "https://github.com/user/router.git",
                "bbbb2222",
                leaf_manifest("router", "1.2.0"),
            );

            let locked = resolve(&manifest, &provider).await.unwrap();
            assert_eq!(locked.len(), 1, "{locked:?}");
            let p = &locked[0];
            assert_eq!(p.id, "git+https://github.com/user/router.git/user:router");
            assert_eq!(p.version, "1.2.0");
            assert_eq!(p.resolved_ref.as_deref(), Some("bbbb2222"));
            assert!(p.integrity.is_none());
        });
    }

    #[test]
    fn resolves_ref_pinned_git_dep_using_package_version() {
        block_on(async {
            let manifest: Manifest = r#"
[package]
name = "app"
version = "0.1.0"

[dependencies]
"user:router" = { git = "https://github.com/user/router.git", ref = "main" }
"#
            .parse()
            .unwrap();
            let mut provider = InMemoryDependencyProvider::new();
            provider.add_git_ref("https://github.com/user/router.git", "main", "deadbeef1234");
            provider.add_git_manifest(
                "https://github.com/user/router.git",
                "deadbeef1234",
                leaf_manifest("router", "0.3.1"),
            );

            let locked = resolve(&manifest, &provider).await.unwrap();
            assert_eq!(locked.len(), 1, "{locked:?}");
            assert_eq!(locked[0].version, "0.3.1");
            assert_eq!(locked[0].resolved_ref.as_deref(), Some("deadbeef1234"));
        });
    }

    #[test]
    fn git_dep_transitive_registry_dep_is_locked() {
        block_on(async {
            let manifest: Manifest = r#"
[package]
name = "app"
version = "0.1.0"

[dependencies]
"user:router" = { git = "https://github.com/user/router.git", ref = "main" }
"#
            .parse()
            .unwrap();
            let router: Manifest = r#"
[package]
name = "router"
version = "1.0.0"

[registries]
default = "https://wa.dev"

[dependencies]
"ns:dep" = { version = "^1.0.0" }
"#
            .parse()
            .unwrap();
            let mut provider = InMemoryDependencyProvider::new();
            provider.add_git_ref("https://github.com/user/router.git", "main", "abc12345");
            provider.add_git_manifest("https://github.com/user/router.git", "abc12345", router);
            provider.add_registry_package(
                "https://wa.dev",
                "ns:dep",
                Version::parse("1.0.0").unwrap(),
                leaf_info("dep", "1.0.0", "sha256:d"),
            );

            let locked = resolve(&manifest, &provider).await.unwrap();
            assert_eq!(locked.len(), 2, "{locked:?}");
            let git = locked
                .iter()
                .find(|p| p.id.starts_with("git+"))
                .expect("git pkg");
            assert_eq!(git.deps, vec!["registry+https://wa.dev/ns:dep@1.0.0"]);
        });
    }

    #[test]
    fn unsatisfiable_git_version_has_no_solution() {
        block_on(async {
            let manifest: Manifest = r#"
[package]
name = "app"
version = "0.1.0"

[dependencies]
"user:router" = { git = "https://github.com/user/router.git", version = "^2.0.0" }
"#
            .parse()
            .unwrap();
            let mut provider = InMemoryDependencyProvider::new();
            provider.add_git_tag(
                "https://github.com/user/router.git",
                GitTagInfo {
                    version: Version::parse("1.0.0").unwrap(),
                    sha: "aaaa1111".to_string(),
                },
            );
            let err = resolve(&manifest, &provider).await.unwrap_err();
            assert!(matches!(err, ResolveError::NoSolution { .. }), "{err:?}");
        });
    }
}
