//! What the inliner decided and why, for `wado dump --inline-decisions`.
//!
//! The decisions record the reasons the inliner's own functions return
//! ([`Basis`] for a callee, [`Refusal`] for a site), so the log cannot drift
//! from what the inliner did.

use std::fmt::Write as _;

use super::{Basis, ByHeat, ColdBy, Refusal, Site};
use crate::module_source::{ModuleSource, ParseModules};
use crate::pgo::Heat;
use crate::token::Span;

/// Every round's decisions about the functions whose name holds a pattern.
#[derive(Debug)]
pub struct InlineLog {
    pattern: String,
    rounds: Vec<Round>,
}

impl InlineLog {
    /// A log of the decisions where a caller's or a callee's name holds
    /// `pattern`.
    pub fn new(pattern: &str) -> Self {
        InlineLog {
            pattern: pattern.to_string(),
            rounds: Vec::new(),
        }
    }

    pub(super) fn wants(&self, name: &str) -> bool {
        name.contains(&self.pattern)
    }

    pub(super) fn push(&mut self, round: Round) {
        self.rounds.push(round);
    }

    /// The log as text, each span named by the module whose parse it indexes.
    pub fn render(&self, parses: &ParseModules) -> String {
        if self.rounds.is_empty() {
            return "(no inliner round ran)\n".to_string();
        }
        let mut out = String::new();
        for (i, round) in self.rounds.iter().enumerate() {
            round.render(i + 1, parses, &mut out);
        }
        if out.is_empty() {
            let _ = writeln!(
                out,
                "(no decision is about a function whose name holds `{}`)",
                self.pattern
            );
        }
        out
    }
}

/// One run of the inliner.
#[derive(Debug, Default)]
pub(super) struct Round {
    pub threshold: usize,
    /// What the unit holds as the round starts, or `None` where nothing
    /// priced it.
    pub unit_size: Option<usize>,
    pub held: Vec<(String, ModuleSource)>,
    pub callees: Vec<CalleeEntry>,
    pub sites: Vec<SiteEntry>,
}

#[derive(Debug)]
pub(super) struct CalleeEntry {
    pub name: String,
    pub module: ModuleSource,
    /// What running one copy costs at a site of each heat, where the verdict
    /// priced it.
    pub hot: ByHeat<Option<usize>>,
    /// What holding one copy costs, where the round priced it.
    pub size: Option<usize>,
    pub params: usize,
    pub sites: usize,
    pub basis: ByHeat<Basis>,
    /// Admitted, and then turned down by the growth budget.
    pub over_budget: bool,
}

#[derive(Debug)]
pub(super) struct SiteEntry {
    pub caller: String,
    pub caller_module: ModuleSource,
    pub callee: String,
    pub callee_module: ModuleSource,
    pub span: Span,
    pub site: Site,
    /// The heat the site was priced at.
    pub heat: Heat,
    pub refusal: Option<Refusal>,
}

impl Round {
    fn render(&self, number: usize, parses: &ParseModules, out: &mut String) {
        if self.held.is_empty() && self.callees.is_empty() && self.sites.is_empty() {
            return;
        }
        let _ = write!(out, "round {number}: threshold {}", self.threshold);
        if let Some(unit) = self.unit_size {
            let _ = write!(out, ", unit {unit}");
        }
        out.push('\n');
        if !self.held.is_empty() {
            let held: Vec<String> = self
                .held
                .iter()
                .map(|(name, module)| function(name, module))
                .collect();
            let _ = writeln!(out, "  held: {}", held.join(", "));
        }
        for c in &self.callees {
            let _ = write!(out, "  callee {}:", function(&c.name, &c.module));
            if let Some(hot) = c.hot.plain {
                let _ = write!(out, " hot {hot},");
            }
            if let Some(size) = c.size {
                let _ = write!(out, " size {size},");
            }
            let _ = write!(
                out,
                " {} params, {} sites -> {}",
                c.params,
                c.sites,
                basis_text(c.basis.plain)
            );
            for (heat, name) in [(Heat::Hot, "hot"), (Heat::Cold, "cold")] {
                let (hot, basis) = (*c.hot.at(heat), *c.basis.at(heat));
                if (hot, basis) != (c.hot.plain, c.basis.plain) {
                    let _ = write!(out, "; at a profiled {name} site:");
                    if let Some(hot) = hot {
                        let _ = write!(out, " hot {hot},");
                    }
                    let _ = write!(out, " {}", basis_text(basis));
                }
            }
            if c.over_budget {
                out.push_str(", over the growth budget");
            }
            out.push('\n');
        }
        for s in &self.sites {
            let at = location(s.span, &s.caller_module, parses);
            let _ = write!(
                out,
                "  site {} at {at} ({}{}) -> {}: ",
                function(&s.caller, &s.caller_module),
                site_text(s.site, &s.caller_module, parses),
                heat_text(s.heat),
                function(&s.callee, &s.callee_module)
            );
            match s.refusal {
                None => out.push_str("spliced"),
                Some(refusal) => {
                    let _ = write!(out, "declined, {}", refusal_text(refusal));
                }
            }
            out.push('\n');
        }
    }
}

/// A function's name, which only its module tells from another of that name.
fn function(name: &str, module: &ModuleSource) -> String {
    format!("`{name}` in {module}")
}

fn location(span: Span, holder: &ModuleSource, parses: &ParseModules) -> String {
    if span.is_synthetic() {
        return "(synthesized)".to_string();
    }
    format!(
        "{}:{}:{}",
        parses.module_of_span(span, holder),
        span.line,
        span.column
    )
}

fn site_text(site: Site, holder: &ModuleSource, parses: &ParseModules) -> String {
    match site {
        Site::Plain => "plain".to_string(),
        Site::Loop => "loop".to_string(),
        Site::Cold(by) => format!("cold, {}", cold_text(by, holder, parses)),
        Site::UnlikelyPlain => "plain, unlikely".to_string(),
        Site::UnlikelyLoop => "loop, unlikely".to_string(),
    }
}

/// The profile's heat, where it moved the site's price.
fn heat_text(heat: Heat) -> &'static str {
    match heat {
        Heat::Plain => "",
        Heat::Hot => ", profiled hot",
        Heat::Cold => ", profiled cold",
    }
}

fn cold_text(by: ColdBy, holder: &ModuleSource, parses: &ParseModules) -> String {
    match by {
        ColdBy::Run(span) => format!("in the run from {}", location(span, holder, parses)),
        ColdBy::Callers => "every path into it passes a cold run".to_string(),
    }
}

fn basis_text(basis: Basis) -> &'static str {
    match basis {
        Basis::NoBody => "nowhere: no body",
        Basis::CycleCut => "nowhere: a call cycle is cut here",
        Basis::NeverHint => "nowhere: `#[inline(never)]`",
        Basis::CmBinding => "nowhere: a CM binding",
        Basis::Diverges => "nowhere: it returns `!`",
        Basis::Forced => "everywhere: `#[inline(always)]`",
        Basis::Fits => "everywhere: under the threshold",
        Basis::Folds => "everywhere: the callers' constants fold it under the threshold",
        Basis::OverPayback => "nowhere: over the payback bound",
        Basis::SoleOverPayback => "its one site, in a loop: over the payback bound, and moved",
        Basis::Safepoint {
            sole_and_loop_free: true,
        } => "its one site outside a loop: it reaches a safepoint",
        Basis::Safepoint {
            sole_and_loop_free: false,
        } => "nowhere: it reaches a safepoint, and has a loop or several sites",
        Basis::Payback { sole: true } => "its one site: under the payback bound",
        Basis::Payback { sole: false } => {
            "its one site in each loop: under the payback bound, with several sites"
        }
    }
}

fn refusal_text(refusal: Refusal) -> &'static str {
    match refusal {
        Refusal::Cold => "a cold site takes only `#[inline(always)]` or a callee that shrinks it",
        Refusal::Glue => "the caller is CM binding or dispatch glue",
        Refusal::NotTheSoleSite => "admitted at its one site, and this site came with a splice",
        Refusal::LeftToEngine => "left to the compile-time engine to fold",
        Refusal::InLoop => "admitted outside loops only",
        Refusal::OutsideLoop => "admitted in loops only",
        Refusal::OtherHeat => "admitted only at sites the profile heats otherwise",
        Refusal::SeveralLoopSites => "called from several sites of this loop",
        Refusal::RescanSpent => "the splice under way spent what its re-scan may add",
    }
}
