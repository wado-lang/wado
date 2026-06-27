//! Resolve a manifest's dependency graph through a [`DependencyProvider`] into
//! locked packages. Registry deps are locked; path deps are traversed but not
//! locked (WEP); git/workspace are not resolved yet. Version selection is
//! highest-compatible per requirement; a conflicting second requirement is an
//! error, not a silently-wrong lock (no backtracking yet — `PubGrub` later).

use std::collections::{BTreeMap, VecDeque};
use std::fmt;

use indexmap::IndexMap;

use crate::lockfile::LockedPackage;
use crate::manifest::{Dependency, DependencySource, Manifest};
use crate::provider::{DependencyProvider, ProviderError};
use crate::version::{Version, VersionSpecifier};

#[derive(Debug, Clone)]
pub enum ResolveError {
    Provider(ProviderError),
    NoMatchingVersion {
        package: String,
        requirement: String,
    },
    VersionConflict {
        package: String,
        requirement: String,
        resolved: String,
    },
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
}

impl fmt::Display for ResolveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ResolveError::Provider(e) => write!(f, "{e}"),
            ResolveError::NoMatchingVersion {
                package,
                requirement,
            } => write!(f, "no version of {package:?} matches {requirement:?}"),
            ResolveError::VersionConflict {
                package,
                requirement,
                resolved,
            } => write!(
                f,
                "{package:?} is required as {requirement:?} but already resolved to {resolved:?}"
            ),
            ResolveError::NoRegistry { dep } => {
                write!(
                    f,
                    "dependency {dep:?}: no registry in scope (set [registries].default or a registry alias)"
                )
            }
            ResolveError::InvalidRequirement {
                dep,
                requirement,
                reason,
            } => write!(
                f,
                "dependency {dep:?}: invalid version requirement {requirement:?}: {reason}"
            ),
            ResolveError::UnsupportedSource { dep, kind } => {
                write!(
                    f,
                    "dependency {dep:?}: {kind} resolution is not yet supported"
                )
            }
        }
    }
}

impl std::error::Error for ResolveError {}

struct Frame {
    key: String,
    source: DependencySource,
    // The declaring manifest's registries and directory (relative to the project
    // root); `base` rebases nested path deps onto their declarer.
    registries: IndexMap<String, String>,
    base: String,
    dev: bool,
}

pub async fn resolve(
    manifest: &Manifest,
    provider: &impl DependencyProvider,
) -> Result<Vec<LockedPackage>, ResolveError> {
    let mut resolved: BTreeMap<String, LockedPackage> = BTreeMap::new();
    // Child ids per id; joined with the children's chosen versions in a second pass.
    let mut children: BTreeMap<String, Vec<String>> = BTreeMap::new();

    let mut queue: VecDeque<Frame> = VecDeque::new();
    enqueue(&mut queue, &manifest.dependencies, manifest, "", false);
    enqueue(&mut queue, &manifest.dev_dependencies, manifest, "", true);

    while let Some(frame) = queue.pop_front() {
        let (registry, package, version) = match &frame.source {
            DependencySource::Registry {
                registry,
                package,
                version,
            } => (registry, package, version),
            // Path deps are traversed for transitive deps but never locked (WEP).
            DependencySource::Path { path, .. } => {
                let dep_path = join_base(&frame.base, path);
                let dep_manifest = provider
                    .load_path_manifest(&dep_path)
                    .await
                    .map_err(ResolveError::Provider)?;
                enqueue(
                    &mut queue,
                    &dep_manifest.dependencies,
                    &dep_manifest,
                    &dep_path,
                    frame.dev,
                );
                continue;
            }
            DependencySource::Git { .. } | DependencySource::Workspace => {
                return Err(ResolveError::UnsupportedSource {
                    dep: frame.key.clone(),
                    kind: source_kind(&frame.source),
                });
            }
        };

        let url = registry_url(&frame.registries, registry.as_deref()).ok_or_else(|| {
            ResolveError::NoRegistry {
                dep: frame.key.clone(),
            }
        })?;
        // package is `ns:pkg` (no `/`), so the last `/` splits url from package.
        let id = format!("registry+{url}/{package}");
        let req =
            VersionSpecifier::parse(version).map_err(|e| ResolveError::InvalidRequirement {
                dep: frame.key.clone(),
                requirement: version.clone(),
                reason: e.to_string(),
            })?;
        // Already resolved: a conflicting requirement errors; a non-dev use clears dev.
        if let Some(existing) = resolved.get_mut(&id) {
            let existing_ver =
                Version::parse(&existing.version).expect("locked version came from Version");
            if !req.matches(&existing_ver) {
                return Err(ResolveError::VersionConflict {
                    package: package.clone(),
                    requirement: version.clone(),
                    resolved: existing.version.clone(),
                });
            }
            if !frame.dev {
                existing.dev = false;
            }
            continue;
        }

        let available = provider
            .list_registry_versions(&url, package)
            .await
            .map_err(ResolveError::Provider)?;
        let chosen = available
            .into_iter()
            .filter(|v| req.matches(v))
            .max()
            .ok_or_else(|| ResolveError::NoMatchingVersion {
                package: package.clone(),
                requirement: version.clone(),
            })?;
        let info = provider
            .fetch_registry_package(&url, package, &chosen)
            .await
            .map_err(ResolveError::Provider)?;

        let mut child_ids = Vec::new();
        for (ckey, cdep) in &info.manifest.dependencies {
            if let DependencySource::Registry {
                registry: creg,
                package: cpkg,
                ..
            } = &cdep.source
                && let Some(curl) = registry_url(&info.manifest.registries, creg.as_deref())
            {
                child_ids.push(format!("registry+{curl}/{cpkg}"));
            }
            queue.push_back(Frame {
                key: ckey.clone(),
                source: cdep.source.clone(),
                registries: info.manifest.registries.clone(),
                base: String::new(),
                dev: frame.dev,
            });
        }

        resolved.insert(
            id.clone(),
            LockedPackage {
                id: id.clone(),
                version: chosen.to_string(),
                resolved_ref: None,
                integrity: Some(info.integrity.clone()),
                dev: frame.dev,
                world: info.manifest.world.clone(),
                deps: Vec::new(),
            },
        );
        children.insert(id, child_ids);
    }

    let versions: BTreeMap<&String, &String> =
        resolved.iter().map(|(id, p)| (id, &p.version)).collect();
    let dep_refs: BTreeMap<String, Vec<String>> = children
        .iter()
        .map(|(id, cids)| {
            let mut refs: Vec<String> = cids
                .iter()
                .filter_map(|cid| versions.get(cid).map(|v| format!("{cid}@{v}")))
                .collect();
            refs.sort();
            refs.dedup();
            (id.clone(), refs)
        })
        .collect();
    for (id, pkg) in &mut resolved {
        if let Some(refs) = dep_refs.get(id) {
            pkg.deps.clone_from(refs);
        }
    }

    let mut out: Vec<LockedPackage> = resolved.into_values().collect();
    out.sort_by(|a, b| {
        a.id.cmp(&b.id)
            .then_with(|| version_order(&a.version, &b.version))
    });
    Ok(out)
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

fn enqueue(
    queue: &mut VecDeque<Frame>,
    deps: &IndexMap<String, Dependency>,
    ctx: &Manifest,
    base: &str,
    dev: bool,
) {
    for (key, dep) in deps {
        queue.push_back(Frame {
            key: key.clone(),
            source: dep.source.clone(),
            registries: ctx.registries.clone(),
            base: base.to_string(),
            dev,
        });
    }
}

fn registry_url(registries: &IndexMap<String, String>, alias: Option<&str>) -> Option<String> {
    registries.get(alias.unwrap_or("default")).cloned()
}

fn source_kind(source: &DependencySource) -> &'static str {
    match source {
        DependencySource::Git { .. } => "git",
        DependencySource::Path { .. } => "path",
        DependencySource::Workspace => "workspace",
        DependencySource::Registry { .. } => "registry",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{InMemoryDependencyProvider, RegistryPackageInfo};
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
    fn incompatible_requirements_conflict() {
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
            assert!(
                matches!(err, ResolveError::VersionConflict { .. }),
                "{err:?}"
            );
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
    fn unsatisfiable_requirement_errors() {
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
            assert!(
                matches!(err, ResolveError::NoMatchingVersion { .. }),
                "{err:?}"
            );
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

    fn leaf_manifest(name: &str, version: &str) -> Manifest {
        format!("[package]\nname = \"{name}\"\nversion = \"{version}\"\n")
            .parse()
            .unwrap()
    }

    #[test]
    fn git_source_is_not_yet_supported() {
        block_on(async {
            let manifest: Manifest = r#"
[package]
name = "app"
version = "0.1.0"

[dependencies]
"lib:router" = { git = "https://github.com/user/router.git", ref = "main" }
"#
            .parse()
            .unwrap();
            let provider = InMemoryDependencyProvider::new();
            let err = resolve(&manifest, &provider).await.unwrap_err();
            assert!(
                matches!(err, ResolveError::UnsupportedSource { kind: "git", .. }),
                "{err:?}"
            );
        });
    }
}
