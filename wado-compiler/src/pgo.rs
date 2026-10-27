//! The profile `wado test --pgo` records: how often each region of each
//! measured function ran, in a text file the package commits. See
//! [WEP: Profile-Guided Optimization](../../docs/wep-2026-10-25-profile-guided-optimization.md).
//!
//! The format is the specification's,
//! [Profiles](../../docs/spec-testing.md#profiles). A `module` line names a
//! module as [`Coverage::files`] keys it.

use std::fmt::Write as _;

use crate::coverage::Coverage;

/// The first line of every profile, naming its format.
pub const HEADER: &str = "wado-pgo 1";

/// `coverage` as a profile, with its files keyed by the names a profile gives
/// modules.
#[must_use]
pub fn to_text(coverage: &Coverage) -> String {
    let mut out = format!("{HEADER}\n");
    for (module, file) in &coverage.files {
        let _ = writeln!(out, "module {module}");
        let regions = file.plan.regions_by_function();
        for (function, ids) in file.plan.functions.iter().zip(&regions) {
            let _ = writeln!(
                out,
                "fn {:016x} {} {}",
                function.hash, function.line, function.name
            );
            for (index, &id) in ids.iter().enumerate() {
                let count = file.count(id);
                if count > 0 {
                    let _ = writeln!(out, "{index} {count}");
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::coverage::{ModulePlan, PlannedFunction, Pos, Region, RegionKind};

    fn region(kind: RegionKind, function: u32, line: u32) -> Region {
        Region {
            kind,
            start: Pos { line, column: 1 },
            end: Pos { line, column: 9 },
            function,
            choice: None,
            derived: Vec::new(),
        }
    }

    #[test]
    fn a_profile_lists_every_function_and_the_regions_that_ran() {
        let plan = ModulePlan {
            path: "a.wado".to_string(),
            functions: vec![
                PlannedFunction {
                    name: "f".to_string(),
                    line: 1,
                    region: 0,
                    hash: 0xab,
                },
                PlannedFunction {
                    name: "g".to_string(),
                    line: 5,
                    region: 2,
                    hash: 0xcd,
                },
            ],
            regions: vec![
                region(RegionKind::Function, 0, 1),
                region(RegionKind::Then, 0, 2),
                region(RegionKind::Function, 1, 5),
            ],
            lines: Vec::new(),
        };
        let mut coverage = Coverage::default();
        let registered = coverage
            .register(&vec![(plan, 0)], |_| "src/a.wado".to_string())
            .expect("one plan");
        coverage.record(&registered, &BTreeMap::from([(0, 3)]), None);
        coverage.record(&registered, &BTreeMap::from([(0, 2), (1, 2)]), None);
        assert_eq!(
            to_text(&coverage),
            "wado-pgo 1\n\
             module src/a.wado\n\
             fn 00000000000000ab 1 f\n\
             0 5\n\
             1 2\n\
             fn 00000000000000cd 5 g\n"
        );
    }
}
