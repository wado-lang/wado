pub fn should_skip_file(source: &str) -> bool {
    let data = source.find("\n__DATA__\n").map_or("", |p| &source[p..]);
    data.contains("\"TODO\"") || data.contains("\"compile_error\"")
}

pub fn extract_world_from_data_section(source: &str) -> Option<String> {
    let marker = "\n__DATA__\n";
    let data = if let Some(pos) = source.find(marker) {
        &source[pos + marker.len()..]
    } else if let Some(stripped) = source.strip_prefix("__DATA__\n") {
        stripped
    } else {
        return None;
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
