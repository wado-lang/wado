//! The user's own settings: `$XDG_CONFIG_HOME/wado/config.toml`, defaulting to
//! `~/.config/wado/config.toml`. Read once per process.
//!
//! The file can open paths to every child a guest spawns, so one that another
//! user can write is ignored whole, as `ssh` ignores such a config: owned by
//! someone else, or writable by group or others.

use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use serde::Deserialize;
use wado_lsp::host::discovery::home_dir;

#[derive(Default)]
pub struct UserConfig {
    /// Where the file was read from, for the warnings about its entries.
    source: PathBuf,
    /// `root`: the dependency cache root.
    pub root: Option<PathBuf>,
    /// `[process] read`, `~` expanded and not yet resolved.
    process_read: Vec<PathBuf>,
    resolved_process_read: OnceLock<Vec<PathBuf>>,
}

#[derive(Deserialize, Default)]
struct RawConfig {
    root: Option<String>,
    #[serde(default)]
    process: RawProcess,
}

#[derive(Deserialize, Default)]
struct RawProcess {
    #[serde(default)]
    read: Vec<String>,
}

/// The settings, read on first use. A file that cannot be trusted or parsed
/// warns and reads as empty.
pub fn user_config() -> &'static UserConfig {
    static CONFIG: OnceLock<UserConfig> = OnceLock::new();
    CONFIG.get_or_init(|| {
        let Some(path) = config_path() else {
            return UserConfig::default();
        };
        load(&path).unwrap_or_else(|reason| {
            eprintln!("warning: ignoring {}: {reason}", path.display());
            UserConfig::default()
        })
    })
}

fn config_path() -> Option<PathBuf> {
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(xdg).join("wado").join("config.toml"));
    }
    home_dir().map(|home| home.join(".config").join("wado").join("config.toml"))
}

fn load(path: &Path) -> Result<UserConfig, String> {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(UserConfig::default()),
        Err(e) => return Err(e.to_string()),
    };
    // The open handle's metadata, so the file checked is the file read.
    check_trusted(&file.metadata().map_err(|e| e.to_string())?)?;
    let mut text = String::new();
    file.read_to_string(&mut text).map_err(|e| e.to_string())?;
    parse(path, &text)
}

#[cfg(unix)]
fn check_trusted(meta: &std::fs::Metadata) -> Result<(), String> {
    use std::os::unix::fs::MetadataExt;
    // SAFETY: plain system call.
    let me = unsafe { libc::geteuid() };
    if meta.uid() != me && meta.uid() != 0 {
        return Err("it is owned by another user".to_owned());
    }
    if meta.mode() & 0o022 != 0 {
        return Err("other users can write it (`chmod go-w` it)".to_owned());
    }
    Ok(())
}

#[cfg(not(unix))]
fn check_trusted(_meta: &std::fs::Metadata) -> Result<(), String> {
    Ok(())
}

fn parse(source: &Path, text: &str) -> Result<UserConfig, String> {
    let raw: RawConfig = toml::from_str(text).map_err(|e| e.message().to_owned())?;
    let mut process_read = Vec::with_capacity(raw.process.read.len());
    for entry in raw.process.read {
        let path = expand_tilde(&entry);
        if path.is_absolute() {
            process_read.push(path);
        } else {
            eprintln!(
                "warning: {}: [process] read `{entry}` is neither absolute nor under `~/`; skipped",
                source.display()
            );
        }
    }
    Ok(UserConfig {
        source: source.to_owned(),
        root: raw.root.map(|root| expand_tilde(&root)),
        process_read,
        resolved_process_read: OnceLock::new(),
    })
}

fn expand_tilde(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/")
        && let Some(home) = home_dir()
    {
        return home.join(rest);
    }
    PathBuf::from(path)
}

impl UserConfig {
    /// `[process] read`, resolved on first use as `--process-read` resolves
    /// its path. One absent on this machine warns and is skipped, so a config
    /// shared across machines names each one's toolchains.
    pub fn process_read(&self) -> &[PathBuf] {
        self.resolved_process_read.get_or_init(|| {
            self.process_read
                .iter()
                .filter_map(|path| match std::fs::canonicalize(path) {
                    Ok(resolved) => Some(resolved),
                    Err(e) => {
                        eprintln!(
                            "warning: {}: [process] read {}: {e}; skipped",
                            self.source.display(),
                            path.display()
                        );
                        None
                    }
                })
                .collect()
        })
    }
}

#[cfg(test)]
mod tests;
