pub fn should_skip_file(source: &str) -> bool {
    // Skip if __DATA__ contains "compile_error"
    let data = source.find("\n__DATA__\n").map_or("", |p| &source[p..]);
    if data.contains("\"compile_error\"") {
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

/// Resolve a fixture's target world. `no_data_default` is the world used when
/// the source has **no `__DATA__` section at all** — the `tests/fixtures` e2e
/// set passes `Some("test")` so a library-shaped source runs under the test
/// world (mirroring `run_fixture` in e2e.rs), while the `format.fixtures` set
/// passes `None` to keep the CLI default. A `__DATA__` section that is present
/// but carries no world key always falls through to the CLI default (`None`).
pub fn extract_world_from_data_section(
    source: &str,
    no_data_default: Option<&str>,
) -> Option<String> {
    let marker = "\n__DATA__\n";
    let data = if let Some(pos) = source.find(marker) {
        &source[pos + marker.len()..]
    } else if let Some(stripped) = source.strip_prefix("__DATA__\n") {
        stripped
    } else {
        return no_data_default.map(str::to_string);
    };
    let json: serde_json::Value = serde_json::from_str(data.trim()).ok()?;
    if let Some(world) = json.get("world").and_then(|v| v.as_str()) {
        return Some(world.to_string());
    }
    if let Some(obj) = json.as_object() {
        for key in obj.keys() {
            if key.starts_with("wasi:") || key == "test" {
                return Some(key.clone());
            }
        }
    }
    None
}
