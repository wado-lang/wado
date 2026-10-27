//! `wado update` — resolve the project's dependency graph and write `wado.lock`.

use std::fmt::Write as _;
use std::fs;

use wado_manifest::{LOCK_FILENAME, LockFile};

use crate::args::{self, CliExit};
use crate::build_dep::resolve_build_dependencies;
use crate::manifest::{discover, emit_manifest_warnings, hold_lock_file};
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
    emit_manifest_warnings(&project);

    let provider = FilesystemProvider::new(project.root.clone());
    let packages = wado_manifest::resolve(&project.manifest, &provider)
        .await
        .map_err(|e| CliExit::error(format!("resolving dependencies: {e}")))?;
    // `update` re-resolves build-dependencies fresh (empty lock) so a new
    // published version is picked up, then pins each with its integrity digest.
    let resolved_build_deps =
        resolve_build_dependencies(&project.manifest, &indexmap::IndexMap::new())
            .await
            .map_err(|e| CliExit::error(format!("resolving build-dependencies: {e}")))?;
    let mut build_dependencies = Vec::with_capacity(resolved_build_deps.len());
    for dep in &resolved_build_deps {
        let integrity = dep
            .integrity()
            .await
            .map_err(|e| CliExit::error(format!("resolving build-dependencies: {e}")))?;
        build_dependencies.push(dep.locked_package(integrity));
    }

    let lock_path = project.root.join(LOCK_FILENAME);
    let count = packages.len() + build_dependencies.len();
    // A remote pin is dropped: the next build records what it reads then, as
    // only a build knows how far a Kiln probe reads.
    let lock = LockFile {
        version: 1,
        deps_hash: project.manifest.deps_hash(),
        packages,
        build_dependencies,
        remotes: Vec::new(),
    };
    let _held = hold_lock_file(&project.root)
        .map_err(|e| CliExit::error(format!("holding wado.lock: {e}")))?;
    fs::write(&lock_path, lock.to_toml())
        .map_err(|e| CliExit::error(format!("writing wado.lock: {e}")))?;
    eprintln!("Locked {count} package(s) → {}", lock_path.display());
    Ok(())
}
