use std::env;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use toml::Value as TomlValue;
use wado_lsp::host::discovery::slash_separated;

use wado_manifest::{
    MANIFEST_FILENAME, Manifest, ManifestError, WORKSPACE_FILENAME, WorkspaceManifest,
    WorkspacePackage, resolve_member,
};

use crate::args::{self, CliExit};
use crate::discover::{checkout_root, is_git_ignored};
use crate::workspace::{member_entry, nearest_above, with_member, write_workspace};

#[derive(Debug)]
pub struct InitOptions {
    pub name: Option<String>,
    pub namespace: Option<String>,
    pub kind: Kind,
    pub force: bool,
}

/// The worlds a new package declares. Neither flag given means a library.
#[derive(Debug, Clone, Copy)]
pub struct Kind {
    pub lib: bool,
    pub cli: bool,
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

fn format_usage(command: &str, operand: &str, summary: &str) -> String {
    let mut buf = String::new();
    writeln!(buf, "Usage: wado {command} {operand}[options]").unwrap();
    writeln!(buf).unwrap();
    writeln!(buf, "{summary}").unwrap();
    writeln!(buf).unwrap();
    writeln!(
        buf,
        "It writes wado.toml, README.md, AGENTS.md, src/ and tests/, keeping any of"
    )
    .unwrap();
    writeln!(
        buf,
        "them but wado.toml that is already there. Under a workspace whose `members`"
    )
    .unwrap();
    writeln!(
        buf,
        "misses the package, the package is added to `members`, unless git ignores it."
    )
    .unwrap();
    writeln!(buf).unwrap();
    writeln!(buf, "Options:").unwrap();
    write!(buf, "{}", args::format_opts_help(Opt::ALL, |o| o.spec())).unwrap();
    buf
}

pub(crate) fn init_usage() -> String {
    format_usage("init", "", "Create a package in the current directory.")
}

pub(crate) fn new_usage() -> String {
    format_usage(
        "new",
        "<dir> ",
        "Create <dir> and a package in it: `mkdir -p <dir> && cd <dir> && wado init`.",
    )
}

pub fn parse_args(parser: lexopt::Parser) -> Result<InitOptions, CliExit> {
    let usage = init_usage();
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
    let mut opts = InitOptions {
        name: None,
        namespace: None,
        kind: Kind {
            lib: false,
            cli: false,
        },
        force: false,
    };
    let mut dir = None;

    while let Some(arg) = args::next_arg(&mut parser)? {
        if let Some(opt) = args::match_opt(&arg, Opt::ALL, |o| o.spec()) {
            match opt {
                Opt::Name => opts.name = Some(args::require_string(&mut parser)?),
                Opt::Namespace => opts.namespace = Some(args::require_string(&mut parser)?),
                Opt::Lib => opts.kind.lib = true,
                Opt::Cli => opts.kind.cli = true,
                Opt::Force => opts.force = true,
                Opt::Help => return Err(CliExit::help(usage.to_owned())),
            }
        } else {
            args::take_dir_operand(arg, &mut dir, usage)?;
        }
    }
    if !opts.kind.cli {
        opts.kind.lib = true;
    }
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

    let workspace = match nearest_above(&dir)? {
        Some((root, text)) => match with_member(&root, &text, &dir)? {
            None => Some(Membership {
                text,
                joining: false,
                root,
            }),
            Some(_) if is_git_ignored(&dir).map_err(CliExit::error)? => {
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
            return Err(CliExit::error(format!(
                "--namespace is required: no workspace sets one\n\n{}",
                init_usage()
            )));
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

    fs::write(dir.join(MANIFEST_FILENAME), &content)
        .map_err(|e| CliExit::error(format!("writing {MANIFEST_FILENAME}: {e}")))?;
    eprintln!("Created {MANIFEST_FILENAME}");
    write_scaffold(&dir, Scaffold::Package(opts.kind), &name)?;

    if let Some(membership) = workspace
        && membership.joining
    {
        write_workspace(
            &membership.root,
            &membership.text,
            &member_entry(&membership.root, &dir),
        )?;
    }
    Ok(())
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
    if fields.kind.lib {
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
    if fields.kind.cli {
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
                if kind.lib {
                    templates.extend([&LIB, &LIB_TEST]);
                }
                if kind.cli {
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

pub(crate) fn canonical(dir: &Path) -> Result<PathBuf, CliExit> {
    dir.canonicalize()
        .map_err(|e| CliExit::error(format!("{}: {e}", dir.display())))
}

/// `mkdir -p dir`, answering the directory's canonical path.
pub(crate) fn create_dir(dir: &Path) -> Result<PathBuf, CliExit> {
    fs::create_dir_all(dir)
        .map_err(|e| CliExit::error(format!("creating {}: {e}", dir.display())))?;
    canonical(dir)
}

pub(crate) fn directory_name(dir: &Path) -> Result<String, CliExit> {
    dir.file_name()
        .map(|name| name.to_string_lossy().into_owned())
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
            kind: Kind {
                lib: true,
                cli: true,
            },
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
        assert!(opts.kind.lib && !opts.kind.cli);
        assert!(opts.name.is_none());
        assert!(dir.is_none());
    }

    #[test]
    fn parse_options_takes_cli_alone() {
        let parser = lexopt::Parser::from_args(vec!["--cli", "app"]);
        let (opts, dir) = parse_options(parser, "").unwrap();
        assert!(!opts.kind.lib && opts.kind.cli);
        assert_eq!(dir, Some(PathBuf::from("app")));
    }
}
