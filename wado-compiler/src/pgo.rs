//! The profile `wado test --pgo` records: how often each region of each
//! measured function ran, in a text file the package commits. A compile given
//! one marks each region unlikely, cold, or both, by what it measured. See
//! [WEP: Profile-Guided Optimization](../../docs/wep-2026-10-25-profile-guided-optimization.md).
//!
//! The format is the specification's,
//! [The Profile Format](../../docs/spec-pgo.md#the-profile-format). A `module`
//! line is a [`Coverage::files`] key, which `--pgo` makes the module's name.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::{self, Write as _};

use crate::ast::{self, AstId, AstIdSpace};
use crate::coverage::{
    Coverage, PlannedModule, Pos, ProbeSite, RegionKind, SourceMarker, plan_module,
};
use crate::hashmap::{IndexMap, IndexSet};
use crate::module_source::ModuleSource;
use crate::name::unversioned_module_name;
use crate::rarity::Mark;
use crate::stdlib::core_module_file;
use crate::token::Span;

/// The first line of every profile, naming its format.
pub const HEADER: &str = "wado-pgo 1";

/// The runs of each region of a function that ran, by its number within the
/// function.
type Counts = BTreeMap<u32, i64>;

/// `coverage` as a profile, with its files keyed by the names a profile gives
/// modules, and each function's counts summed with every other function's of
/// its hash.
#[must_use]
pub fn to_text(coverage: &Coverage) -> String {
    let mut summed: BTreeMap<u64, Vec<i64>> = BTreeMap::new();
    for file in coverage.files.values() {
        for (function, ids) in file
            .plan
            .functions
            .iter()
            .zip(file.plan.regions_by_function())
        {
            let runs = summed
                .entry(function.hash)
                .or_insert_with(|| vec![0; ids.len()]);
            assert_eq!(runs.len(), ids.len(), "a hash covers the region plan");
            for (sum, id) in runs.iter_mut().zip(ids) {
                *sum += file.count(id);
            }
        }
    }
    let mut out = format!("{HEADER}\n");
    for (module, file) in &coverage.files {
        let _ = writeln!(out, "module {module}");
        for function in &file.plan.functions {
            let _ = writeln!(
                out,
                "fn {:016x} {} {}",
                function.hash, function.line, function.name
            );
            for (index, &runs) in summed[&function.hash].iter().enumerate() {
                if runs > 0 {
                    let _ = writeln!(out, "{index} {runs}");
                }
            }
        }
    }
    out
}

/// A profile read back: the counts of each function by its hash, which is all
/// a compile matches a function by.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Profile {
    counts: BTreeMap<u64, Counts>,
    /// The functions each module lists, by their names and by the module's
    /// [`unversioned_module_name`]. They tell a function whose source changed
    /// from one no workload measured, and the stdlib's from the rest.
    listed: BTreeMap<String, Vec<(String, u64)>>,
}

impl Profile {
    /// The part listed under `core:` modules: what the stdlib snapshot applies.
    #[must_use]
    pub(crate) fn stdlib_part(&self) -> Self {
        let listed: BTreeMap<_, _> = self
            .listed
            .iter()
            .filter(|(name, _)| core_module_file(name).is_some())
            .map(|(name, functions)| (name.clone(), functions.clone()))
            .collect();
        let counts = listed
            .values()
            .flatten()
            .map(|&(_, hash)| (hash, self.counts[&hash].clone()))
            .collect();
        Self { counts, listed }
    }

    #[must_use]
    pub(crate) fn is_empty(&self) -> bool {
        self.counts.is_empty()
    }

    /// Whether a workload measured a function named `name` in `module`, under
    /// any version of its package.
    fn measured(&self, module: &str, name: &str) -> bool {
        self.listed
            .get(&unversioned_module_name(module))
            .is_some_and(|functions| functions.iter().any(|(listed, _)| listed == name))
    }

    /// This profile read under `thresholds`, against its own hot line.
    #[must_use]
    pub fn applied(&self, thresholds: Thresholds) -> Applied<'_> {
        Applied {
            profile: self,
            reading: Reading {
                thresholds,
                hot_line: self.count_line(thresholds.hot),
                // No count inside the share leaves only a region that never
                // ran at or below it.
                cold_line: self.count_line(thresholds.cold).unwrap_or(0),
            },
        }
    }

    /// The smallest count still inside `cutoff`'s share of all runs, with
    /// every count sorted from the largest down and the share rounded down, as
    /// LLVM's `ProfileSummaryBuilder` places its hot and cold lines. `None`
    /// where the share holds no count.
    fn count_line(&self, cutoff: Cutoff) -> Option<i64> {
        let mut counts: Vec<i64> = self
            .counts
            .values()
            .flat_map(Counts::values)
            .copied()
            .collect();
        counts.sort_unstable_by(|a, b| b.cmp(a));
        let total: u128 = counts.iter().map(|&count| count as u128).sum();
        let share = total * u128::from(cutoff.0) / u128::from(Cutoff::PER_MILLION);
        let (mut runs, mut line) = (0, None);
        for &count in &counts {
            if runs >= share {
                break;
            }
            runs += count as u128;
            line = Some(count);
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
    hot_line: Option<i64>,
    cold_line: i64,
}

impl Reading {
    /// How a call in a region the profile ran `runs` times is priced, as
    /// LLVM's `isHotCount` and `isColdCount` read a count.
    fn heat(self, runs: i64) -> Heat {
        if runs > 0 && self.hot_line.is_some_and(|line| runs >= line) {
            Heat::Hot
        } else if runs <= self.cold_line {
            Heat::Cold
        } else {
            Heat::Plain
        }
    }
}

/// The price a profile sets on a call site: the inliner's hot threshold, its
/// cold one, or the threshold a site without a profile pays. Ordered from cold
/// to hot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Heat {
    Cold,
    Plain,
    Hot,
}

impl Heat {
    /// This heat, with a hot one priced plain where `demoted` holds.
    #[must_use]
    pub fn plain_where(self, demoted: bool) -> Self {
        match self {
            Heat::Hot if demoted => Heat::Plain,
            heat => heat,
        }
    }
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
    let mut profile = Profile::default();
    let mut modules = BTreeSet::new();
    let mut module: Option<String> = None;
    // The function the count lines read belong to: its `fn` line and hash.
    let mut function: Option<(usize, u64, Counts)> = None;
    let close = |profile: &mut Profile, function: Option<(usize, u64, Counts)>| {
        let Some((number, hash, counts)) = function else {
            return Ok(());
        };
        match profile.counts.insert(hash, counts.clone()) {
            Some(listed) if listed != counts => Err(error(
                number,
                "a function's counts are the same wherever its hash is listed",
            )),
            _ => Ok(()),
        }
    };
    for (number, line) in lines {
        if let Some(name) = line.strip_prefix("module ") {
            close(&mut profile, function.take())?;
            if !modules.insert(name.to_string()) {
                return Err(error(number, "a module is listed once"));
            }
            module = Some(unversioned_module_name(name));
        } else if let Some(rest) = line.strip_prefix("fn ") {
            close(&mut profile, function.take())?;
            let (hash, name) = parse_function(rest)
                .ok_or_else(|| error(number, "a function is `fn <hash> <line> <name>`"))?;
            let module = module
                .clone()
                .ok_or_else(|| error(number, "a function follows a `module` line"))?;
            profile
                .listed
                .entry(module)
                .or_default()
                .push((name.to_string(), hash));
            function = Some((number, hash, Counts::new()));
        } else {
            let (region, runs) = parse_count(line)
                .ok_or_else(|| error(number, "a count is `<region> <runs>`, runs above zero"))?;
            let (_, _, counts) = function
                .as_mut()
                .ok_or_else(|| error(number, "a count follows a `fn` line"))?;
            if counts.insert(region, runs).is_some() {
                return Err(error(number, "a region is counted once"));
            }
        }
    }
    close(&mut profile, function)?;
    Ok(profile)
}

/// A `fn` line's hash and name.
fn parse_function(rest: &str) -> Option<(u64, &str)> {
    let mut parts = rest.splitn(3, ' ');
    let hash = parts.next().filter(|hash| hash.len() == 16)?;
    parts.next()?.parse::<u32>().ok()?;
    let name = parts.next().filter(|name| !name.is_empty())?;
    Some((u64::from_str_radix(hash, 16).ok()?, name))
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
    any_hot: bool,
}

#[derive(Debug, Clone, Copy)]
struct MeasuredRegion {
    start: Pos,
    end: Pos,
    heat: Heat,
    /// The index of the innermost region holding this one.
    enclosing: Option<usize>,
}

impl HeatMap {
    /// Whether some region is [`Heat::Hot`].
    #[must_use]
    pub fn any_hot(&self) -> bool {
        self.any_hot
    }

    fn add(&mut self, space: AstIdSpace, measured: impl IntoIterator<Item = MeasuredRegion>) {
        let regions = self.regions.entry(space).or_default();
        regions.extend(measured);
        self.any_hot |= regions.iter().any(|r| r.heat == Heat::Hot);
        regions.sort_by_key(|r| (r.start, std::cmp::Reverse(r.end)));
        let mut open: Vec<usize> = Vec::new();
        for i in 0..regions.len() {
            while open
                .last()
                .is_some_and(|&o| regions[o].end <= regions[i].start)
            {
                open.pop();
            }
            let enclosing = open.last().copied();
            if let Some(o) = enclosing {
                assert!(regions[i].end <= regions[o].end, "regions must nest");
            }
            regions[i].enclosing = enclosing;
            open.push(i);
        }
    }

    /// The heat of the code at `span`: the innermost measured region's that
    /// holds its start, [`Heat::Plain`] where none does.
    #[must_use]
    pub fn heat(&self, span: Span) -> Heat {
        let Some(regions) = self.regions.get(&span.space) else {
            return Heat::Plain;
        };
        let at = Pos::start(span);
        // The last region to start before `at` either holds it or is held by
        // every region that does.
        let mut next = regions.partition_point(|r| r.start <= at).checked_sub(1);
        while let Some(i) = next {
            if at < regions[i].end {
                return regions[i].heat;
            }
            next = regions[i].enclosing;
        }
        Heat::Plain
    }
}

impl MarkMap {
    /// Apply the profile to `modules`, on top of what `seed`, the stdlib
    /// snapshot's map, found in the modules it serves.
    pub(crate) fn build<'a>(
        seed: Option<&MarkMap>,
        modules: impl IntoIterator<Item = (&'a ModuleSource, &'a ast::Module)>,
        applied: Applied<'_>,
        rt_unreachable_calls: &IndexSet<AstId>,
    ) -> Self {
        let mut map = seed.cloned().unwrap_or_default();
        for (source, module) in modules {
            map.apply(
                plan_module(source, module, rt_unreachable_calls),
                module.ast_id_space(),
                applied,
            );
        }
        map
    }

    /// Mark each region of `planned` by what the profile recorded of the
    /// function of its hash, read as `applied` says.
    fn apply(&mut self, planned: PlannedModule, space: AstIdSpace, applied: Applied<'_>) {
        let Applied { profile, reading } = applied;
        let thresholds = reading.thresholds;
        let PlannedModule {
            plan,
            sites,
            parents,
            names,
            markers,
            ..
        } = planned;
        let mut counts: Vec<Option<i64>> = vec![None; plan.regions.len()];
        for ((function, ids), &name) in plan
            .functions
            .iter()
            .zip(plan.regions_by_function())
            .zip(&names)
        {
            let Some(recorded) = profile.counts.get(&function.hash) else {
                if profile.measured(&plan.name, &function.name) {
                    self.remarks.push((
                        name,
                        format!(
                            "the profile's counts for `{}` are discarded: its source or region \
                             plan changed since they were recorded",
                            function.name
                        ),
                    ));
                }
                continue;
            };
            // Workloads are tests, so a build's own entry, and whatever only
            // it reaches, never runs under them: a function they never entered
            // says nothing about how often it runs.
            if !recorded.contains_key(&0) {
                continue;
            }
            for (index, id) in ids.into_iter().enumerate() {
                counts[id as usize] = Some(recorded.get(&(index as u32)).copied().unwrap_or(0));
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
        // A hot price on the rare side of a branch would grow code that rarely
        // runs, so an unlikely region pays the plain one, and so does every
        // region inside it, as the inliner reads the hint a marker writes. A
        // `?`'s region spans its operand, whose calls run before the failure
        // path the region counts, so it prices none of them.
        let under_unlikely = |id: usize| {
            let mut region = Some(id);
            while let Some(r) = region {
                if unlikely(r) {
                    return true;
                }
                region = parents[r].map(|p| p as usize);
            }
            false
        };
        self.heat.add(
            space,
            plan.regions.iter().enumerate().filter_map(|(id, region)| {
                if region.kind == RegionKind::Try {
                    return None;
                }
                counts[id].map(|runs| MeasuredRegion {
                    start: region.start,
                    end: region.end,
                    heat: reading.heat(runs).plain_where(under_unlikely(id)),
                    enclosing: None,
                })
            }),
        );
        for (id, region) in plan.regions.iter().enumerate() {
            if counts[id].is_none() {
                continue;
            }
            // Under a region that never ran, whose marker covers this one.
            if count_of(parents[id]) == Some(0) {
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
            // Where the profile has no count, the source's marker stays.
            let Some(runs) = counts[region] else {
                continue;
            };
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

    #[cfg(test)]
    #[must_use]
    pub(crate) fn heat_map(&self) -> &HeatMap {
        &self.heat
    }

    #[must_use]
    pub(crate) fn into_heat_map(self) -> HeatMap {
        self.heat
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
        assert_eq!(
            profile.counts,
            BTreeMap::from([
                (0xab, BTreeMap::from([(0, 5), (1, 2)])),
                (0xcd, BTreeMap::new()),
            ])
        );
    }

    /// Functions of one hash share their counts, as a generic function's
    /// instances do, and each module lists them under its own name.
    #[test]
    fn a_profile_sums_the_functions_of_one_hash() {
        let plan = |name: &str| ModulePlan {
            name: name.to_string(),
            path: format!("./{name}"),
            functions: vec![PlannedFunction {
                name: "f".to_string(),
                line: 1,
                region: 0,
                hash: 0xab,
            }],
            regions: vec![
                region(RegionKind::Function, 0, 1),
                region(RegionKind::Then, 0, 2),
            ],
            lines: Vec::new(),
        };
        let mut coverage = Coverage::default();
        let registered = coverage
            .register(&vec![(plan("a.wado"), 0), (plan("b.wado"), 2)], |plan| {
                format!("src/{}", plan.name)
            })
            .expect("two plans");
        coverage.record(&registered, &BTreeMap::from([(0, 3), (2, 2), (3, 2)]), None);
        assert_eq!(
            to_text(&coverage),
            "wado-pgo 1\n\
             module src/a.wado\n\
             fn 00000000000000ab 1 f\n\
             0 5\n\
             1 2\n\
             module src/b.wado\n\
             fn 00000000000000ab 1 f\n\
             0 5\n\
             1 2\n"
        );
    }

    /// A module's name without its package's version tells a function whose
    /// source changed from one no workload measured, across an upgrade too.
    #[test]
    fn a_profile_knows_what_it_measured_under_another_version() {
        let profile = parse(
            "wado-pgo 1\n\
             module acme:app@0.1.0/src/lib.wado\n\
             fn 00000000000000ab 1 f\n\
             0 1\n",
        )
        .expect("it parses");
        assert!(profile.measured("acme:app@0.2.0/src/lib.wado", "f"));
        assert!(!profile.measured("acme:app@0.2.0/src/lib.wado", "g"));
        assert!(!profile.measured("acme:app@0.2.0/src/main.wado", "f"));
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
        assert_eq!(line(900_000), Some(900));
        assert_eq!(line(990_000), Some(90));
        assert_eq!(line(999_999), Some(10));
        assert_eq!(line(0), None);
        let reading = profile.applied(Thresholds::default()).reading;
        assert_eq!(reading.heat(90), Heat::Hot);
        assert_eq!(reading.heat(50), Heat::Plain);
        assert_eq!(reading.heat(10), Heat::Cold);
    }

    /// A share that holds no count places no line, so nothing is hot.
    #[test]
    fn an_empty_hot_share_heats_nothing() {
        let profile = parse(
            "wado-pgo 1\n\
             module src/a.wado\n\
             fn 0000000000000001 1 f\n\
             0 900\n\
             1 10\n",
        )
        .expect("it parses");
        let thresholds = Thresholds {
            hot: Cutoff::new(0).expect("a share"),
            ..Thresholds::default()
        };
        let reading = profile.applied(thresholds).reading;
        assert_eq!(reading.heat(900), Heat::Plain);
        assert_eq!(reading.heat(10), Heat::Cold);
    }

    #[test]
    fn the_innermost_measured_region_decides_the_heat_of_code() {
        let space = AstIdSpace::next();
        let region = |start: [u32; 2], end: [u32; 2], heat| MeasuredRegion {
            start: Pos {
                line: start[0],
                column: start[1],
            },
            end: Pos {
                line: end[0],
                column: end[1],
            },
            heat,
            enclosing: None,
        };
        let mut map = HeatMap::default();
        map.add(
            space,
            [
                region([3, 5], [5, 6], Heat::Cold),
                region([6, 1], [7, 1], Heat::Cold),
                region([10, 1], [11, 1], Heat::Cold),
                region([1, 1], [9, 2], Heat::Hot),
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
        assert_eq!(map.heat(at(8, 1)), Heat::Hot);
        assert_eq!(map.heat(at(9, 2)), Heat::Plain);
        assert_eq!(map.heat(at(12, 1)), Heat::Plain);
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
            (
                "wado-pgo 1\nmodule a\nfn 00000000000000ab 1 f\n0 1\n\
                 module b\nfn 00000000000000ab 1 f\n0 2\n",
                6,
            ),
        ];
        for (text, line) in cases {
            assert_eq!(parse(text).map_err(|e| e.line), Err(line), "{text}");
        }
    }
}
