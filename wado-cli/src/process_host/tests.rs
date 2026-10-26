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

/// A child the host killed at its timeout failed, so what the sandbox refused
/// it may be why it hung.
#[cfg(unix)]
#[test]
fn a_timed_out_child_reports_what_the_sandbox_refused_it() {
    use std::collections::BTreeSet;

    use wado_host::sandbox::Refusal;

    use super::posix::refusals_warning;
    use super::types::ExitStatus;

    let refusals = BTreeSet::from([Refusal::LeaveGroup]);
    let warning = refusals_warning("job", &ExitStatus::TimedOut, &refusals, Trace::Off);
    assert!(
        warning
            .is_some_and(|warning| warning.contains("`job` timed out after the sandbox refused"))
    );
}
