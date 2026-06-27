//! `wado update` — resolve the project's dependency graph and write `wado.lock`.

use std::fmt::Write as _;
use std::fs;

use sha2::{Digest, Sha256};
use wado_manifest::{DependencySource, GitPin, LockFile, Manifest};

use crate::args::{self, CliExit};
use crate::manifest::discover;
use crate::registry::FilesystemProvider;

#[derive(Debug)]
pub struct UpdateOptions {}

fn format_usage() -> String {
    let mut buf = String::new();
    writeln!(buf, "Usage: wado update [options]").unwrap();
    writeln!(buf).unwrap();
    writeln!(
        buf,
        "Resolve the project's dependencies and write wado.lock."
    )
    .unwrap();
    writeln!(buf).unwrap();
    writeln!(buf, "Options:").unwrap();
    writeln!(buf, "  -h, --help  Show this help message").unwrap();
    buf
}

pub fn parse_args(mut parser: lexopt::Parser) -> Result<UpdateOptions, CliExit> {
    let usage = format_usage();
    if let Some(arg) = args::next_arg(&mut parser)? {
        return match arg {
            lexopt::Arg::Long("help") | lexopt::Arg::Short('h') => Err(CliExit::help(usage)),
            other => Err(args::unexpected_arg(other, &usage)),
        };
    }
    Ok(UpdateOptions {})
}

pub async fn run(_opts: UpdateOptions) -> Result<(), CliExit> {
    let cwd = std::env::current_dir()
        .map_err(|e| CliExit::error(format!("cannot get current directory: {e}")))?;
    let project = discover(&cwd)
        .map_err(CliExit::error)?
        .ok_or_else(|| CliExit::error("no wado.toml found"))?;

    let provider = FilesystemProvider::new(project.root.clone());
    let packages = wado_manifest::resolve(&project.manifest, &provider)
        .await
        .map_err(|e| CliExit::error(format!("resolving dependencies: {e}")))?;

    let count = packages.len();
    let lock = LockFile {
        version: 1,
        deps_hash: deps_hash(&project.manifest),
        packages,
        build_dependencies: Vec::new(),
    };
    let lock_path = project.root.join("wado.lock");
    fs::write(&lock_path, lock.to_toml())
        .map_err(|e| CliExit::error(format!("writing wado.lock: {e}")))?;
    eprintln!("Locked {count} package(s) → {}", lock_path.display());
    Ok(())
}

/// Hash of the `[dependencies]` + `[dev-dependencies]` sections for lock-file
/// staleness detection. Deterministic: keys sorted, each source rendered to a
/// stable fingerprint.
fn deps_hash(manifest: &Manifest) -> String {
    let mut hasher = Sha256::new();
    for (label, deps) in [
        ("deps", &manifest.dependencies),
        ("dev", &manifest.dev_dependencies),
    ] {
        let mut keys: Vec<&String> = deps.keys().collect();
        keys.sort();
        for key in keys {
            hasher.update(label.as_bytes());
            hasher.update(b"\0");
            hasher.update(key.as_bytes());
            hasher.update(b"\0");
            hasher.update(source_fingerprint(&deps[key].source).as_bytes());
            hasher.update(b"\n");
        }
    }
    let digest = hasher.finalize();
    let mut out = String::with_capacity(7 + 64);
    out.push_str("sha256:");
    for byte in &digest {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

fn source_fingerprint(source: &DependencySource) -> String {
    match source {
        DependencySource::Registry {
            registry,
            package,
            version,
        } => format!(
            "registry|{}|{package}|{version}",
            registry.as_deref().unwrap_or("")
        ),
        DependencySource::Git { url, pin } => match pin {
            GitPin::Version(v) => format!("git|{url}|version|{v}"),
            GitPin::Ref(r) => format!("git|{url}|ref|{r}"),
        },
        DependencySource::Path { path, .. } => format!("path|{path}"),
        DependencySource::Workspace => "workspace".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(deps: &str) -> Manifest {
        format!(
            r#"
[package]
name = "app"
version = "0.1.0"

[registries]
default = "https://wa.dev"

[dependencies]
{deps}
"#
        )
        .parse()
        .unwrap()
    }

    #[test]
    fn deps_hash_is_stable() {
        let a = deps_hash(&manifest(r#""ns:pkg" = { version = "^1.0.0" }"#));
        let b = deps_hash(&manifest(r#""ns:pkg" = { version = "^1.0.0" }"#));
        assert_eq!(a, b);
        assert!(a.starts_with("sha256:"));
    }

    #[test]
    fn deps_hash_changes_with_version() {
        let a = deps_hash(&manifest(r#""ns:pkg" = { version = "^1.0.0" }"#));
        let b = deps_hash(&manifest(r#""ns:pkg" = { version = "^2.0.0" }"#));
        assert_ne!(a, b);
    }
}
