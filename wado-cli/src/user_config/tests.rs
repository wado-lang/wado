use std::fs;
use std::path::Path;

use super::{load, parse};

#[test]
fn reads_the_root_and_the_process_read_paths() {
    let config = parse(
        Path::new("config.toml"),
        "root = \"/srv/wado\"\n[process]\nread = [\"/opt/jdk\", \"relative/jdk\"]\n",
    )
    .unwrap();
    assert_eq!(config.root.as_deref(), Some(Path::new("/srv/wado")));
    assert_eq!(config.process_read, [Path::new("/opt/jdk")]);
}

#[test]
fn an_empty_file_sets_nothing() {
    let config = parse(Path::new("config.toml"), "# empty\n").unwrap();
    assert_eq!(config.root, None);
    assert!(config.process_read.is_empty());
}

#[test]
fn a_malformed_file_is_an_error() {
    assert!(parse(Path::new("config.toml"), "root = 42\n").is_err());
    assert!(parse(Path::new("config.toml"), "not valid [").is_err());
}

#[test]
fn an_absent_path_is_skipped_when_resolved() {
    let dir = tempfile::tempdir().unwrap();
    let text = format!(
        "[process]\nread = [{:?}, {:?}]\n",
        dir.path().display().to_string(),
        dir.path().join("absent").display().to_string()
    );
    let config = parse(Path::new("config.toml"), &text).unwrap();
    assert_eq!(
        config.process_read(),
        [fs::canonicalize(dir.path()).unwrap()]
    );
}

#[cfg(unix)]
#[test]
fn a_file_others_can_write_is_ignored() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    fs::write(&path, "root = \"/srv/wado\"\n").unwrap();

    for mode in [0o644, 0o664] {
        fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
        assert!(load(&path).unwrap().root.is_some());
    }

    fs::set_permissions(&path, fs::Permissions::from_mode(0o646)).unwrap();
    let reason = load(&path).err().unwrap();
    assert!(reason.contains("other users can write it"), "{reason}");
}

#[test]
fn an_absent_file_sets_nothing() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(load(&dir.path().join("config.toml")).unwrap().root, None);
}
