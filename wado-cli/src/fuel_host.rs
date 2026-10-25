//! `wado test`'s `core:fuel` host: what each test store reports through it,
//! and `build/fuel.json`, the run's report of it under `--report-fuel`.

use std::path::Path;

use serde::Serialize;
use wasmtime::StoreContextMut;
use wasmtime::component::Linker;

use crate::args::CliExit;
use crate::compile::build_dir;
use crate::runtime::{WasiState, fuel_spent};
use crate::test::TestResult;

/// A region a test measured with `core:fuel`'s `measure`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FuelRegion {
    pub label: String,
    pub fuel: u64,
}

/// Link `core:fuel` into a test linker. The host functions read the store's
/// fuel, which a bindgen `Host` cannot reach, so they are wrapped by hand; the
/// linker checks them against the component's import at instantiation.
///
/// # Errors
///
/// Returns an error if the linker already defines the interface.
pub fn add_to_linker(linker: &mut Linker<WasiState>) -> wasmtime::Result<()> {
    let mut host = linker.instance("core:fuel/fuel-host@0.1.0")?;
    host.func_wrap("spent", |store: StoreContextMut<'_, WasiState>, (): ()| {
        Ok((fuel_spent(&store),))
    })?;
    host.func_wrap(
        "report",
        |mut store: StoreContextMut<'_, WasiState>, (label, fuel): (String, u64)| {
            store.data_mut().fuel_regions().push(FuelRegion { label, fuel });
            Ok(())
        },
    )
}

/// One metered test in `build/fuel.json`.
#[derive(Serialize)]
struct FuelEntry<'a> {
    file: &'a str,
    test: &'a str,
    fuel: u64,
    regions: &'a [FuelRegion],
}

/// Write `build/fuel.json` under `root`: every metered test of the run, by
/// file and then by test, so one program writes one report.
///
/// # Errors
///
/// Returns an error if the report cannot be written.
pub(crate) fn write_report(root: &Path, results: &[TestResult]) -> Result<(), CliExit> {
    let mut entries: Vec<FuelEntry<'_>> = results
        .iter()
        .filter_map(|result| {
            Some(FuelEntry {
                file: &result.file_path,
                test: &result.display_name,
                fuel: result.fuel?,
                regions: &result.fuel_regions,
            })
        })
        .collect();
    entries.sort_by(|a, b| (a.file, a.test).cmp(&(b.file, b.test)));
    let dir = build_dir(root);
    let path = dir.join("fuel.json");
    let write = |e: &dyn std::fmt::Display| {
        CliExit::error(format!("failed to write {}: {e}", path.display()))
    };
    std::fs::create_dir_all(&dir).map_err(|e| write(&e))?;
    let mut json = serde_json::to_string_pretty(&entries).map_err(|e| write(&e))?;
    json.push('\n');
    std::fs::write(&path, json).map_err(|e| write(&e))?;
    eprintln!("Fuel report written to {}", path.display());
    Ok(())
}
