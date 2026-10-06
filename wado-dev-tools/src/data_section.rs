use wado_host::fixture::{CompileInputs, data_section};

pub fn should_skip_file(source: &str) -> bool {
    // Skip if __DATA__ contains "compile_error"
    if data_section(source).is_some_and(|data| data.contains("\"compile_error\"")) {
        return true;
    }
    // Skip if module has #![TODO] attribute (may fail to compile)
    for line in source.lines() {
        let t = line.trim();
        if t.is_empty() || t.starts_with("//") {
            continue;
        }
        if t.starts_with("#![") {
            if t.contains("TODO") {
                return true;
            }
            continue;
        }
        break;
    }
    false
}

/// How a fixture's `__DATA__` says to compile it. Golden generation compiles a
/// fixture the way the e2e harness does, so it reads the same [`CompileInputs`].
#[derive(Debug)]
pub struct FixtureCompile {
    /// The world to compile for; `None` keeps the CLI default.
    pub world: Option<String>,
    /// What the host and the `#[param]` resolution are given.
    pub inputs: CompileInputs,
}

/// Read a fixture's `__DATA__` once, for both its world and its compile inputs.
///
/// `no_data_default` is the world used when the source has **no `__DATA__`
/// section at all** — the `tests/fixtures` e2e set passes `Some("test")` so a
/// library-shaped source runs under the test world (mirroring `run_fixture` in
/// e2e.rs), while the `format.fixtures` set passes `None` to keep the CLI
/// default. A `__DATA__` section that is present but carries no world key
/// always falls through to the CLI default (`None`).
///
/// # Errors
/// If the `__DATA__` section is not a JSON object of the fixture spec's shape.
pub fn read_fixture_compile(
    source: &str,
    no_data_default: Option<&str>,
) -> Result<FixtureCompile, String> {
    let Some(data) = data_section(source) else {
        return Ok(FixtureCompile {
            world: no_data_default.map(str::to_string),
            inputs: CompileInputs::default(),
        });
    };
    let json: serde_json::Value =
        serde_json::from_str(data.trim()).map_err(|e| format!("__DATA__ is not JSON: {e}"))?;
    let world = match json.get("world").and_then(|v| v.as_str()) {
        Some(world) => Some(world.to_string()),
        None => json.as_object().and_then(|obj| {
            obj.keys()
                .find(|key| key.starts_with("wasi:") || *key == "test")
                .cloned()
        }),
    };
    let inputs =
        serde_json::from_value(json).map_err(|e| format!("__DATA__ is not a fixture spec: {e}"))?;
    Ok(FixtureCompile { world, inputs })
}
