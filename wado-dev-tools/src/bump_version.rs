//! `bump-version` subcommand.
//!
//! Updates `[workspace.package].version` in the root `Cargo.toml`. Run from
//! the workspace root (the directory containing the workspace `Cargo.toml`).
//!
//! Modes:
//!   wado-dev-tools bump-version <X.Y.Z>      — set explicit version
//!   wado-dev-tools bump-version --bump major — increment MAJOR (resets MINOR, PATCH)
//!   wado-dev-tools bump-version --bump minor — increment MINOR (resets PATCH)
//!   wado-dev-tools bump-version --bump patch — increment PATCH
//!   wado-dev-tools bump-version --check <X.Y.Z>
//!       — exit 0 if [workspace.package].version equals X.Y.Z, otherwise non-zero
//!   wado-dev-tools bump-version --show
//!       — print [workspace.package].version to stdout

use std::path::Path;
use std::process;

use lexopt::Arg::{Long, Value};

const MANIFEST: &str = "Cargo.toml";

pub fn run(mut parser: lexopt::Parser) {
    let mut bump_kind: Option<String> = None;
    let mut check: bool = false;
    let mut show: bool = false;
    let mut positional: Option<String> = None;

    while let Some(arg) = parser.next().expect("failed to parse args") {
        match arg {
            Long("bump") => {
                bump_kind = Some(parser.value().unwrap().to_string_lossy().into_owned());
            }
            Long("check") => {
                check = true;
            }
            Long("show") => {
                show = true;
            }
            Value(v) if positional.is_none() => {
                positional = Some(v.to_string_lossy().into_owned());
            }
            _ => panic!("unexpected argument: {arg:?}"),
        }
    }

    // Mode flags are mutually exclusive — silently ignoring extras would let
    // a misuse like `--show 1.0.0` or `--check 1.0.0 --bump major` look
    // successful while doing the wrong thing in CI.
    if show && (check || bump_kind.is_some() || positional.is_some()) {
        panic!("--show takes no other arguments");
    }
    if check && bump_kind.is_some() {
        panic!("--check is incompatible with --bump");
    }

    if show {
        let current = read_workspace_version();
        println!("{current}");
        return;
    }

    if check {
        let current = read_workspace_version();
        let expected = positional.expect("usage: bump-version --check <X.Y.Z>");
        validate_semver(&expected);
        if current != expected {
            eprintln!(
                "version mismatch: [workspace.package].version = {current}, expected {expected}"
            );
            process::exit(1);
        }
        return;
    }

    let new_version = match (bump_kind, positional) {
        (Some(_), Some(_)) => {
            panic!("specify either <X.Y.Z> or --bump <kind>, not both");
        }
        (Some(kind), None) => bump(&read_workspace_version(), &kind),
        (None, Some(v)) => v,
        (None, None) => {
            panic!(
                "usage: bump-version <X.Y.Z> | --bump <major|minor|patch> | --check <X.Y.Z> | --show"
            );
        }
    };
    validate_semver(&new_version);

    let original =
        std::fs::read_to_string(MANIFEST).unwrap_or_else(|e| panic!("read {MANIFEST}: {e}"));
    let updated = replace_workspace_version(&original, &new_version);
    if updated == original {
        let current = read_workspace_version_from(&original);
        if current == new_version {
            eprintln!("[workspace.package].version is already {new_version}");
            return;
        }
        panic!("[workspace.package].version not found in {MANIFEST}");
    }
    std::fs::write(MANIFEST, updated).unwrap_or_else(|e| panic!("write {MANIFEST}: {e}"));
    eprintln!("bumped [workspace.package].version → {new_version}");
}

fn read_workspace_version() -> String {
    let manifest = std::fs::read_to_string(MANIFEST)
        .unwrap_or_else(|e| panic!("read {MANIFEST}: {e} (run from workspace root)"));
    read_workspace_version_from(&manifest)
}

fn read_workspace_version_from(src: &str) -> String {
    let mut in_target = false;
    for line in src.lines() {
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix('[')
            && let Some(name) = rest.split(']').next()
        {
            in_target = name.trim() == "workspace.package";
            continue;
        }
        if in_target
            && let Some(rest) = trimmed.strip_prefix("version")
            && rest.trim_start().starts_with('=')
        {
            let after_eq = rest.trim_start().trim_start_matches('=').trim();
            let v = after_eq
                .trim_start_matches('"')
                .split('"')
                .next()
                .unwrap_or("");
            assert!(
                !v.is_empty(),
                "[workspace.package].version is empty in {MANIFEST}"
            );
            return v.to_string();
        }
    }
    panic!("[workspace.package].version not found in {MANIFEST}");
}

fn replace_workspace_version(src: &str, new_version: &str) -> String {
    let mut out = String::with_capacity(src.len());
    let mut in_target = false;
    let mut replaced = false;
    let trailing_newline = src.ends_with('\n');

    for line in src.lines() {
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix('[')
            && let Some(name) = rest.split(']').next()
        {
            in_target = name.trim() == "workspace.package";
            out.push_str(line);
            out.push('\n');
            continue;
        }
        if in_target
            && !replaced
            && let Some(rest) = trimmed.strip_prefix("version")
            && rest.trim_start().starts_with('=')
        {
            let indent = &line[..line.len() - trimmed.len()];
            out.push_str(indent);
            out.push_str(&format!("version = \"{new_version}\"\n"));
            replaced = true;
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }

    if !trailing_newline && out.ends_with('\n') {
        out.pop();
    }
    out
}

fn bump(current: &str, kind: &str) -> String {
    let parts = parse_semver(current);
    let (major, minor, patch) = match kind {
        "major" => (parts.0 + 1, 0, 0),
        "minor" => (parts.0, parts.1 + 1, 0),
        "patch" => (parts.0, parts.1, parts.2 + 1),
        other => panic!("unknown --bump kind: {other} (expected major, minor, or patch)"),
    };
    format!("{major}.{minor}.{patch}")
}

fn parse_semver(v: &str) -> (u64, u64, u64) {
    let parts: Vec<&str> = v.split('.').collect();
    assert!(
        parts.len() == 3,
        "version {v:?} must be MAJOR.MINOR.PATCH (no pre-release / build metadata)"
    );
    let major = parts[0]
        .parse::<u64>()
        .unwrap_or_else(|_| panic!("invalid MAJOR in {v:?}"));
    let minor = parts[1]
        .parse::<u64>()
        .unwrap_or_else(|_| panic!("invalid MINOR in {v:?}"));
    let patch = parts[2]
        .parse::<u64>()
        .unwrap_or_else(|_| panic!("invalid PATCH in {v:?}"));
    (major, minor, patch)
}

fn validate_semver(v: &str) {
    let _ = parse_semver(v);
}

#[allow(dead_code)]
fn assert_workspace_root() {
    assert!(
        Path::new(MANIFEST).is_file(),
        "{MANIFEST} not found in current directory; run from workspace root"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"[workspace]
members = ["a", "b"]

[workspace.package]
version = "0.1.0"
license = "MIT"

[workspace.dependencies]
serde = "1"
"#;

    #[test]
    fn replaces_workspace_package_version() {
        let updated = replace_workspace_version(SAMPLE, "0.2.0");
        assert!(updated.contains("version = \"0.2.0\""));
        assert!(!updated.contains("version = \"0.1.0\""));
        assert!(updated.contains("[workspace.dependencies]"));
        assert!(updated.contains("license = \"MIT\""));
    }

    #[test]
    fn does_not_touch_non_target_sections() {
        let src = r#"[workspace]
version = "ignored"

[workspace.package]
version = "0.1.0"
"#;
        let updated = replace_workspace_version(src, "0.9.9");
        assert!(updated.contains("[workspace]\nversion = \"ignored\""));
        assert!(updated.contains("version = \"0.9.9\""));
    }

    #[test]
    fn reads_version() {
        assert_eq!(read_workspace_version_from(SAMPLE), "0.1.0");
    }

    #[test]
    fn bump_minor_resets_patch() {
        assert_eq!(bump("0.1.5", "minor"), "0.2.0");
        assert_eq!(bump("1.2.3", "major"), "2.0.0");
        assert_eq!(bump("0.1.0", "patch"), "0.1.1");
    }

    #[test]
    fn preserves_indentation() {
        let src = "[workspace.package]\n    version = \"0.1.0\"\n";
        let updated = replace_workspace_version(src, "1.0.0");
        assert_eq!(updated, "[workspace.package]\n    version = \"1.0.0\"\n");
    }

    #[test]
    #[should_panic(expected = "MAJOR.MINOR.PATCH")]
    fn rejects_pre_release() {
        validate_semver("0.1.0-alpha.1");
    }

    #[test]
    #[should_panic(expected = "MAJOR.MINOR.PATCH")]
    fn rejects_two_components() {
        validate_semver("0.1");
    }
}
