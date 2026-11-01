//! The profile `wado test --pgo` records: how often each region of each
//! measured function ran, in a text file the package commits. A compile given
//! one marks the regions that never ran cold. See
//! [WEP: Profile-Guided Optimization](../../docs/wep-2026-10-25-profile-guided-optimization.md).
//!
//! The format is the specification's,
//! [Profiles](../../docs/spec-testing.md#profiles). A `module` line is a
//! [`Coverage::files`] key, which `--pgo` makes the module's name.

use std::collections::BTreeMap;
use std::fmt::{self, Write as _};

use crate::ast::{self, AstId};
use crate::coverage::{Coverage, Marker, PlannedModule, ProbeSite, RegionKind, plan_module};
use crate::hashmap::{IndexMap, IndexSet};
use crate::module_source::ModuleSource;
use crate::stdlib::core_module_file;
use crate::token::Span;

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

/// A profile read back: each module's functions, by the name [`to_text`]
/// gives the module.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Profile {
    modules: BTreeMap<String, Vec<ProfiledFunction>>,
}

/// What a profile holds for one function.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ProfiledFunction {
    name: String,
    hash: u64,
    /// The runs of each region that ran, by its number within the function.
    counts: BTreeMap<u32, i64>,
}

impl Profile {
    /// The part naming `core:` modules: what the stdlib snapshot applies.
    #[must_use]
    pub(crate) fn stdlib_part(&self) -> Self {
        Self {
            modules: self
                .modules
                .iter()
                .filter(|(name, _)| core_module_file(name).is_some())
                .map(|(name, functions)| (name.clone(), functions.clone()))
                .collect(),
        }
    }

    #[must_use]
    pub(crate) fn is_empty(&self) -> bool {
        self.modules.is_empty()
    }

    /// This profile read against its own summary under `cutoffs`.
    #[must_use]
    pub fn applied(&self, cutoffs: Cutoffs) -> Applied<'_> {
        Applied {
            profile: self,
            summary: Summary::of(self, cutoffs),
        }
    }

    fn counts(&self) -> impl Iterator<Item = i64> + '_ {
        self.modules
            .values()
            .flatten()
            .flat_map(|function| function.counts.values().copied())
    }
}

/// The shares of all runs, in parts per million, that place the hot and the
/// cold line. The defaults are LLVM's for its options of the same names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cutoffs {
    pub hot: u32,
    pub cold: u32,
}

impl Cutoffs {
    pub const PER_MILLION: u32 = 1_000_000;
}

impl Default for Cutoffs {
    fn default() -> Self {
        Self {
            hot: 990_000,
            cold: 999_999,
        }
    }
}

/// Where a profile's counts turn hot and cold. With every count sorted from
/// the largest down, a line is the smallest count still inside its cutoff's
/// share of all runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Summary {
    hot: i64,
    cold: i64,
}

impl Summary {
    fn of(profile: &Profile, cutoffs: Cutoffs) -> Self {
        assert!(
            cutoffs.hot <= Cutoffs::PER_MILLION && cutoffs.cold <= Cutoffs::PER_MILLION,
            "a cutoff is a share of all runs: {cutoffs:?}"
        );
        let mut counts: Vec<i64> = profile.counts().collect();
        counts.sort_unstable_by(|a, b| b.cmp(a));
        let total: u128 = counts.iter().map(|&count| count as u128).sum();
        let line = |cutoff: u32| {
            let share = (total * u128::from(cutoff)).div_ceil(u128::from(Cutoffs::PER_MILLION));
            let mut runs = 0;
            counts
                .iter()
                .copied()
                .find(|&count| {
                    runs += count as u128;
                    runs >= share
                })
                .unwrap_or(i64::MAX)
        };
        Self {
            hot: line(cutoffs.hot),
            cold: line(cutoffs.cold),
        }
    }

    fn is_hot(self, count: i64) -> bool {
        count >= self.hot
    }

    /// Hot is asked first, as LLVM's inliner does, so a count on both lines
    /// is hot.
    fn is_cold(self, count: i64) -> bool {
        !self.is_hot(count) && count <= self.cold
    }
}

/// A profile a compile applies, with the summary its counts are read against.
/// The summary is the whole profile's, wherever a part of it is applied.
#[derive(Debug, Clone, Copy)]
pub struct Applied<'a> {
    pub(crate) profile: &'a Profile,
    pub(crate) summary: Summary,
}

/// Why a profile's text does not parse, at its 1-based `line`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub line: usize,
    pub reason: String,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "line {}: {}", self.line, self.reason)
    }
}

/// Read a profile [`to_text`] wrote.
///
/// # Errors
///
/// Returns the first line that is not what the format allows there.
pub fn parse(text: &str) -> Result<Profile, ParseError> {
    let error = |line: usize, reason: &str| ParseError {
        line,
        reason: reason.to_string(),
    };
    let mut lines = text
        .lines()
        .enumerate()
        .map(|(index, line)| (index + 1, line));
    if lines.next().map(|(_, line)| line) != Some(HEADER) {
        return Err(error(1, &format!("a profile starts with `{HEADER}`")));
    }
    let mut modules: Vec<(String, Vec<ProfiledFunction>)> = Vec::new();
    for (number, line) in lines {
        if let Some(name) = line.strip_prefix("module ") {
            if modules.iter().any(|(listed, _)| listed == name) {
                return Err(error(number, "a module is listed once"));
            }
            modules.push((name.to_string(), Vec::new()));
        } else if let Some(rest) = line.strip_prefix("fn ") {
            let function = parse_function(rest)
                .ok_or_else(|| error(number, "a function is `fn <hash> <line> <name>`"))?;
            modules
                .last_mut()
                .ok_or_else(|| error(number, "a function follows a `module` line"))?
                .1
                .push(function);
        } else {
            let (region, runs) = parse_count(line)
                .ok_or_else(|| error(number, "a count is `<region> <runs>`, runs above zero"))?;
            let function = modules
                .last_mut()
                .and_then(|(_, functions)| functions.last_mut())
                .ok_or_else(|| error(number, "a count follows a `fn` line"))?;
            if function.counts.insert(region, runs).is_some() {
                return Err(error(number, "a region is counted once"));
            }
        }
    }
    Ok(Profile {
        modules: modules.into_iter().collect(),
    })
}

fn parse_function(rest: &str) -> Option<ProfiledFunction> {
    let mut parts = rest.splitn(3, ' ');
    let hash = parts.next().filter(|hash| hash.len() == 16)?;
    parts.next()?.parse::<u32>().ok()?;
    let name = parts.next().filter(|name| !name.is_empty())?;
    Some(ProfiledFunction {
        name: name.to_string(),
        hash: u64::from_str_radix(hash, 16).ok()?,
        counts: BTreeMap::new(),
    })
}

fn parse_count(line: &str) -> Option<(u32, i64)> {
    let (region, runs) = line.split_once(' ')?;
    let runs = runs.parse::<i64>().ok().filter(|&runs| runs > 0)?;
    Some((region.parse().ok()?, runs))
}

/// Where a profile marks a compile's code cold, and the remarks applying it
/// makes.
#[derive(Debug, Clone, Default)]
pub(crate) struct ColdMap {
    /// The sites reify puts a cold marker at.
    sites: IndexSet<(ProbeSite, AstId)>,
    /// The `?` nodes whose failure path reify leaves unmarked: the profile ran
    /// it more often than its summary calls cold.
    warm_tries: IndexSet<AstId>,
    /// The modules applied, which a compile seeded with this map leaves alone.
    sources: IndexSet<ModuleSource>,
    /// Each region marked, as `module:line:column kind`.
    regions: Vec<String>,
    remarks: Vec<(Span, String)>,
}

impl ColdMap {
    /// Apply `profile` to the modules it names. The modules `seed` applied
    /// keep what it found: they come from the stdlib snapshot with their
    /// markers already in place.
    pub(crate) fn build<'a>(
        seed: Option<&ColdMap>,
        modules: impl IntoIterator<Item = (&'a ModuleSource, &'a ast::Module)>,
        applied: Applied<'_>,
        rt_unreachable_calls: &IndexSet<AstId>,
    ) -> Self {
        let mut map = seed.cloned().unwrap_or_default();
        for (source, module) in modules {
            if map.sources.contains(source) {
                continue;
            }
            let Some(functions) = applied.profile.modules.get(&source.to_string()) else {
                continue;
            };
            map.apply(
                plan_module(source, module, rt_unreachable_calls),
                functions,
                applied.summary,
            );
            map.sources.insert(source.clone());
        }
        map
    }

    /// Mark each outermost region of `planned` that `functions` recorded no
    /// run of. A function is matched by its name, the `n`th of a name to the
    /// `n`th recorded, and its counts apply only where its hash is the
    /// recorded one. A `?`'s marker stays only on a count `summary` calls
    /// cold.
    fn apply(&mut self, planned: PlannedModule, functions: &[ProfiledFunction], summary: Summary) {
        let PlannedModule {
            plan,
            sites,
            parents,
            names,
            markers,
            ..
        } = planned;
        let mut by_name: IndexMap<&str, Vec<&ProfiledFunction>> = IndexMap::default();
        for function in functions {
            by_name.entry(&function.name).or_default().push(function);
        }
        let mut recorded: IndexMap<&str, std::vec::IntoIter<&ProfiledFunction>> = by_name
            .into_iter()
            .map(|(name, functions)| (name, functions.into_iter()))
            .collect();
        let mut counts: Vec<Option<i64>> = vec![None; plan.regions.len()];
        for ((function, ids), &name) in plan
            .functions
            .iter()
            .zip(plan.regions_by_function())
            .zip(&names)
        {
            let Some(profiled) = recorded
                .get_mut(function.name.as_str())
                .and_then(Iterator::next)
            else {
                continue;
            };
            if profiled.hash != function.hash {
                self.remarks.push((
                    name,
                    format!(
                        "the profile's counts for `{}` are discarded: its source or region plan \
                         changed since they were recorded",
                        function.name
                    ),
                ));
                continue;
            }
            // Workloads are tests, so a build's own entry, and whatever only
            // it reaches, never runs under them: a function they never entered
            // says nothing about how often it runs.
            if !profiled.counts.contains_key(&0) {
                continue;
            }
            for (index, id) in ids.into_iter().enumerate() {
                counts[id as usize] =
                    Some(profiled.counts.get(&(index as u32)).copied().unwrap_or(0));
            }
        }
        let site_of: IndexMap<u32, (ProbeSite, AstId)> = sites
            .into_iter()
            .map(|(site, id, region)| (region, (site, id)))
            .collect();
        for (id, region) in plan.regions.iter().enumerate() {
            // A region in an expression has no statement position to mark.
            if counts[id] != Some(0) || region.kind == RegionKind::Rhs {
                continue;
            }
            let mut parent = parents[id];
            while let Some(p) = parent
                && plan.regions[p as usize].kind == RegionKind::Rhs
            {
                parent = parents[p as usize];
            }
            if parent.is_some_and(|p| counts[p as usize] == Some(0)) {
                continue;
            }
            self.sites.insert(site_of[&(id as u32)]);
            self.regions.push(format!(
                "{}:{}:{} {}",
                plan.name,
                region.start.line,
                region.start.column,
                region.kind.label()
            ));
        }
        for (marker, span, region) in markers {
            let Some(count) = counts[region as usize].filter(|&runs| runs > 0) else {
                continue;
            };
            let runs = match count {
                1 => "once".to_string(),
                n => format!("{n} times"),
            };
            let message = match marker {
                Marker::ColdPath => {
                    format!("`cold_path()` marks a path the profile ran {runs}; the marker stays")
                }
                Marker::Try(_) if summary.is_cold(count) => continue,
                Marker::Try(id) => {
                    self.warm_tries.insert(id);
                    let line = if summary.is_hot(count) {
                        format!("the hot line is {}", summary.hot)
                    } else {
                        format!("the cold line is {}", summary.cold)
                    };
                    format!(
                        "`?` marks its failure path cold, but the profile ran it {runs}, and \
                         {line}; the marker is dropped"
                    )
                }
            };
            self.remarks.push((span, message));
        }
    }

    /// Whether reify puts a cold marker at `site` on node `id`.
    #[must_use]
    pub(crate) fn marks(&self, site: ProbeSite, id: AstId) -> bool {
        self.sites.contains(&(site, id))
    }

    /// Whether reify leaves the failure path of the `?` node `id` unmarked.
    #[must_use]
    pub(crate) fn is_warm_try(&self, id: AstId) -> bool {
        self.warm_tries.contains(&id)
    }

    /// Each region marked, as `module:line:column kind`.
    #[must_use]
    pub(crate) fn regions(&self) -> &[String] {
        &self.regions
    }

    /// What applying the profile says about its counts: a function's
    /// discarded, an author's marker on a path the profile ran, a `?`'s
    /// marker dropped.
    #[must_use]
    pub(crate) fn remarks(&self) -> &[(Span, String)] {
        &self.remarks
    }
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

    fn recorded() -> Coverage {
        let plan = ModulePlan {
            name: "a.wado".to_string(),
            path: "./a.wado".to_string(),
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
        coverage
    }

    #[test]
    fn a_profile_lists_every_function_and_the_regions_that_ran() {
        assert_eq!(
            to_text(&recorded()),
            "wado-pgo 1\n\
             module src/a.wado\n\
             fn 00000000000000ab 1 f\n\
             0 5\n\
             1 2\n\
             fn 00000000000000cd 5 g\n"
        );
    }

    #[test]
    fn a_profile_reads_back_what_was_written() {
        let profile = parse(&to_text(&recorded())).expect("it parses");
        let functions = &profile.modules["src/a.wado"];
        assert_eq!(
            functions,
            &[
                ProfiledFunction {
                    name: "f".to_string(),
                    hash: 0xab,
                    counts: BTreeMap::from([(0, 5), (1, 2)]),
                },
                ProfiledFunction {
                    name: "g".to_string(),
                    hash: 0xcd,
                    counts: BTreeMap::new(),
                },
            ]
        );
    }

    fn summary(counts: &[i64], cutoffs: Cutoffs) -> Summary {
        let mut text = format!("{HEADER}\nmodule a\nfn 00000000000000ab 1 f\n");
        for (region, count) in counts.iter().enumerate() {
            let _ = writeln!(text, "{region} {count}");
        }
        Summary::of(&parse(&text).expect("it parses"), cutoffs)
    }

    #[test]
    fn each_line_is_the_smallest_count_inside_its_share() {
        let lines = summary(&[600, 300, 60, 30, 10], Cutoffs { hot: 900_000, cold: 990_000 });
        assert_eq!(lines, Summary { hot: 300, cold: 30 });
        assert!(lines.is_hot(300) && !lines.is_cold(300));
        assert!(!lines.is_hot(60) && !lines.is_cold(60));
        assert!(lines.is_cold(30) && lines.is_cold(10));
    }

    #[test]
    fn a_count_on_both_lines_is_hot() {
        let lines = summary(&[1, 1, 1], Cutoffs::default());
        assert_eq!(lines, Summary { hot: 1, cold: 1 });
        assert!(lines.is_hot(1) && !lines.is_cold(1));
    }

    #[test]
    fn a_malformed_profile_names_its_line() {
        let cases = [
            ("wado-pgo 2\n", 1),
            ("wado-pgo 1\nfn 00000000000000ab 1 f\n", 2),
            ("wado-pgo 1\nmodule a\n0 5\n", 3),
            ("wado-pgo 1\nmodule a\nfn ab 1 f\n", 3),
            ("wado-pgo 1\nmodule a\nfn 00000000000000ab 1 f\n0 0\n", 4),
            (
                "wado-pgo 1\nmodule a\nfn 00000000000000ab 1 f\n0 1\n0 2\n",
                5,
            ),
            ("wado-pgo 1\nmodule a\nmodule a\n", 3),
        ];
        for (text, line) in cases {
            assert_eq!(parse(text).map_err(|e| e.line), Err(line), "{text}");
        }
    }
}
