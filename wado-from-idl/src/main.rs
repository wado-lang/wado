//! wado-from-idl CLI

use indexmap::{IndexMap, IndexSet};
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process;

use anyhow::{Context, Result};
use lexopt::Arg::Long;
use wit_parser::Resolve;

use wado_from_idl::{Transformer, WadoCodeGenerator};

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
    wit_dir: Option<PathBuf>,
    output_dir: Option<PathBuf>,
    package: Option<String>,
    package_name: String,
    package_version: String,
    skip_unstable: bool,
    /// Skip writing the package-level flat re-export file
    /// (`<pkg>.wado`). Use when the caller provides a hand-written facade.
    skip_flat_reexport: bool,
}

fn print_usage() {
    eprintln!("wado-from-idl - Generate Wado standard library from IDL files (WIT, WebIDL)");
    eprintln!();
    eprintln!("Filter mode (default): Reads WIT from stdin, writes Wado to stdout.");
    eprintln!("Directory mode: Use --wit-dir and --output-dir to batch process files.");
    eprintln!();
    eprintln!("Usage: wado-from-idl [options]");
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
    eprintln!("  --skip-flat-reexport      Do not write the per-package flat re-export file");
    eprintln!("  --help                    Show this help message");
}

fn parse_args() -> Cli {
    let mut cli = Cli {
        wit_dir: None,
        output_dir: None,
        package: None,
        package_name: "wasi".to_string(),
        package_version: "0.0.0".to_string(),
        skip_unstable: false,
        skip_flat_reexport: false,
    };

    let mut parser = lexopt::Parser::from_env();

    while let Some(arg) = next_arg(&mut parser) {
        match arg {
            Long("help") => {
                print_usage();
                process::exit(0);
            }
            Long("wit-dir") => cli.wit_dir = Some(require_path(&mut parser)),
            Long("output-dir") => cli.output_dir = Some(require_path(&mut parser)),
            Long("package") => cli.package = Some(require_string(&mut parser)),
            Long("package-name") => cli.package_name = require_string(&mut parser),
            Long("package-version") => cli.package_version = require_string(&mut parser),
            Long("skip-unstable") => cli.skip_unstable = true,
            Long("skip-flat-reexport") => cli.skip_flat_reexport = true,
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

    if let Some(ref wit_dir) = cli.wit_dir {
        // Directory mode
        let output_dir = cli
            .output_dir
            .as_ref()
            .context("--output-dir is required when using --wit-dir")?;
        run_directory_mode(
            wit_dir,
            output_dir,
            cli.package.as_deref(),
            cli.skip_unstable,
            cli.skip_flat_reexport,
        )
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

fn run_directory_mode(
    wit_dir: &PathBuf,
    output_dir: &PathBuf,
    package_filter: Option<&str>,
    skip_unstable: bool,
    skip_flat_reexport: bool,
) -> Result<()> {
    // Parse WIT files from directory
    // Include @unstable items by default (unless --skip-unstable is specified)
    let mut resolve = Resolve {
        all_features: !skip_unstable,
        ..Default::default()
    };
    let (_pkg_id, _) = resolve
        .push_dir(wit_dir)
        .with_context(|| format!("Failed to parse WIT files from {}", wit_dir.display()))?;

    // Build a map from interface/world name to source WIT file
    let iface_to_file = build_interface_to_file_map(wit_dir)?;

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
            module.package_name.clone_from(&version);

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
        let mut worlds_module = wado_from_idl::WadoModule::new(pkg_name.clone(), version.clone());
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

        if !flat_reexports.is_empty() && !skip_flat_reexport {
            write_flat_reexport_file(output_dir, &pkg.name.namespace, pkg_name, &flat_reexports)?;
        }
    }

    Ok(())
}

/// Generate a flat package-level re-exporting file (e.g., wasi/filesystem.wado,
/// core/kiln.wado). Re-exports all types from sub-interface files so consumers
/// import a single module (`wasi:filesystem`, `core:kiln`) rather than individual
/// sub-interfaces.
fn write_flat_reexport_file(
    output_dir: &Path,
    namespace: &str,
    pkg_name: &str,
    flat_reexports: &[(String, Vec<String>)],
) -> Result<()> {
    use std::fmt::Write;

    let mut flat_code = String::new();
    flat_code.push_str("#![generated(by = \"wado-from-idl\")]\n");
    flat_code.push('\n');

    let mut exported_names: IndexSet<String> = IndexSet::new();
    for (iface_name, names) in flat_reexports {
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
        writeln!(
            flat_code,
            "pub use {{ {names_str} }} from \"{namespace}:{pkg_name}/{iface_name}.wado\";"
        )
        .unwrap();
        for n in new_names {
            exported_names.insert(n.clone());
        }
    }

    let output_path = output_dir.join(format!("{pkg_name}.wado"));
    fs::write(&output_path, flat_code)
        .with_context(|| format!("Failed to write {}", output_path.display()))?;
    eprintln!("Generated: {}", output_path.display());
    Ok(())
}

/// Build a map from interface/world name to the WIT file that defines it
fn build_interface_to_file_map(dir: &PathBuf) -> Result<IndexMap<String, String>> {
    let mut map = IndexMap::new();
    let base_dir = std::env::current_dir()?;
    build_interface_map_recursive(dir, &base_dir, &mut map)?;
    Ok(map)
}

fn build_interface_map_recursive(
    dir: &PathBuf,
    base_dir: &PathBuf,
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
