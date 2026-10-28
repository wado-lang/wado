#[cfg(unix)]
use std::collections::BTreeSet;
#[cfg(unix)]
use std::path::{Path, PathBuf};

use wado_host::sandbox::Trace;
#[cfg(unix)]
use wado_host::sandbox::{Access as Reach, Refusal};

#[cfg(unix)]
use super::posix::{refusals_warning, refused_warning};
#[cfg(unix)]
use super::types::ExitStatus;
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

/// A child a signal ended failed, so what the sandbox refused it may be why.
#[cfg(unix)]
#[test]
fn a_signaled_child_reports_what_the_sandbox_refused_it() {
    let refusals = BTreeSet::from([Refusal::LeaveGroup]);
    let warning = refusals_warning("job", &ExitStatus::Signaled(9), &refusals, Trace::Off);
    assert!(warning.is_some_and(|warning| {
        warning.contains("`job` was killed by signal 9 after the sandbox refused")
    }));
}

/// A write is granted on the directory the sandbox judged, which an
/// `O_TMPFILE` open names itself, the root included.
#[cfg(unix)]
#[test]
fn a_refused_write_is_granted_on_the_directory_judged() {
    let refusals = BTreeSet::from([Refusal::Path {
        path: PathBuf::from("/"),
        dir: PathBuf::from("/"),
        access: Reach::ReadWrite,
    }]);
    let warning = refusals_warning("job", &ExitStatus::Exited(1), &refusals, Trace::Off).unwrap();
    assert!(warning.contains("with `--dir /`"), "{warning}");
}

/// The spawn's help grants the file refused, as the trace's does, whatever
/// the guest named: the root has no directory above it.
#[cfg(unix)]
#[test]
fn a_refused_program_is_granted_as_the_file_refused() {
    let interpreter = Path::new("/opt/interp/bin/sh");
    let script = Path::new("/opt/tool");
    let warning = refused_warning("tool", script, script, interpreter);
    assert!(
        warning.contains("`--process-read /opt/interp/bin/sh`"),
        "{warning}"
    );
    let root = Path::new("/");
    assert!(refused_warning("x", root, root, root).contains("`--process-read /`"));
}
