//! `wado workspace init | new | add`: the commands that edit
//! `wado-workspace.toml` itself, and the `members` entry `wado init` adds.

use std::fmt::Write as _;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use glob::Pattern;
use lexopt::Arg::Value;
use toml_edit::{Array, DocumentMut, Value as TomlValue};

use wado_manifest::workspace::workspace_governs;
use wado_manifest::{
    MANIFEST_FILENAME, WORKSPACE_FILENAME, WorkspaceManifest, read_workspace_members,
    resolve_member,
};

use crate::args::{self, CliExit};
use crate::init::{self, Scaffold, write_scaffold};

#[derive(Debug)]
pub enum WorkspaceCommand {
    Init(WorkspaceInitOptions),
    New(PathBuf, WorkspaceInitOptions),
    Add(PathBuf),
}

#[derive(Debug)]
pub struct WorkspaceInitOptions {
    pub namespace: Option<String>,
}

#[derive(Clone, Copy)]
enum Opt {
    Namespace,
    Help,
}

impl Opt {
    const INIT: &[Self] = &[Self::Namespace, Self::Help];
    const HELP: &[Self] = &[Self::Help];

    const fn spec(self) -> args::OptSpec {
        match self {
            Self::Namespace => args::OptSpec {
                long: Some("namespace"),
                short: None,
                value: Some("<ns>"),
                desc: "The namespace every member inherits",
            },
            Self::Help => args::HELP_SPEC,
        }
    }
}

fn format_usage() -> String {
    let mut buf = String::new();
    writeln!(buf, "Usage: wado workspace <command> [options]").unwrap();
    writeln!(buf).unwrap();
    writeln!(buf, "Edit wado-workspace.toml.").unwrap();
    writeln!(buf).unwrap();
    writeln!(buf, "Commands:").unwrap();
    writeln!(
        buf,
        "  init [options]        Create a workspace in the current directory"
    )
    .unwrap();
    writeln!(
        buf,
        "  new <dir> [options]   Create <dir> and a workspace in it"
    )
    .unwrap();
    writeln!(
        buf,
        "  add <dir>             Add the package in <dir> to `members`"
    )
    .unwrap();
    writeln!(buf).unwrap();
    writeln!(
        buf,
        "`wado new <dir>` under a workspace adds the new package to `members` itself."
    )
    .unwrap();
    buf
}

fn format_init_usage(command: &str, operand: &str) -> String {
    let mut buf = String::new();
    writeln!(buf, "Usage: wado workspace {command} {operand}[options]").unwrap();
    writeln!(buf).unwrap();
    writeln!(
        buf,
        "Write wado-workspace.toml with no members, and AGENTS.md."
    )
    .unwrap();
    writeln!(buf).unwrap();
    writeln!(buf, "Options:").unwrap();
    write!(buf, "{}", args::format_opts_help(Opt::INIT, |o| o.spec())).unwrap();
    buf
}

fn format_add_usage() -> String {
    let mut buf = String::new();
    writeln!(buf, "Usage: wado workspace add <dir>").unwrap();
    writeln!(buf).unwrap();
    writeln!(
        buf,
        "Add the package in <dir> to `members` of the workspace above it."
    )
    .unwrap();
    writeln!(buf).unwrap();
    writeln!(buf, "Options:").unwrap();
    write!(buf, "{}", args::format_opts_help(Opt::HELP, |o| o.spec())).unwrap();
    buf
}

pub fn parse_args(mut parser: lexopt::Parser) -> Result<WorkspaceCommand, CliExit> {
    let usage = format_usage();
    let Some(arg) = args::next_arg(&mut parser)? else {
        return Err(CliExit::error_with_usage("missing command", &usage));
    };
    let command = match arg {
        Value(command) => command.to_string_lossy().into_owned(),
        arg if args::match_opt(&arg, Opt::HELP, |o| o.spec()).is_some() => {
            return Err(CliExit::help(usage));
        }
        arg => return Err(args::unexpected_arg(arg, &usage)),
    };
    match command.as_str() {
        "init" => {
            let usage = format_init_usage("init", "");
            let (opts, dir) = parse_init_args(parser, &usage)?;
            if dir.is_some() {
                return Err(CliExit::error_with_usage(
                    "unexpected positional argument",
                    &usage,
                ));
            }
            Ok(WorkspaceCommand::Init(opts))
        }
        "new" => {
            let usage = format_init_usage("new", "<dir> ");
            let (opts, dir) = parse_init_args(parser, &usage)?;
            let dir = dir.ok_or_else(|| CliExit::error_with_usage("missing <dir>", &usage))?;
            Ok(WorkspaceCommand::New(dir, opts))
        }
        "add" => parse_add_args(parser).map(WorkspaceCommand::Add),
        other => Err(CliExit::error_with_usage(
            format!("unknown command '{other}'"),
            &usage,
        )),
    }
}

fn parse_init_args(
    mut parser: lexopt::Parser,
    usage: &str,
) -> Result<(WorkspaceInitOptions, Option<PathBuf>), CliExit> {
    let mut namespace = None;
    let mut dir = None;
    while let Some(arg) = args::next_arg(&mut parser)? {
        if let Some(opt) = args::match_opt(&arg, Opt::INIT, |o| o.spec()) {
            match opt {
                Opt::Namespace => namespace = Some(args::require_string(&mut parser)?),
                Opt::Help => return Err(CliExit::help(usage.to_owned())),
            }
        } else {
            args::take_dir_operand(arg, &mut dir, usage)?;
        }
    }
    Ok((WorkspaceInitOptions { namespace }, dir))
}

fn parse_add_args(mut parser: lexopt::Parser) -> Result<PathBuf, CliExit> {
    let usage = format_add_usage();
    let mut dir = None;
    while let Some(arg) = args::next_arg(&mut parser)? {
        if args::match_opt(&arg, Opt::HELP, |o| o.spec()).is_some() {
            return Err(CliExit::help(usage));
        }
        args::take_dir_operand(arg, &mut dir, &usage)?;
    }
    dir.ok_or_else(|| CliExit::error_with_usage("missing <dir>", &usage))
}

pub fn run(command: WorkspaceCommand) -> Result<(), CliExit> {
    let cwd = init::current_dir()?;
    match command {
        WorkspaceCommand::Init(opts) => init_workspace(&cwd, &opts),
        WorkspaceCommand::New(dir, opts) => {
            let dir = init::create_dir(&cwd.join(dir))?;
            init_workspace(&dir, &opts)
        }
        WorkspaceCommand::Add(dir) => add(&cwd.join(dir)),
    }
}

fn init_workspace(dir: &Path, opts: &WorkspaceInitOptions) -> Result<(), CliExit> {
    let workspace_path = dir.join(WORKSPACE_FILENAME);
    if workspace_path.exists() {
        return Err(CliExit::error(format!(
            "{WORKSPACE_FILENAME} already exists in {}",
            dir.display()
        )));
    }
    if dir.join(MANIFEST_FILENAME).exists() {
        return Err(CliExit::error(format!(
            "{} holds a package's {MANIFEST_FILENAME}; a workspace's directory holds none",
            dir.display()
        )));
    }

    let mut text = String::from("[workspace]\nmembers = []\n");
    if let Some(namespace) = &opts.namespace {
        writeln!(
            text,
            "\n[workspace.package]\nnamespace = {}",
            init::toml_string(namespace)
        )
        .unwrap();
    }
    text.parse::<WorkspaceManifest>()
        .map_err(|e| CliExit::error(format!("the workspace would not be valid: {e}")))?;
    fs::write(&workspace_path, &text)
        .map_err(|e| CliExit::error(format!("writing {WORKSPACE_FILENAME}: {e}")))?;
    eprintln!("Created {WORKSPACE_FILENAME}");

    write_scaffold(dir, Scaffold::Workspace, &init::directory_name(dir)?)
}

/// The nearest `wado-workspace.toml` strictly above `dir`, which must be
/// canonical: its directory and contents. Only the nearest one is asked, as
/// `governing_workspace` asks.
pub(crate) fn nearest_above(dir: &Path) -> Result<Option<(PathBuf, String)>, CliExit> {
    for root in dir.ancestors().skip(1) {
        let path = root.join(WORKSPACE_FILENAME);
        match fs::read_to_string(&path) {
            Ok(text) => return Ok(Some((root.to_path_buf(), text))),
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(e) => return Err(CliExit::error(format!("reading {}: {e}", path.display()))),
        }
    }
    Ok(None)
}

/// The workspace text with `member_dir` in `members`, or `None` where
/// `members` already covers it. A `members` that cannot be read governs
/// everything under it, as `governing_workspace` reads it.
pub(crate) fn with_member(
    root: &Path,
    text: &str,
    member_dir: &Path,
) -> Result<Option<String>, CliExit> {
    let governed = read_workspace_members(text)
        .is_none_or(|members| workspace_governs(root, &members, member_dir));
    if governed {
        return Ok(None);
    }
    let member = member_entry(root, member_dir);
    insert_member(text, &member).map(Some)
}

/// `member_dir` relative to `root`, `/`-separated, as a glob that matches
/// only itself.
pub(crate) fn member_entry(root: &Path, member_dir: &Path) -> String {
    let relative = member_dir
        .strip_prefix(root)
        .expect("a member lies under its workspace root");
    let parts: Vec<String> = relative
        .components()
        .map(|c| Pattern::escape(&c.as_os_str().to_string_lossy()))
        .collect();
    parts.join("/")
}

/// `text` with `member` inserted into `[workspace].members` before the first
/// entry that sorts after it, so a sorted list stays sorted. A multi-line
/// array keeps its layout; a one-line array is laid out afresh.
fn insert_member(text: &str, member: &str) -> Result<String, CliExit> {
    let invalid = |reason: &str| CliExit::error(format!("{WORKSPACE_FILENAME}: {reason}"));
    let mut doc = text
        .parse::<DocumentMut>()
        .map_err(|e| invalid(&e.to_string()))?;
    let members: &mut Array = doc
        .get_mut("workspace")
        .and_then(|workspace| workspace.get_mut("members"))
        .and_then(|members| members.as_array_mut())
        .ok_or_else(|| invalid("`[workspace].members` must be an array"))?;

    let at = members
        .iter()
        .position(|entry| entry.as_str().is_some_and(|entry| entry > member))
        .unwrap_or(members.len());
    let multi_line = members.iter().any(|entry| {
        entry
            .decor()
            .prefix()
            .and_then(|prefix| prefix.as_str())
            .is_some_and(|prefix| prefix.contains('\n'))
    });
    let mut value = TomlValue::from(member);
    if multi_line {
        let model = members
            .get(at)
            .or_else(|| members.iter().last())
            .expect("a multi-line array has an entry");
        *value.decor_mut() = model.decor().clone();
        members.insert_formatted(at, value);
    } else {
        members.insert_formatted(at, value);
        members.fmt();
    }
    Ok(doc.to_string())
}

fn add(dir: &Path) -> Result<(), CliExit> {
    let manifest_path = dir.join(MANIFEST_FILENAME);
    let manifest = match fs::read_to_string(&manifest_path) {
        Ok(manifest) => manifest,
        Err(e) if e.kind() == ErrorKind::NotFound => {
            return Err(CliExit::error(format!(
                "no {MANIFEST_FILENAME} in {}; `wado new <dir>` creates a package",
                dir.display()
            )));
        }
        Err(e) => {
            return Err(CliExit::error(format!(
                "reading {}: {e}",
                manifest_path.display()
            )));
        }
    };
    let dir = init::canonical(dir)?;
    let Some((root, text)) = nearest_above(&dir)? else {
        return Err(CliExit::error(format!(
            "no {WORKSPACE_FILENAME} above {}",
            dir.display()
        )));
    };
    let entry = member_entry(&root, &dir);
    let Some(updated) = with_member(&root, &text, &dir)? else {
        eprintln!("{entry} is already a member");
        return Ok(());
    };
    resolve_member(&manifest, &updated).map_err(|e| {
        CliExit::error(format!(
            "{} cannot join the workspace: {e}",
            manifest_path.display()
        ))
    })?;
    write_workspace(&root, &updated, &entry)
}

pub(crate) fn write_workspace(root: &Path, text: &str, entry: &str) -> Result<(), CliExit> {
    let path = root.join(WORKSPACE_FILENAME);
    fs::write(&path, text)
        .map_err(|e| CliExit::error(format!("writing {}: {e}", path.display())))?;
    eprintln!("Added {entry} to members in {}", path.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_keeps_a_multi_line_layout() {
        let text = "[workspace]\nmembers = [\n    \"a\",\n    \"c\",\n]\n";
        assert_eq!(
            insert_member(text, "b").unwrap(),
            "[workspace]\nmembers = [\n    \"a\",\n    \"b\",\n    \"c\",\n]\n"
        );
        assert_eq!(
            insert_member(text, "d").unwrap(),
            "[workspace]\nmembers = [\n    \"a\",\n    \"c\",\n    \"d\",\n]\n"
        );
    }

    #[test]
    fn insert_lays_out_a_one_line_array() {
        let text = "[workspace]\nmembers = [\"b\"]\n";
        assert_eq!(
            insert_member(text, "a").unwrap(),
            "[workspace]\nmembers = [\"a\", \"b\"]\n"
        );
        assert_eq!(
            insert_member(text, "c").unwrap(),
            "[workspace]\nmembers = [\"b\", \"c\"]\n"
        );
        assert_eq!(
            insert_member("[workspace]\nmembers = []\n", "a").unwrap(),
            "[workspace]\nmembers = [\"a\"]\n"
        );
    }

    #[test]
    fn member_entry_escapes_glob_characters() {
        assert_eq!(
            member_entry(Path::new("/ws"), Path::new("/ws/pkgs/[x]")),
            "pkgs/[[]x[]]"
        );
    }
}
