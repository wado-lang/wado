use std::fmt::Write as _;
use std::fs;
use std::path::Path;

use lexopt::Arg::Value;

use crate::args::{self, CliExit};

#[derive(Debug)]
pub struct InitOptions {
    pub name: String,
    pub namespace: Option<String>,
    pub force: bool,
}

#[derive(Clone, Copy)]
enum Opt {
    Name,
    Namespace,
    Force,
    Help,
}

impl Opt {
    const ALL: &[Self] = &[Self::Name, Self::Namespace, Self::Force, Self::Help];

    const fn spec(self) -> args::OptSpec {
        match self {
            Self::Name => args::OptSpec {
                long: Some("name"),
                short: None,
                value: Some("<name>"),
                desc: "Package name (required)",
            },
            Self::Namespace => args::OptSpec {
                long: Some("namespace"),
                short: None,
                value: Some("<ns>"),
                desc: "Package namespace (optional, required for publishing)",
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

fn format_usage() -> String {
    let mut buf = String::new();
    writeln!(buf, "Usage: wado init [options]").unwrap();
    writeln!(buf).unwrap();
    writeln!(
        buf,
        "Create a new wado.toml manifest in the current directory."
    )
    .unwrap();
    writeln!(buf).unwrap();
    writeln!(buf, "Options:").unwrap();
    write!(buf, "{}", args::format_opts_help(Opt::ALL, |o| o.spec())).unwrap();
    buf
}

pub fn parse_args(mut parser: lexopt::Parser) -> Result<InitOptions, CliExit> {
    let usage = format_usage();
    let mut name: Option<String> = None;
    let mut namespace: Option<String> = None;
    let mut force = false;

    while let Some(arg) = args::next_arg(&mut parser)? {
        if let Some(opt) = args::match_opt(&arg, Opt::ALL, |o| o.spec()) {
            match opt {
                Opt::Name => name = Some(args::require_string(&mut parser)?),
                Opt::Namespace => namespace = Some(args::require_string(&mut parser)?),
                Opt::Force => force = true,
                Opt::Help => return Err(CliExit::help(usage)),
            }
        } else if let Value(_) = arg {
            return Err(CliExit::error_with_usage(
                "unexpected positional argument",
                &usage,
            ));
        } else {
            return Err(args::unexpected_arg(arg, &usage));
        }
    }

    let name = name.ok_or_else(|| CliExit::error_with_usage("--name is required", &usage))?;

    Ok(InitOptions {
        name,
        namespace,
        force,
    })
}

pub fn run(opts: InitOptions) -> Result<(), CliExit> {
    let manifest_path = Path::new("wado.toml");

    if manifest_path.exists() && !opts.force {
        return Err(CliExit::error(
            "wado.toml already exists in the current directory (use --force to overwrite)",
        ));
    }

    let content = generate_manifest(&opts.name, opts.namespace.as_deref());

    fs::write(manifest_path, &content)
        .map_err(|e| CliExit::error(format!("writing wado.toml: {e}")))?;
    eprintln!("Created wado.toml");
    Ok(())
}

fn generate_manifest(name: &str, namespace: Option<&str>) -> String {
    let mut buf = String::new();
    buf.push_str("[package]\n");
    if let Some(ns) = namespace {
        writeln!(buf, "namespace = \"{ns}\"").unwrap();
    }
    writeln!(buf, "name = \"{name}\"").unwrap();
    writeln!(buf, "version = \"0.1.0\"").unwrap();
    writeln!(buf, "command = \"src/main.wado\"").unwrap();
    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_manifest_minimal() {
        let result = generate_manifest("my-app", None);
        assert!(result.contains("name = \"my-app\""));
        assert!(result.contains("version = \"0.1.0\""));
        assert!(result.contains("command = \"src/main.wado\""));
        assert!(!result.contains("namespace"));
    }

    #[test]
    fn generate_manifest_with_namespace() {
        let result = generate_manifest("my-app", Some("myorg"));
        assert!(result.contains("namespace = \"myorg\""));
        assert!(result.contains("name = \"my-app\""));
    }

    #[test]
    fn parse_args_requires_name() {
        let parser = lexopt::Parser::from_args(Vec::<String>::new());
        let err = parse_args(parser).unwrap_err();
        assert_eq!(err.exit_code, 1);
        assert!(err.message.contains("--name is required"));
    }

    #[test]
    fn parse_args_with_name() {
        let parser = lexopt::Parser::from_args(vec!["--name", "my-app"]);
        let opts = parse_args(parser).unwrap();
        assert_eq!(opts.name, "my-app");
        assert!(opts.namespace.is_none());
    }

    #[test]
    fn parse_args_with_namespace() {
        let parser = lexopt::Parser::from_args(vec!["--name", "my-app", "--namespace", "myorg"]);
        let opts = parse_args(parser).unwrap();
        assert_eq!(opts.name, "my-app");
        assert_eq!(opts.namespace.as_deref(), Some("myorg"));
    }
}
