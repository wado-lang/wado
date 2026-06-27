//! Dependency resolution: walk a manifest's dependency graph through a
//! [`DependencyProvider`] and produce locked packages for `wado.lock`.
//!
//! Scope: registry dependencies (the wa.dev path). Git, path, and workspace
//! sources are recognized and reported as not-yet-resolved so they slot into
//! the same worklist later, without changing the shape of the result.
//!
//! Version selection is highest-compatible per requirement, first-wins on a
//! repeated id. Full conflict-driven resolution (`PubGrub`: multi-version
//! coexistence, backtracking) is a later refinement behind this same seam.

use std::collections::{BTreeMap, VecDeque};
use std::fmt;

use indexmap::IndexMap;

use crate::lockfile::LockedPackage;
use crate::manifest::{Dependency, DependencySource, Manifest};
use crate::provider::{DependencyProvider, ProviderError};
use crate::version::VersionSpecifier;

/// Errors from dependency resolution.
#[derive(Debug, Clone)]
pub enum ResolveError {
    /// A provider I/O operation failed.
    Provider(ProviderError),
    /// No available version satisfies the requirement.
    NoMatchingVersion {
        package: String,
        requirement: String,
    },
    /// A registry dependency names an unknown alias, or omits `registry` with
    /// no `default` registry in scope.
    NoRegistry { dep: String },
    /// The version requirement could not be parsed.
    InvalidRequirement {
        dep: String,
        requirement: String,
        reason: String,
    },
    /// The source kind is not resolved yet (git/path/workspace).
    UnsupportedSource { dep: String, kind: &'static str },
}

impl fmt::Display for ResolveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ResolveError::Provider(e) => write!(f, "{e}"),
            ResolveError::NoMatchingVersion {
                package,
                requirement,
            } => write!(f, "no version of {package:?} matches {requirement:?}"),
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

/// One pending dependency, carrying the registries table of the manifest that
/// declared it (a transitive dep resolves aliases against its own manifest).
struct Frame {
    key: String,
    source: DependencySource,
    registries: IndexMap<String, String>,
    dev: bool,
}

/// Resolve a manifest's dependency graph into locked packages, sorted by
/// `(id, version)` for a deterministic `wado.lock`.
///
/// # Errors
///
/// Returns [`ResolveError`] on provider failure, an unsatisfiable requirement,
/// a missing registry, or a not-yet-supported source kind.
pub async fn resolve(
    manifest: &Manifest,
    provider: &impl DependencyProvider,
) -> Result<Vec<LockedPackage>, ResolveError> {
    let mut resolved: BTreeMap<String, LockedPackage> = BTreeMap::new();
    // Direct child ids per resolved id, filled while resolving, joined with the
    // children's chosen versions once everything is known.
    let mut children: BTreeMap<String, Vec<String>> = BTreeMap::new();

    let mut queue: VecDeque<Frame> = VecDeque::new();
    enqueue(&mut queue, &manifest.dependencies, manifest, false);
    enqueue(&mut queue, &manifest.dev_dependencies, manifest, true);

    while let Some(frame) = queue.pop_front() {
        let DependencySource::Registry {
            registry,
            package,
            version,
        } = &frame.source
        else {
            return Err(ResolveError::UnsupportedSource {
                dep: frame.key.clone(),
                kind: source_kind(&frame.source),
            });
        };

        let url = registry_url(&frame.registries, registry.as_deref()).ok_or_else(|| {
            ResolveError::NoRegistry {
                dep: frame.key.clone(),
            }
        })?;
        let id = format!("registry+{url}/{package}");
        if resolved.contains_key(&id) {
            continue;
        }

        let req =
            VersionSpecifier::parse(version).map_err(|e| ResolveError::InvalidRequirement {
                dep: frame.key.clone(),
                requirement: version.clone(),
                reason: e.to_string(),
            })?;
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

        // Enqueue transitive deps against the fetched manifest's own registries.
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
    out.sort_by(|a, b| a.id.cmp(&b.id).then_with(|| a.version.cmp(&b.version)));
    Ok(out)
}

fn enqueue(
    queue: &mut VecDeque<Frame>,
    deps: &IndexMap<String, Dependency>,
    ctx: &Manifest,
    dev: bool,
) {
    for (key, dep) in deps {
        queue.push_back(Frame {
            key: key.clone(),
            source: dep.source.clone(),
            registries: ctx.registries.clone(),
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
            // a@1.0.0 depends on b ^1.0
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
