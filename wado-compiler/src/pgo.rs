//! The profile `wado test --pgo` records: how often each region of each
//! measured function ran, in a text file the package commits. A compile given
//! one marks each region unlikely, cold, or both, by what it measured. See
//! [WEP: Profile-Guided Optimization](../../docs/wep-2026-10-25-profile-guided-optimization.md).
//!
//! The format is the specification's,
//! [The Profile Format](../../docs/spec-pgo.md#the-profile-format). A `module`
//! line is a [`Coverage::files`] key, which `--pgo` makes the module's name.

use std::collections::BTreeMap;
use std::fmt::{self, Write as _};

use crate::ast::{self, AstId, AstIdSpace};
use crate::coverage::{
    Coverage, PlannedModule, Pos, ProbeSite, RegionKind, SourceMarker, plan_module,
};
use crate::hashmap::{IndexMap, IndexSet};
use crate::module_source::ModuleSource;
use crate::rarity::Mark;
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

    /// This profile read under `thresholds`, against its own hot line.
    #[must_use]
    pub fn applied(&self, thresholds: Thresholds) -> Applied<'_> {
        Applied {
            profile: self,
            reading: Reading {
                thresholds,
                hot_line: self.count_line(thresholds.hot),
                cold_line: self.count_line(thresholds.cold),
            },
        }
    }

    /// The smallest count still inside `cutoff`'s share of all runs, with
    /// every count sorted from the largest down and the share rounded down, as
    /// LLVM's `ProfileSummaryBuilder` places its hot and cold lines.
    fn count_line(&self, cutoff: Cutoff) -> i64 {
        let mut counts: Vec<i64> = self
            .modules
            .values()
            .flatten()
            .flat_map(|function| function.counts.values().copied())
            .collect();
        counts.sort_unstable_by(|a, b| b.cmp(a));
        let total: u128 = counts.iter().map(|&count| count as u128).sum();
        let share = total * u128::from(cutoff.0) / u128::from(Cutoff::PER_MILLION);
        let (mut runs, mut line) = (0, 0);
        for &count in &counts {
            if runs >= share {
                break;
            }
            runs += count as u128;
            line = count;
        }
        line
    }
}

/// A share of all runs, in parts per million.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cutoff(u32);

impl Cutoff {
    pub const PER_MILLION: u32 = 1_000_000;

    /// `per_million` as a share, where it is at most the whole.
    #[must_use]
    pub fn new(per_million: u32) -> Option<Self> {
        (per_million <= Self::PER_MILLION).then_some(Self(per_million))
    }
}

/// A share of a branch's runs, in percent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Percent(u32);

impl Percent {
    pub const WHOLE: u32 = 100;

    /// `percent` as a share, where it is at most the whole.
    #[must_use]
    pub fn new(percent: u32) -> Option<Self> {
        (percent <= Self::WHOLE).then_some(Self(percent))
    }
}

/// How a compile reads a profile's counts: a region is unlikely past the share
/// of a branch's runs that makes the branch predictable, cold where it never
/// ran, and hot at or above the smallest count inside the `hot` share of all
/// runs. A call site is priced cold at or below the smallest count inside the
/// `cold` share. The defaults are LLVM's for its options of the same names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Thresholds {
    pub predictable_branch: Percent,
    pub hot: Cutoff,
    pub cold: Cutoff,
}

impl Default for Thresholds {
    fn default() -> Self {
        Self {
            predictable_branch: Percent(99),
            hot: Cutoff(990_000),
            cold: Cutoff(999_999),
        }
    }
}

/// What a profile's counts mean to a compile: the thresholds, and the hot and
/// cold lines they place over the whole profile, wherever a part of it is
/// applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Reading {
    thresholds: Thresholds,
    hot_line: i64,
    cold_line: i64,
}

impl Reading {
    /// How a call in a region the profile ran `runs` times is priced, as
    /// LLVM's `isHotCount` and `isColdCount` read a count.
    fn heat(self, runs: i64) -> Heat {
        if runs > 0 && runs >= self.hot_line {
            Heat::Hot
        } else if runs <= self.cold_line {
            Heat::Cold
        } else {
            Heat::Plain
        }
    }
}

/// The price a profile sets on a call site: the inliner's hot threshold, its
/// cold one, or the threshold a site without a profile pays.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Heat {
    Cold,
    Plain,
    Hot,
}

impl Thresholds {
    /// Whether a region the profile ran `runs` times is cold. Only one that
    /// never ran is, as GCC's `probably_never_executed` holds. The optimizer
    /// takes every function only cold sites reach as cold, so a region that
    /// ran, however rarely, may call what runs a million times.
    fn is_cold(runs: i64) -> bool {
        runs == 0
    }

    /// Whether a region the profile ran `runs` times, where the code around it
    /// ran `around` times, is entered on the rare side of a predictable branch.
    fn is_unlikely(self, runs: i64, around: i64) -> bool {
        let rare = u128::from(Percent::WHOLE - self.predictable_branch.0);
        runs as u128 * u128::from(Percent::WHOLE) <= around as u128 * rare
    }
}

/// A profile a compile applies, with how its counts are read.
#[derive(Debug, Clone, Copy)]
pub struct Applied<'a> {
    pub(crate) profile: &'a Profile,
    pub(crate) reading: Reading,
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

fn times(runs: i64) -> String {
    match runs {
        1 => "once".to_string(),
        n => format!("{n} times"),
    }
}

/// Where a profile marks a compile's code rare, the source's markers it
/// overrides, and the remarks applying it makes.
#[derive(Debug, Clone, Default)]
pub(crate) struct MarkMap {
    /// The sites reify puts a marker at.
    sites: IndexMap<(ProbeSite, AstId), Mark>,
    /// The nodes of the author's `cold_path()` calls and the `?`s whose
    /// markers reify leaves out, the profile having measured their regions.
    overridden: IndexSet<AstId>,
    /// The modules applied, which a compile seeded with this map leaves alone.
    sources: IndexSet<ModuleSource>,
    /// Each region marked, as `module:line:column kind mark`.
    marked: Vec<String>,
    remarks: Vec<(Span, String)>,
    /// The price each measured region sets on a call site, for the inliner.
    heat: HeatMap,
}

/// The [`Heat`] a profile found for each region it measured, per parse,
/// sorted by where a region starts, the outer of two starting together first.
/// Regions nest, so the innermost holding a position decides.
#[derive(Debug, Clone, Default)]
pub struct HeatMap {
    regions: IndexMap<AstIdSpace, Vec<MeasuredRegion>>,
}

#[derive(Debug, Clone, Copy)]
struct MeasuredRegion {
    start: Pos,
    end: Pos,
    heat: Heat,
}

impl HeatMap {
    fn add(&mut self, space: AstIdSpace, measured: impl IntoIterator<Item = MeasuredRegion>) {
        let regions = self.regions.entry(space).or_default();
        regions.extend(measured);
        regions.sort_by_key(|r| (r.start, std::cmp::Reverse(r.end)));
    }

    /// The heat of the code at `span`: the innermost measured region's that
    /// holds its start, [`Heat::Plain`] where none does.
    #[must_use]
    pub fn heat(&self, span: Span) -> Heat {
        let Some(regions) = self.regions.get(&span.space) else {
            return Heat::Plain;
        };
        let at = Pos {
            line: span.line as u32,
            column: span.column as u32,
        };
        let started = regions.partition_point(|r| r.start <= at);
        regions[..started]
            .iter()
            .rev()
            .find(|r| at < r.end)
            .map_or(Heat::Plain, |r| r.heat)
    }
}

impl MarkMap {
    /// Apply the profile to the modules it names. The modules `seed` applied
    /// keep what it found: they come from the stdlib snapshot with their
    /// markers already in place.
    pub(crate) fn build<'a>(
        seed: Option<&MarkMap>,
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
                module.ast_id_space(),
                functions,
                applied.reading,
            );
            map.sources.insert(source.clone());
        }
        map
    }

    /// Mark each region of `planned` by what `functions` recorded of it, read
    /// as `reading` says. A function is matched by its name, the `n`th of a
    /// name to the `n`th recorded, and its counts apply only where its hash is
    /// the recorded one.
    fn apply(
        &mut self,
        planned: PlannedModule,
        space: AstIdSpace,
        functions: &[ProfiledFunction],
        reading: Reading,
    ) {
        let thresholds = reading.thresholds;
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
        let count_of = |region: Option<u32>| region.and_then(|r| counts[r as usize]);
        // Unlikely: entered on the rare side of a predictable branch. The ratio
        // to the code around it is that branch's for a path past a branch or a
        // leaving statement, and bounds it from above for a loop's body, whose
        // test also runs once per iteration. A function's body and a closure's
        // are entered by a call, not a branch.
        let unlikely = |id: usize| match (counts[id], count_of(parents[id])) {
            (Some(runs), Some(around)) => thresholds.is_unlikely(runs, around),
            _ => false,
        };
        let cold = |id: usize| counts[id].is_some_and(Thresholds::is_cold);
        // The region a marker for `id` sits under: a region in an expression
        // has no statement position to mark.
        let markable_parent = |id: usize| {
            let mut parent = parents[id];
            while let Some(p) = parent
                && plan.regions[p as usize].kind == RegionKind::Rhs
            {
                parent = parents[p as usize];
            }
            parent
        };
        // A hot price on the rare side of a branch would grow code that rarely
        // runs, so an unlikely region pays the plain one. A `?`'s region spans
        // its operand, whose calls run before the failure path the region
        // counts, so it prices none of them.
        self.heat.add(
            space,
            plan.regions.iter().enumerate().filter_map(|(id, region)| {
                if region.kind == RegionKind::Try {
                    return None;
                }
                counts[id].map(|runs| MeasuredRegion {
                    start: region.start,
                    end: region.end,
                    heat: match reading.heat(runs) {
                        Heat::Hot if unlikely(id) => Heat::Plain,
                        heat => heat,
                    },
                })
            }),
        );
        for (id, region) in plan.regions.iter().enumerate() {
            if counts[id].is_none() || region.kind == RegionKind::Rhs {
                continue;
            }
            let parent = markable_parent(id);
            // Under a region that never ran, whose marker covers this one.
            if count_of(parent) == Some(0) {
                continue;
            }
            let Some(mark) = Mark::of(unlikely(id), cold(id)) else {
                continue;
            };
            self.sites.insert(site_of[&(id as u32)], mark);
            self.marked.push(format!(
                "{}:{}:{} {} {}",
                plan.name,
                region.start.line,
                region.start.column,
                region.kind.label(),
                mark.label()
            ));
        }
        for (marker, span, region) in markers {
            let region = region as usize;
            // Where the profile has no count, or cannot place a marker of its
            // own, the source's stays.
            let Some(runs) = counts[region] else {
                continue;
            };
            if plan.regions[region].kind == RegionKind::Rhs {
                continue;
            }
            self.overridden.insert(marker.node());
            // A path that never ran is unlikely and cold, which the marker
            // said; one that ran is not cold.
            if Thresholds::is_cold(runs) {
                continue;
            }
            let verdict = if unlikely(region) {
                "unlikely but not cold"
            } else {
                "neither unlikely nor cold"
            };
            let subject = match marker {
                SourceMarker::ColdPath(_) => "`cold_path()` marks this path rare",
                SourceMarker::Try(_) => "`?` marks its failure path rare",
            };
            // A loop's body may run more often than the code around it.
            let measured = match count_of(parents[region]) {
                Some(around) => {
                    format!(
                        "{} where the code around it ran {}",
                        times(runs),
                        times(around)
                    )
                }
                None => times(runs),
            };
            self.remarks.push((
                span,
                format!(
                    "{subject}, but the profile ran it {measured}, so it is {verdict}; the \
                     profile overrides the marker"
                ),
            ));
        }
    }

    /// The marker reify puts at `site` on node `id`, if any.
    #[must_use]
    pub(crate) fn mark(&self, site: ProbeSite, id: AstId) -> Option<Mark> {
        self.sites.get(&(site, id)).copied()
    }

    /// Whether reify leaves out the marker the source states at node `id`.
    #[must_use]
    pub(crate) fn overrides(&self, id: AstId) -> bool {
        self.overridden.contains(&id)
    }

    /// Each region marked, as `module:line:column kind mark`.
    #[must_use]
    pub(crate) fn marked(&self) -> &[String] {
        &self.marked
    }

    #[must_use]
    pub(crate) fn heat_map(&self) -> &HeatMap {
        &self.heat
    }

    /// What applying the profile says about its counts: a function's
    /// discarded, a source's marker overridden by a measurement that
    /// disagrees with it.
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

    /// LLVM's default: a branch is predictable when one side takes 99% of its
    /// runs, so the other side is unlikely at 1% or less.
    #[test]
    fn a_side_is_unlikely_at_the_predictable_branchs_complement() {
        let thresholds = Thresholds::default();
        assert!(thresholds.is_unlikely(10, 1000));
        assert!(!thresholds.is_unlikely(11, 1000));
        assert!(thresholds.is_unlikely(0, 1));
    }

    /// A line is the smallest of the largest counts that, added up, reach the
    /// cutoff's share of all runs. LLVM's defaults put the hot one at 99% and
    /// the cold one at 99.9999%.
    #[test]
    fn a_count_line_is_the_smallest_count_inside_the_cutoff() {
        let profile = parse(
            "wado-pgo 1\n\
             module src/a.wado\n\
             fn 0000000000000001 1 f\n\
             0 900\n\
             1 90\n\
             2 10\n",
        )
        .expect("it parses");
        let line = |per_million| profile.count_line(Cutoff::new(per_million).expect("a share"));
        assert_eq!(line(900_000), 900);
        assert_eq!(line(990_000), 90);
        assert_eq!(line(999_999), 10);
        let reading = profile.applied(Thresholds::default()).reading;
        assert_eq!(reading.heat(90), Heat::Hot);
        assert_eq!(reading.heat(50), Heat::Plain);
        assert_eq!(reading.heat(10), Heat::Cold);
    }

    #[test]
    fn the_innermost_measured_region_decides_the_heat_of_code() {
        let space = AstIdSpace::next();
        let pos = |line, column| Pos { line, column };
        let mut map = HeatMap::default();
        map.add(
            space,
            [
                MeasuredRegion {
                    start: pos(1, 1),
                    end: pos(9, 2),
                    heat: Heat::Hot,
                },
                MeasuredRegion {
                    start: pos(3, 5),
                    end: pos(5, 6),
                    heat: Heat::Cold,
                },
            ],
        );
        let at = |line, column| Span {
            line,
            column,
            space,
            ..Span::default()
        };
        assert_eq!(map.heat(at(2, 1)), Heat::Hot);
        assert_eq!(map.heat(at(3, 5)), Heat::Cold);
        assert_eq!(map.heat(at(5, 5)), Heat::Cold);
        assert_eq!(map.heat(at(5, 6)), Heat::Hot);
        assert_eq!(map.heat(at(9, 2)), Heat::Plain);
        let elsewhere = Span {
            space: AstIdSpace::next(),
            ..at(2, 1)
        };
        assert_eq!(map.heat(elsewhere), Heat::Plain);
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
