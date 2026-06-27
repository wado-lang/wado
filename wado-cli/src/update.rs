//! `wado update` — resolve the project's dependency graph and write `wado.lock`.

use std::fmt::Write as _;
use std::fs;

use wado_manifest::LockFile;

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
    crate::manifest::emit_manifest_warnings(&project);

    let provider = FilesystemProvider::new(project.root.clone());
    let packages = wado_manifest::resolve(&project.manifest, &provider)
        .await
        .map_err(|e| CliExit::error(format!("resolving dependencies: {e}")))?;

    let count = packages.len();
    let lock = LockFile {
        version: 1,
        deps_hash: project.manifest.deps_hash(),
        packages,
        build_dependencies: Vec::new(),
    };
    let lock_path = project.root.join("wado.lock");
    fs::write(&lock_path, lock.to_toml())
        .map_err(|e| CliExit::error(format!("writing wado.lock: {e}")))?;
    eprintln!("Locked {count} package(s) → {}", lock_path.display());
    Ok(())
}
