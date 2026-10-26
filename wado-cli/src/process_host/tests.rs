use wado_host::sandbox::Trace;

use super::{Answer, ProcessGrant, answer, unsupported_reason};

fn grants(dirs: &[(&std::path::Path, &str)]) -> Answer {
    let dirs: Vec<(String, String)> = dirs
        .iter()
        .map(|(host, guest)| (host.display().to_string(), (*guest).to_owned()))
        .collect();
    answer(
        &dirs,
        ProcessGrant::Offered {
            read: Vec::new(),
            trace: Trace::Off,
        },
    )
    .unwrap()
}

#[test]
fn dot_names_the_childs_directory_only_as_the_first_grant() {
    if unsupported_reason().is_some() {
        return;
    }
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    let same = first.path().to_str().unwrap();
    assert!(matches!(
        grants(&[
            (first.path(), "."),
            (second.path(), second.path().to_str().unwrap())
        ]),
        Answer::Available(_)
    ));
    assert!(matches!(
        grants(&[(first.path(), same), (second.path(), ".")]),
        Answer::Denied(_)
    ));
}
