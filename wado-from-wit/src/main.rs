//! wado-from-wit CLI

use indexmap::{IndexMap, IndexSet};
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process;

use anyhow::{Context, Result};
use glob::glob;
use lexopt::Arg::Long;
use toml::Table;
use wit_parser::Resolve;

use wado_from_wit::{Transformer, WadoCodeGenerator};

// Helper functions for lexopt argument parsing

fn exit_error(msg: &str) -> ! {
    eprintln!("Error: {msg}");
    process::exit(1);
}

fn next_arg(parser: &mut lexopt::Parser) -> Option<lexopt::Arg<'_>> {
    match parser.next() {
        Ok(arg) => arg,
        Err(e) => exit_error(&e.to_string()),
    }
}

fn require_string(parser: &mut lexopt::Parser) -> String {
    parser
        .value()
        .unwrap_or_else(|e| exit_error(&e.to_string()))
        .to_string_lossy()
        .into_owned()
}

fn require_path(parser: &mut lexopt::Parser) -> PathBuf {
    PathBuf::from(
        parser
            .value()
            .unwrap_or_else(|e| exit_error(&e.to_string())),
    )
}

struct Cli {
    wit_dirs: Vec<PathBuf>,
    output_dir: Option<PathBuf>,
    package: Option<String>,
    package_name: String,
    package_version: String,
    skip_unstable: bool,
}

fn print_usage() {
    eprintln!("wado-from-wit - Generate Wado standard library from WIT files");
    eprintln!();
    eprintln!("Filter mode (default): Reads WIT from stdin, writes Wado to stdout.");
    eprintln!("Directory mode: Use --wit-dir and --output-dir to batch process files.");
    eprintln!();
    eprintln!("Usage: wado-from-wit [options]");
    eprintln!();
    eprintln!("Options:");
    eprintln!(
        "  --wit-dir <DIR>           Directory containing WIT files (enables directory mode)"
    );
    eprintln!(
        "  --output-dir <DIR>        Output directory for generated Wado files (required with --wit-dir)"
    );
    eprintln!(
        "  --package <NAME>          Only generate for specific package (e.g., \"cli\", \"filesystem\")"
    );
    eprintln!("  --package-name <NAME>     Package name for filter mode (default: \"wasi\")");
    eprintln!("  --package-version <VER>   Package version for filter mode (default: \"0.0.0\")");
    eprintln!("  --skip-unstable           Skip @unstable items");
    eprintln!("  --help                    Show this help message");
}

fn expand_wit_dir_glob(pattern: &str) -> Vec<PathBuf> {
    // Try glob expansion first
    let matches: Vec<PathBuf> = match glob(pattern) {
        Ok(paths) => paths
            .filter_map(|p| p.ok())
            .filter(|p| p.is_dir())
            .collect(),
        Err(_) => vec![],
    };
    if !matches.is_empty() {
        return matches;
    }
    // Fall back to treating it as a literal path
    vec![PathBuf::from(pattern)]
}

fn parse_args() -> Cli {
    let mut cli = Cli {
        wit_dirs: vec![],
        output_dir: None,
        package: None,
        package_name: "wasi".to_string(),
        package_version: "0.0.0".to_string(),
        skip_unstable: false,
    };

    let mut parser = lexopt::Parser::from_env();

    while let Some(arg) = next_arg(&mut parser) {
        match arg {
            Long("help") => {
                print_usage();
                process::exit(0);
            }
            Long("wit-dir") => {
                let pattern = require_string(&mut parser);
                let mut dirs = expand_wit_dir_glob(&pattern);
                if dirs.is_empty() {
                    dirs.push(PathBuf::from(&pattern));
                }
                cli.wit_dirs.extend(dirs);
            }
            Long("output-dir") => cli.output_dir = Some(require_path(&mut parser)),
            Long("package") => cli.package = Some(require_string(&mut parser)),
            Long("package-name") => cli.package_name = require_string(&mut parser),
            Long("package-version") => cli.package_version = require_string(&mut parser),
            Long("skip-unstable") => cli.skip_unstable = true,
            // Bare positional args are treated as extra wit-dir paths (from shell glob expansion)
            lexopt::Arg::Value(v) => {
                cli.wit_dirs.push(PathBuf::from(v));
            }
            _ => {
                eprintln!("Error: unexpected argument");
                print_usage();
                process::exit(1);
            }
        }
    }

    cli
}

fn main() -> Result<()> {
    let cli = parse_args();

    if !cli.wit_dirs.is_empty() {
        // Directory mode
        let output_dir = cli
            .output_dir
            .as_ref()
            .context("--output-dir is required when using --wit-dir")?;
        run_directory_mode(&cli.wit_dirs, output_dir, cli.package.as_deref(), cli.skip_unstable)
    } else {
        // Filter mode: stdin -> stdout
        run_filter_mode(&cli.package_name, &cli.package_version, cli.skip_unstable)
    }
}

fn run_filter_mode(package_name: &str, package_version: &str, skip_unstable: bool) -> Result<()> {
    // Read WIT from stdin
    let mut input = String::new();
    io::stdin()
        .read_to_string(&mut input)
        .context("Failed to read from stdin")?;

    // Prepend package declaration if not present
    let input = if input.contains("package ") {
        input
    } else {
        format!("package {package_name}:{package_name}@{package_version};\n\n{input}")
    };

    // Parse WIT
    // Include @unstable items by default (unless --skip-unstable is specified)
    let mut resolve = Resolve {
        all_features: !skip_unstable,
        ..Default::default()
    };
    resolve
        .push_str("<stdin>", &input)
        .context("Failed to parse WIT")?;

    // Generate Wado for each interface
    let transformer = Transformer::new(&resolve);
    let mut generator = WadoCodeGenerator::new();

    for (iface_id, _iface) in &resolve.interfaces {
        let mut module = transformer
            .transform_interface(iface_id)
            .context("Failed to transform interface")?;
        module.source_files = vec!["<stdin>".to_string()];
        let code = generator.generate(&module);
        io::stdout()
            .write_all(code.as_bytes())
            .context("Failed to write to stdout")?;
    }

    Ok(())
}

/// Parse a simple `deps.toml` file (key = "relative/path" lines) and return
/// the canonical absolute paths of declared dependency directories.
fn read_deps_toml(wit_dir: &Path) -> Result<Vec<PathBuf>> {
    let deps_toml = wit_dir.join("deps.toml");
    if !deps_toml.exists() {
        return Ok(vec![]);
    }
    let content = fs::read_to_string(&deps_toml)
        .with_context(|| format!("Failed to read {}", deps_toml.display()))?;
    let table: Table = content
        .parse()
        .with_context(|| format!("Failed to parse {}", deps_toml.display()))?;
    let mut deps = Vec::new();
    for (_key, val) in &table {
        if let Some(rel) = val.as_str() {
            let abs = wit_dir.join(rel);
            let canon = abs
                .canonicalize()
                .with_context(|| format!("Cannot resolve dep path: {}", abs.display()))?;
            deps.push(canon);
        }
    }
    Ok(deps)
}

/// Topologically sort `dirs` so each directory comes after its deps.toml dependencies.
fn topo_sort_wit_dirs(dirs: &[PathBuf]) -> Result<Vec<PathBuf>> {
    // Canonicalize all input dirs
    let canon_dirs: Vec<PathBuf> = dirs
        .iter()
        .map(|d| d.canonicalize().unwrap_or_else(|_| d.clone()))
        .collect();
    let dir_set: IndexSet<PathBuf> = canon_dirs.iter().cloned().collect();

    // Build adjacency: dir → deps that are also in dir_set
    let mut deps_of: IndexMap<PathBuf, Vec<PathBuf>> = IndexMap::new();
    for dir in &canon_dirs {
        let dep_paths = read_deps_toml(dir)?;
        let known_deps: Vec<PathBuf> = dep_paths
            .into_iter()
            .filter(|d| dir_set.contains(d))
            .collect();
        deps_of.insert(dir.clone(), known_deps);
    }

    // Kahn's algorithm
    let mut in_degree: IndexMap<PathBuf, usize> = canon_dirs
        .iter()
        .map(|d| (d.clone(), 0))
        .collect();
    let mut rdeps: IndexMap<PathBuf, Vec<PathBuf>> = IndexMap::new(); // dep → dirs that depend on it
    for (dir, deps) in &deps_of {
        for dep in deps {
            *in_degree.get_mut(dir).unwrap() += 1;
            rdeps.entry(dep.clone()).or_default().push(dir.clone());
        }
    }

    let mut queue: Vec<PathBuf> = in_degree
        .iter()
        .filter(|&(_, &deg)| deg == 0)
        .map(|(d, _)| d.clone())
        .collect();
    let mut sorted = Vec::new();
    while let Some(dir) = queue.pop() {
        sorted.push(dir.clone());
        if let Some(dependents) = rdeps.get(&dir) {
            for dep in dependents {
                let deg = in_degree.get_mut(dep).unwrap();
                *deg -= 1;
                if *deg == 0 {
                    queue.push(dep.clone());
                }
            }
        }
    }

    if sorted.len() != dirs.len() {
        anyhow::bail!("Cyclic dependencies detected among WIT directories");
    }
    Ok(sorted)
}

fn run_directory_mode(
    wit_dirs: &[PathBuf],
    output_dir: &PathBuf,
    package_filter: Option<&str>,
    skip_unstable: bool,
) -> Result<()> {
    // Sort directories so dependencies are loaded before dependents
    let sorted_dirs = topo_sort_wit_dirs(wit_dirs)?;

    // Parse WIT files from all directories into one shared Resolve so that
    // cross-package dependencies (e.g. wasi:cli depends on wasi:clocks) resolve.
    // Include @unstable items by default (unless --skip-unstable is specified)
    let mut resolve = Resolve {
        all_features: !skip_unstable,
        ..Default::default()
    };
    for wit_dir in &sorted_dirs {
        resolve
            .push_dir(wit_dir)
            .with_context(|| format!("Failed to parse WIT files from {}", wit_dir.display()))?;
    }

    // Build a combined map from interface/world name to source WIT file
    let mut iface_to_file: IndexMap<String, String> = IndexMap::new();
    for wit_dir in &sorted_dirs {
        let map = build_interface_to_file_map(wit_dir)?;
        iface_to_file.extend(map);
    }

    let transformer = Transformer::new(&resolve);
    let mut generator = WadoCodeGenerator::new();

    // Ensure output directory exists
    fs::create_dir_all(output_dir)
        .with_context(|| format!("Failed to create directory {}", output_dir.display()))?;

    // Process each package — generate one file per interface + one worlds file per package
    for (_current_pkg_id, pkg) in &resolve.packages {
        let pkg_name = &pkg.name.name;

        // Filter by package if specified
        if package_filter.is_some_and(|filter| pkg_name != filter) {
            continue;
        }

        let version = pkg
            .name
            .version
            .as_ref()
            .map_or_else(|| "0.0.0".to_string(), std::string::ToString::to_string);

        // Create a subdirectory for this package (e.g., wasi/filesystem/)
        let pkg_dir = output_dir.join(pkg_name.as_str());
        fs::create_dir_all(&pkg_dir)
            .with_context(|| format!("Failed to create directory {}", pkg_dir.display()))?;

        // Process all interfaces — one sub-file each, and collect re-export info for flat file
        // flat_reexports: iface_name → list of public names
        let mut flat_reexports: Vec<(String, Vec<String>)> = Vec::new();

        for (iface_name, iface_id) in &pkg.interfaces {
            let mut module = transformer
                .transform_interface(*iface_id)
                .with_context(|| format!("Failed to transform interface {iface_name}"))?;

            // Skip empty interfaces
            if module.types.is_empty()
                && module.resources.is_empty()
                && module.effects.is_empty()
                && module.imports.is_empty()
            {
                continue;
            }

            if let Some(source_file) = iface_to_file.get(iface_name) {
                module.source_files = vec![source_file.clone()];
            }
            module.package_name = version.clone();

            let names = module.public_names();
            if !names.is_empty() {
                flat_reexports.push((iface_name.clone(), names));
            }

            let code = generator.generate(&module);

            // Write per-interface file (e.g., wasi/filesystem/types.wado)
            let output_path = pkg_dir.join(format!("{iface_name}.wado"));
            fs::write(&output_path, code)
                .with_context(|| format!("Failed to write {}", output_path.display()))?;

            eprintln!("Generated: {}", output_path.display());
        }

        // Collect all world source files and emit worlds file
        let mut worlds_module = wado_from_wit::WadoModule::new(pkg_name.clone(), version.clone());
        let mut worlds_source_files: IndexSet<String> = IndexSet::new();

        for (world_name, world_id) in &pkg.worlds {
            if let Some(source_file) = iface_to_file.get(world_name) {
                worlds_source_files.insert(source_file.clone());
            }

            let world = transformer
                .transform_world(*world_id)
                .with_context(|| format!("Failed to transform world {world_name}"))?;
            worlds_module.worlds.push(world);
        }

        if !worlds_module.worlds.is_empty() {
            let mut source_files: Vec<_> = worlds_source_files.into_iter().collect();
            source_files.sort();
            worlds_module.source_files = source_files;

            let code = generator.generate(&worlds_module);

            let output_path = pkg_dir.join("worlds.wado");
            fs::write(&output_path, code)
                .with_context(|| format!("Failed to write {}", output_path.display()))?;

            eprintln!("Generated: {}", output_path.display());
        }

        // Generate flat package-level re-exporting file (e.g., wasi/filesystem.wado)
        // This re-exports all types from all sub-interface files for backward compatibility.
        if !flat_reexports.is_empty() {
            let mut flat_code = String::new();
            flat_code.push_str(
                "// This file is auto-generated by wado-from-wit. DO NOT EDIT MANUALLY.\n",
            );
            flat_code.push('\n');

            // Track already-exported names to avoid duplicate re-exports across interfaces
            let mut exported_names: IndexSet<String> = IndexSet::new();
            for (iface_name, names) in &flat_reexports {
                let new_names: Vec<&String> = names
                    .iter()
                    .filter(|n| !exported_names.contains(*n))
                    .collect();
                if new_names.is_empty() {
                    continue;
                }
                let names_str = new_names
                    .iter()
                    .map(|n| n.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                flat_code.push_str(&format!(
                    "pub use {{ {names_str} }} from \"wasi:{pkg_name}/{iface_name}.wado\";\n"
                ));
                for n in new_names {
                    exported_names.insert(n.clone());
                }
            }

            // Note: worlds are NOT re-exported from the flat file.
            // The WorldRegistry reads them directly from pkg/worlds.wado via ALL_WASI_MODULES.

            let output_path = output_dir.join(format!("{pkg_name}.wado"));
            fs::write(&output_path, flat_code)
                .with_context(|| format!("Failed to write {}", output_path.display()))?;

            eprintln!("Generated: {}", output_path.display());
        }
    }

    Ok(())
}

/// Build a map from interface/world name to the WIT file that defines it
fn build_interface_to_file_map(dir: &Path) -> Result<IndexMap<String, String>> {
    let mut map = IndexMap::new();
    let base_dir = std::env::current_dir()?;
    build_interface_map_recursive(dir, &base_dir, &mut map)?;
    Ok(map)
}

fn build_interface_map_recursive(
    dir: &Path,
    base_dir: &Path,
    map: &mut IndexMap<String, String>,
) -> Result<()> {
    if dir.is_dir() {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                build_interface_map_recursive(&path, base_dir, map)?;
            } else if path.extension().is_some_and(|ext| ext == "wit") {
                let rel_path = path
                    .strip_prefix(base_dir)
                    .unwrap_or(&path)
                    .display()
                    .to_string();

                // Parse the file to find interface/world definitions
                if let Ok(content) = fs::read_to_string(&path) {
                    for line in content.lines() {
                        let line = line.trim();
                        // Match "interface <name> {" or "world <name> {"
                        if let Some(rest) = line.strip_prefix("interface ") {
                            if let Some(name) = rest.split_whitespace().next() {
                                map.insert(name.to_string(), rel_path.clone());
                            }
                        } else if let Some(rest) = line.strip_prefix("world ")
                            && let Some(name) = rest.split_whitespace().next()
                        {
                            map.insert(name.to_string(), rel_path.clone());
                        }
                    }
                }
            }
        }
    }
    Ok(())
}
