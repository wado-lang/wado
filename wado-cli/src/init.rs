use std::env;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use toml::Value as TomlValue;
use wado_lsp::host::discovery::{Disk, nearest_workspace, slash_separated};

use wado_manifest::{
    MANIFEST_FILENAME, Manifest, ManifestError, WORKSPACE_FILENAME, WorkspaceManifest,
    WorkspacePackage, resolve_member,
};

use crate::args::{self, CliExit};
use crate::discover::checkout_root;
use crate::git;
use crate::workspace::{member_entry, with_member, write_workspace};

#[derive(Debug)]
pub struct InitOptions {
    pub name: Option<String>,
    pub namespace: Option<String>,
    pub kind: Kind,
    pub force: bool,
}

/// The worlds a new package declares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Lib,
    Cli,
    Both,
}

impl Kind {
    /// What `--lib` and `--cli` ask for; neither means a library.
    fn from_flags(lib: bool, cli: bool) -> Self {
        match (lib, cli) {
            (_, false) => Self::Lib,
            (false, true) => Self::Cli,
            (true, true) => Self::Both,
        }
    }

    fn declares_lib(self) -> bool {
        matches!(self, Self::Lib | Self::Both)
    }

    fn declares_cli(self) -> bool {
        matches!(self, Self::Cli | Self::Both)
    }
}

#[derive(Clone, Copy)]
enum Opt {
    Name,
    Namespace,
    Lib,
    Cli,
    Force,
    Help,
}

impl Opt {
    const ALL: &[Self] = &[
        Self::Name,
        Self::Namespace,
        Self::Lib,
        Self::Cli,
        Self::Force,
        Self::Help,
    ];

    const fn spec(self) -> args::OptSpec {
        match self {
            Self::Name => args::OptSpec {
                long: Some("name"),
                short: None,
                value: Some("<name>"),
                desc: "Package name (default: the directory's name)",
            },
            Self::Namespace => args::OptSpec {
                long: Some("namespace"),
                short: None,
                value: Some("<ns>"),
                desc: "Package namespace (required unless the workspace sets one)",
            },
            Self::Lib => args::OptSpec {
                long: Some("lib"),
                short: None,
                value: None,
                desc: "Declare the library world, src/lib.wado (the default)",
            },
            Self::Cli => args::OptSpec {
                long: Some("cli"),
                short: None,
                value: None,
                desc: "Declare the wasi:cli/command world, src/main.wado",
            },
            Self::Force => args::OptSpec {
                long: Some("force"),
                short: Some('f'),
                value: None,
                desc: "Overwrite existing wado.toml",
            },
            Self::Help => args::HELP_SPEC,
        }
    }
}

/// The usage of `init` and of what runs it, `new`: the same options and the
/// same scaffold, under `command`'s own synopsis.
pub(crate) fn scaffold_usage(command: &str, operand: &str, summary: &str) -> String {
    let mut buf = String::new();
    writeln!(buf, "Usage: wado {command} {operand}[options]").unwrap();
    writeln!(buf).unwrap();
    writeln!(buf, "{summary}").unwrap();
    writeln!(buf).unwrap();
    writeln!(
        buf,
        "It writes wado.toml, README.md, AGENTS.md and src/, and tests/ with the"
    )
    .unwrap();
    writeln!(
        buf,
        "library. A file of these already there is kept, but an existing wado.toml"
    )
    .unwrap();
    writeln!(
        buf,
        "is refused unless --force. Under a workspace whose `members` misses the"
    )
    .unwrap();
    writeln!(
        buf,
        "package, the package is added to `members`, unless git ignores it."
    )
    .unwrap();
    writeln!(buf).unwrap();
    writeln!(buf, "Options:").unwrap();
    write!(buf, "{}", args::format_opts_help(Opt::ALL, |o| o.spec())).unwrap();
    buf
}

pub fn parse_args(parser: lexopt::Parser) -> Result<InitOptions, CliExit> {
    let usage = scaffold_usage("init", "", "Create a package in the current directory.");
    let (opts, dir) = parse_options(parser, &usage)?;
    if dir.is_some() {
        return Err(CliExit::error_with_usage(
            "unexpected positional argument",
            &usage,
        ));
    }
    Ok(opts)
}

/// The options `init` and `new` share, and the one operand only `new` takes.
pub(crate) fn parse_options(
    mut parser: lexopt::Parser,
    usage: &str,
) -> Result<(InitOptions, Option<PathBuf>), CliExit> {
    let mut name = None;
    let mut namespace = None;
    let mut lib = false;
    let mut cli = false;
    let mut force = false;
    let mut dir = None;

    while let Some(arg) = args::next_arg(&mut parser)? {
        if let Some(opt) = args::match_opt(&arg, Opt::ALL, |o| o.spec()) {
            match opt {
                Opt::Name => name = Some(args::require_string(&mut parser)?),
                Opt::Namespace => namespace = Some(args::require_string(&mut parser)?),
                Opt::Lib => lib = true,
                Opt::Cli => cli = true,
                Opt::Force => force = true,
                Opt::Help => return Err(CliExit::help(usage.to_owned())),
            }
        } else {
            args::take_dir_operand(arg, &mut dir, usage)?;
        }
    }
    let opts = InitOptions {
        name,
        namespace,
        kind: Kind::from_flags(lib, cli),
        force,
    };
    Ok((opts, dir))
}

pub fn run(opts: InitOptions) -> Result<(), CliExit> {
    init_package(&current_dir()?, &opts)
}

pub(crate) fn init_package(dir: &Path, opts: &InitOptions) -> Result<(), CliExit> {
    if dir.join(MANIFEST_FILENAME).exists() && !opts.force {
        return Err(CliExit::error(format!(
            "{MANIFEST_FILENAME} already exists in {} (use --force to overwrite)",
            dir.display()
        )));
    }
    if dir.join(WORKSPACE_FILENAME).exists() {
        return Err(CliExit::error(format!(
            "{} holds {WORKSPACE_FILENAME}; a member goes in a directory under it (`wado new <dir>`)",
            dir.display()
        )));
    }
    let dir = canonical(dir)?;

    let workspace = match nearest_workspace(&dir, &Disk) {
        Some((root, text)) => match with_member(&root, &text, &dir)? {
            None => Some(Membership {
                text,
                joining: false,
                root,
            }),
            // Only what lies below the workspace root is asked: a workspace in
            // ignored scratch space still takes members.
            Some(_) if git::ignores(&dir)? && !git::ignores(&root)? => {
                eprintln!(
                    "{} is ignored by git, so it does not join the workspace",
                    dir.display()
                );
                None
            }
            Some(joined) => Some(Membership {
                text: joined,
                joining: true,
                root,
            }),
        },
        None => None,
    };
    let inherited = match &workspace {
        Some(membership) => {
            membership
                .text
                .parse::<WorkspaceManifest>()
                .map_err(|e| CliExit::error(format!("invalid {WORKSPACE_FILENAME}: {e}")))?
                .workspace
                .package
        }
        None => None,
    }
    .unwrap_or_default();

    let namespace = match (opts.namespace.as_deref(), inherited.namespace.as_deref()) {
        (Some(_), Some(namespace)) => {
            return Err(CliExit::error(format!(
                "the workspace sets the namespace `{namespace}`; leave out --namespace"
            )));
        }
        (None, None) => {
            return Err(CliExit::error(
                "--namespace is required: no workspace this package belongs to sets one",
            ));
        }
        (namespace, _) => namespace,
    };
    let name = match &opts.name {
        Some(name) => name.clone(),
        None => directory_name(&dir)?,
    };
    let repository_directory = inherited
        .repository
        .is_some()
        .then(|| repository_directory(&dir))
        .flatten();
    let content = generate_manifest(&ManifestFields {
        namespace,
        name: &name,
        version: inherited.version.is_none().then_some("0.1.0"),
        repository_directory: repository_directory.as_deref(),
        kind: opts.kind,
        inherited: inherited_fields(&inherited),
    });
    match &workspace {
        Some(membership) => resolve_member(&content, &membership.text),
        None => content.parse::<Manifest>(),
    }
    .map_err(|e| {
        let named_after_dir = opts.name.is_none()
            && matches!(&e, ManifestError::InvalidName { field, .. } if field == "package.name");
        let hint = if named_after_dir {
            "; the name is the directory's, so choose one with --name"
        } else {
            ""
        };
        CliExit::error(format!("the manifest would not be valid: {e}{hint}"))
    })?;

    // The workspace goes first: a `members` entry naming a directory with no
    // manifest yet is skipped, while a manifest left out of the workspace it
    // leaves fields to does not parse.
    if let Some(membership) = &workspace
        && membership.joining
    {
        write_workspace(
            &membership.root,
            &membership.text,
            &member_entry(&membership.root, &dir),
        )?;
    }
    fs::write(dir.join(MANIFEST_FILENAME), &content)
        .map_err(|e| CliExit::error(format!("writing {MANIFEST_FILENAME}: {e}")))?;
    eprintln!("Created {MANIFEST_FILENAME}");
    write_scaffold(&dir, Scaffold::Package(opts.kind), &name)
}

/// The workspace above a new package, as it reads once the package is a
/// member.
struct Membership {
    root: PathBuf,
    text: String,
    /// Whether `text` adds the package to `members`, to be written back.
    joining: bool,
}

struct ManifestFields<'a> {
    namespace: Option<&'a str>,
    name: &'a str,
    version: Option<&'a str>,
    repository_directory: Option<&'a str>,
    kind: Kind,
    inherited: Vec<&'static str>,
}

/// A manifest leaving out each field `None` stands for, which the governing
/// workspace sets.
fn generate_manifest(fields: &ManifestFields) -> String {
    let mut buf = String::new();
    buf.push_str("[package]\n");
    if let Some(namespace) = fields.namespace {
        writeln!(buf, "namespace = {}", toml_string(namespace)).unwrap();
    }
    writeln!(buf, "name = {}", toml_string(fields.name)).unwrap();
    if let Some(version) = fields.version {
        writeln!(buf, "version = {}", toml_string(version)).unwrap();
    }
    if let Some(directory) = fields.repository_directory {
        writeln!(buf, "repository-directory = {}", toml_string(directory)).unwrap();
    }
    if fields.kind.declares_lib() {
        writeln!(buf, "lib = \"src/lib.wado\"").unwrap();
    }
    if !fields.inherited.is_empty() {
        writeln!(
            buf,
            "# {} inherited from [workspace.package]",
            fields.inherited.join(" / ")
        )
        .unwrap();
    }
    if fields.kind.declares_cli() {
        buf.push_str("\n[world]\n");
        writeln!(buf, "\"wasi:cli/command\" = \"src/main.wado\"").unwrap();
    }
    buf
}

pub(crate) fn toml_string(s: &str) -> String {
    TomlValue::String(s.to_owned()).to_string()
}

/// The `[workspace.package]` fields a member inherits, in the order the
/// specification lists them.
fn inherited_fields(package: &WorkspacePackage) -> Vec<&'static str> {
    let WorkspacePackage {
        version,
        repository,
        namespace,
        license,
        license_file,
        authors,
        wado_version,
        unknown_fields: _,
    } = package;
    [
        ("version", version.is_some()),
        ("repository", repository.is_some()),
        ("namespace", namespace.is_some()),
        ("license", license.is_some()),
        ("license-file", license_file.is_some()),
        ("authors", !authors.is_empty()),
        ("wado-version", wado_version.is_some()),
    ]
    .into_iter()
    .filter_map(|(field, set)| set.then_some(field))
    .collect()
}

/// `dir` relative to the root of the git checkout holding it, which is what
/// `repository-directory` names. `None` outside a checkout or at its root.
fn repository_directory(dir: &Path) -> Option<String> {
    let root = checkout_root(dir)?;
    let relative = dir.strip_prefix(root).expect("an ancestor is a prefix");
    (relative != Path::new("")).then(|| slash_separated(relative))
}

#[derive(Clone, Copy)]
pub(crate) enum Scaffold {
    Package(Kind),
    Workspace,
}

/// A file the scaffold writes: its path under the new directory and its
/// text, each with `{name}` and `{layout_url}` still to fill.
struct Template {
    path: &'static str,
    text: &'static str,
}

const README: Template = Template {
    path: "README.md",
    text: include_str!("../assets/scaffold/package/README.template.md"),
};
const PACKAGE_AGENTS: Template = Template {
    path: "AGENTS.md",
    text: include_str!("../assets/scaffold/package/AGENTS.template.md"),
};
const LIB: Template = Template {
    path: "src/lib.wado",
    text: include_str!("../assets/scaffold/package/src/lib.wado"),
};
const LIB_TEST: Template = Template {
    path: "tests/{name}_test.wado",
    text: include_str!("../assets/scaffold/package/tests/lib_test.wado"),
};
const MAIN: Template = Template {
    path: "src/main.wado",
    text: include_str!("../assets/scaffold/package/src/main.wado"),
};
const WORKSPACE_AGENTS: Template = Template {
    path: "AGENTS.md",
    text: include_str!("../assets/scaffold/workspace/AGENTS.template.md"),
};

impl Scaffold {
    fn templates(self) -> Vec<&'static Template> {
        match self {
            Self::Package(kind) => {
                let mut templates = vec![&README, &PACKAGE_AGENTS];
                if kind.declares_lib() {
                    templates.extend([&LIB, &LIB_TEST]);
                }
                if kind.declares_cli() {
                    templates.push(&MAIN);
                }
                templates
            }
            Self::Workspace => vec![&WORKSPACE_AGENTS],
        }
    }
}

/// The standard package layout as the running `wado` implements it, so the
/// link names the version that wrote the files.
fn layout_url() -> String {
    format!(
        "https://github.com/wado-lang/wado/blob/v{}/docs/spec-package-standard.md",
        env!("CARGO_PKG_VERSION")
    )
}

/// Write each file of `scaffold` under `dir`, keeping one already there.
pub(crate) fn write_scaffold(dir: &Path, scaffold: Scaffold, name: &str) -> Result<(), CliExit> {
    let layout_url = layout_url();
    let fill = |s: &str| {
        s.replace("{name}", name)
            .replace("{layout_url}", &layout_url)
    };
    for template in scaffold.templates() {
        let relative = fill(template.path);
        let path = dir.join(&relative);
        if path.exists() {
            eprintln!("Kept {relative}");
            continue;
        }
        let parent = path
            .parent()
            .expect("a scaffold file lies under its directory");
        fs::create_dir_all(parent)
            .map_err(|e| CliExit::error(format!("creating {}: {e}", parent.display())))?;
        fs::write(&path, fill(template.text))
            .map_err(|e| CliExit::error(format!("writing {relative}: {e}")))?;
        eprintln!("Created {relative}");
    }
    Ok(())
}

pub(crate) fn current_dir() -> Result<PathBuf, CliExit> {
    env::current_dir().map_err(|e| CliExit::error(format!("reading the current directory: {e}")))
}

pub(crate) const CANONICAL_IS_UTF8: &str = "a path through `canonical` must be UTF-8";

/// `dir` resolved. A path that is not UTF-8 is refused here, where it enters:
/// the manifests name a package by its directory, in text.
pub(crate) fn canonical(dir: &Path) -> Result<PathBuf, CliExit> {
    let resolved = dir
        .canonicalize()
        .map_err(|e| CliExit::error(format!("{}: {e}", dir.display())))?;
    if resolved.to_str().is_none() {
        return Err(CliExit::error(format!(
            "{} is not UTF-8, so no manifest can name it",
            dir.display()
        )));
    }
    Ok(resolved)
}

/// `mkdir -p dir`, then `init` there, given the canonical path. Where `init`
/// fails, the directories this run created go again: nothing but this run
/// wrote under them.
pub(crate) fn in_new_dir(
    dir: &Path,
    init: impl FnOnce(&Path) -> Result<(), CliExit>,
) -> Result<(), CliExit> {
    // One level at a time, since `..` in the operand puts a created directory
    // off the ancestors of `dir`.
    let mut created = Vec::new();
    let mut path = PathBuf::new();
    let mut result = Ok(());
    for component in dir.components() {
        path.push(component);
        if path.exists() {
            continue;
        }
        if let Err(e) = fs::create_dir(&path) {
            result = Err(CliExit::error(format!("creating {}: {e}", path.display())));
            break;
        }
        created.push(path.clone());
    }
    let result = result.and_then(|()| canonical(dir)).and_then(|dir| init(&dir));
    if result.is_err() {
        // A directory is created after its parent, so the reverse order
        // removes each before the directory holding it.
        for created in created.iter().rev() {
            fs::remove_dir_all(created)
                .map_err(|e| CliExit::error(format!("removing {}: {e}", created.display())))?;
        }
    }
    result
}

pub(crate) fn directory_name(dir: &Path) -> Result<String, CliExit> {
    dir.file_name()
        .map(|name| name.to_str().expect(CANONICAL_IS_UTF8).to_owned())
        .ok_or_else(|| CliExit::error(format!("{} has no name; pass --name", dir.display())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_manifest_parses() {
        let result = generate_manifest(&ManifestFields {
            namespace: Some("myorg"),
            name: "my-app",
            version: Some("0.1.0"),
            repository_directory: None,
            kind: Kind::Both,
            inherited: Vec::new(),
        });
        let manifest: Manifest = result.parse().unwrap();
        let package = manifest.package.as_ref().unwrap();
        assert_eq!(package.coordinate(), "myorg:my-app@0.1.0");
        assert_eq!(package.lib.as_deref(), Some("src/lib.wado"));
        assert_eq!(
            manifest.world_entry("wasi:cli/command"),
            Some("src/main.wado")
        );
    }

    #[test]
    fn parse_options_defaults_to_a_library() {
        let parser = lexopt::Parser::from_args(vec!["--namespace", "myorg"]);
        let (opts, dir) = parse_options(parser, "").unwrap();
        assert_eq!(opts.kind, Kind::Lib);
        assert!(opts.name.is_none());
        assert!(dir.is_none());
    }

    #[test]
    fn parse_options_takes_cli_alone() {
        let parser = lexopt::Parser::from_args(vec!["--cli", "app"]);
        let (opts, dir) = parse_options(parser, "").unwrap();
        assert_eq!(opts.kind, Kind::Cli);
        assert_eq!(dir, Some(PathBuf::from("app")));
    }
}
