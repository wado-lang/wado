/// Track fixture directories so Cargo recompiles test crates when files are
/// added or removed. This complements the `include_bytes!` dependency tracking
/// in `datatest-mini` which handles content changes to existing files.
fn main() {
    track_fixture_dir("tests/fixtures");
    track_fixture_dir("tests/fixtures.golden");
}

fn track_fixture_dir(dir: &str) {
    let path = std::path::Path::new(dir);
    if !path.exists() {
        return;
    }
    println!("cargo:rerun-if-changed={dir}");
    visit_dir(path);
}

fn visit_dir(dir: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<_> = entries.filter_map(Result::ok).collect();
    entries.sort_by_key(std::fs::DirEntry::path);
    for entry in entries {
        let path = entry.path();
        println!("cargo:rerun-if-changed={}", path.display());
        if path.is_dir() {
            visit_dir(&path);
        }
    }
}
