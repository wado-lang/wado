//! Getting a component out of the input, by calling `wado` for a `.wado` file.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context as _, Result, bail};
use tempfile::{TempDir, tempdir};

use crate::args::Args;

/// A component to run. A compiled one holds its temporary directory open for
/// as long as the run needs the file in it.
pub struct Component {
    path: PathBuf,
    _dir: Option<TempDir>,
}

impl Component {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Compile the input unless it is already a component.
pub fn component_for(args: &Args) -> Result<Component> {
    if args.input.extension().is_some_and(|ext| ext == "wasm") {
        return Ok(Component {
            path: args.input.clone(),
            _dir: None,
        });
    }

    let dir = tempdir()?;
    let path = dir.path().join("program.wasm");
    let wado = wado_binary();
    let status = Command::new(&wado)
        .arg("compile")
        .arg(format!("-O{}", args.opt_level))
        .arg("-o")
        .arg(&path)
        .arg(&args.input)
        .status()
        .with_context(|| format!("failed to run '{}'", wado.display()))?;
    if !status.success() {
        bail!("{} compile failed", wado.display());
    }

    Ok(Component {
        path,
        _dir: Some(dir),
    })
}

/// `WADO` is the binary that dispatched this subcommand, so a child compiles
/// with the same `wado` the user invoked. Run directly, it falls back to `PATH`.
fn wado_binary() -> PathBuf {
    std::env::var_os("WADO").map_or_else(|| PathBuf::from("wado"), PathBuf::from)
}
