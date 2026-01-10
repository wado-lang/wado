//! wado-from-wit CLI

use std::fs;
use std::io::{self, Read, Write};
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;
use wit_parser::Resolve;

use wado_from_wit::{Transformer, WadoCodeGenerator};

#[derive(Parser)]
#[command(name = "wado-from-wit")]
#[command(about = "Generate Wado standard library from WIT files")]
#[command(long_about = "Generate Wado standard library from WIT files.\n\n\
    Filter mode (default): Reads WIT from stdin, writes Wado to stdout.\n\
    Directory mode: Use --wit-dir and --output-dir to batch process files.")]
struct Cli {
    /// Directory containing WIT files (enables directory mode)
    #[arg(long, value_name = "DIR")]
    wit_dir: Option<PathBuf>,

    /// Output directory for generated Wado files (required with --wit-dir)
    #[arg(long, value_name = "DIR")]
    output_dir: Option<PathBuf>,

    /// Only generate for specific package (e.g., "cli", "filesystem")
    #[arg(long)]
    package: Option<String>,

    /// Package name for filter mode (default: "wasi")
    #[arg(long, default_value = "wasi")]
    package_name: String,

    /// Package version for filter mode (default: "0.0.0")
    #[arg(long, default_value = "0.0.0")]
    package_version: String,

    /// Skip @unstable items (by default, all items including unstable are included)
    #[arg(long)]
    skip_unstable: bool,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

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
    let input = if !input.contains("package ") {
        format!(
            "package {}:{}@{};\n\n{}",
            package_name, package_name, package_version, input
        )
    } else {
        input
    };

    // Parse WIT
    let mut resolve = Resolve::default();
    // Include @unstable items by default (unless --skip-unstable is specified)
    resolve.all_features = !skip_unstable;
    resolve
        .push_str("<stdin>", &input)
        .context("Failed to parse WIT")?;

    // Generate Wado for each interface
    let transformer = Transformer::new(&resolve);
    let mut generator = WadoCodeGenerator::new();

    for (iface_id, _iface) in resolve.interfaces.iter() {
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
) -> Result<()> {
    // Parse WIT files from directory
    let mut resolve = Resolve::default();
    // Include @unstable items by default (unless --skip-unstable is specified)
    resolve.all_features = !skip_unstable;
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

    // Process each package
    for (_current_pkg_id, pkg) in resolve.packages.iter() {
        let pkg_name = &pkg.name.name;

        // Filter by package if specified
        if package_filter.is_some_and(|filter| pkg_name != filter) {
            continue;
        }

        let version = pkg
            .name
            .version
            .as_ref()
            .map(|v| v.to_string())
            .unwrap_or_else(|| "0.0.0".to_string());

        // Create a single combined module for the entire package
        let mut combined_module = wado_from_wit::WadoModule::new(pkg_name.clone(), version);

        // Collect all source files
        let mut source_files: std::collections::HashSet<String> = std::collections::HashSet::new();

        // Process all interfaces
        for (iface_name, iface_id) in &pkg.interfaces {
            if let Some(source_file) = iface_to_file.get(iface_name) {
                source_files.insert(source_file.clone());
            }

            let module = transformer
                .transform_interface(*iface_id)
                .with_context(|| format!("Failed to transform interface {}", iface_name))?;

            // Merge module contents
            combined_module.types.extend(module.types);
            combined_module.resources.extend(module.resources);
            combined_module.effects.extend(module.effects);
        }

        // Process all worlds
        for (world_name, world_id) in &pkg.worlds {
            if let Some(source_file) = iface_to_file.get(world_name) {
                source_files.insert(source_file.clone());
            }

            let world = transformer
                .transform_world(*world_id)
                .with_context(|| format!("Failed to transform world {}", world_name))?;
            combined_module.worlds.push(world);
        }

        // Skip empty packages (no types, resources, effects, or worlds)
        if combined_module.types.is_empty()
            && combined_module.resources.is_empty()
            && combined_module.effects.is_empty()
            && combined_module.worlds.is_empty()
        {
            continue;
        }

        // Sort source files for consistent output
        let mut source_files: Vec<_> = source_files.into_iter().collect();
        source_files.sort();
        combined_module.source_files = source_files;

        let code = generator.generate(&combined_module);

        // Write combined file (e.g., wasi/cli.wado)
        let output_path = output_dir.join(format!("{}.wado", pkg_name));
        fs::write(&output_path, code)
            .with_context(|| format!("Failed to write {}", output_path.display()))?;

        eprintln!("Generated: {}", output_path.display());
    }

    Ok(())
}

use std::collections::HashMap;

/// Build a map from interface/world name to the WIT file that defines it
fn build_interface_to_file_map(dir: &PathBuf) -> Result<HashMap<String, String>> {
    let mut map = HashMap::new();
    let base_dir = std::env::current_dir()?;
    build_interface_map_recursive(dir, &base_dir, &mut map)?;
    Ok(map)
}

fn build_interface_map_recursive(
    dir: &PathBuf,
    base_dir: &PathBuf,
    map: &mut HashMap<String, String>,
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
                        } else if let Some(rest) = line.strip_prefix("world ") {
                            if let Some(name) = rest.split_whitespace().next() {
                                map.insert(name.to_string(), rel_path.clone());
                            }
                        }
                    }
                }
            }
        }
    }
    Ok(())
}
