//! Test coverage: the region plan a module's source implies, the probe sites
//! reify reads it through, and the `org.wado-lang.coverage` custom section that
//! carries it to `wado test`. See
//! [WEP: Test Coverage](../../docs/wep-2026-09-28-test-coverage.md).
//!
//! A region is a span of source that runs as a unit. The plan numbers the
//! regions of every function a module declares, reached or not, so the numbers
//! depend on the source text alone. Reify puts a probe at each [`ProbeSite`];
//! the host hears each region's first hit and the runner joins hits to plans.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;

use sha2::{Digest, Sha256};

use crate::ast::{
    self, AstId, AstVisitor, Block, Condition, ConditionElement, Expr, Function, Item, MatchExpr,
    Stmt, walk_condition, walk_expr, walk_stmt,
};
use crate::attribute;
use crate::hashmap::{IndexMap, IndexSet};
use crate::lexer::normalize_line_breaks;
use crate::module_source::ModuleSource;
use crate::token::Span;

/// Name of the custom section embedded in instrumented test-world components.
pub const SECTION_NAME: &str = "org.wado-lang.coverage";

/// Which modules a compile instruments.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CoverageScope {
    /// Also the modules of dependency and remote packages.
    pub include_deps: bool,
    /// Also `core:` modules: the package under test is the standard library.
    pub stdlib: bool,
}

impl CoverageScope {
    /// Whether a module from `source` is measured.
    #[must_use]
    pub fn measures(self, source: &ModuleSource) -> bool {
        match source {
            ModuleSource::EntryPoint { .. } | ModuleSource::Local { .. } => true,
            ModuleSource::Dependency { .. } | ModuleSource::Remote { .. } => self.include_deps,
            // The allocator is the Canonical ABI's `realloc`, where a call to
            // the host traps (`may_leave` is clear).
            ModuleSource::Core { .. } => self.stdlib && *source != ModuleSource::allocator(),
            ModuleSource::Binding { .. }
            | ModuleSource::Redirected { .. }
            | ModuleSource::Wasm { .. } => false,
            ModuleSource::Effect(head) => self.measures(head.module()),
        }
    }
}

/// What starts a region.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum RegionKind {
    /// A `fn` or method body.
    Function,
    /// A closure body.
    Closure,
    /// An `if` branch taken on true.
    Then,
    /// An `if` branch taken on false, written or not.
    Else,
    /// A `match` arm.
    Arm,
    /// A loop body.
    Loop,
    /// The `else` block of `let … else`.
    LetElse,
    /// The right operand of `&&` or `||`.
    Rhs,
    /// The early return `?` takes.
    Try,
    /// The statements after one that can leave its block early.
    Continuation,
}

impl RegionKind {
    const ALL: [Self; 10] = [
        Self::Function,
        Self::Closure,
        Self::Then,
        Self::Else,
        Self::Arm,
        Self::Loop,
        Self::LetElse,
        Self::Rhs,
        Self::Try,
        Self::Continuation,
    ];

    /// How a report and a fixture spell this kind.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Function => "fn",
            Self::Closure => "closure",
            Self::Then => "then",
            Self::Else => "else",
            Self::Arm => "arm",
            Self::Loop => "loop",
            Self::LetElse => "let-else",
            Self::Rhs => "rhs",
            Self::Try => "try",
            Self::Continuation => "rest",
        }
    }

    /// Whether a region of this kind is one side of a choice.
    #[must_use]
    pub fn is_branch(self) -> bool {
        matches!(
            self,
            Self::Then | Self::Else | Self::Arm | Self::LetElse | Self::Rhs | Self::Try
        )
    }

    fn code(self) -> u8 {
        Self::ALL.iter().position(|k| *k == self).expect("listed") as u8
    }

    fn from_code(code: u8) -> Option<Self> {
        Self::ALL.get(usize::from(code)).copied()
    }
}

/// A source position, 1-based.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Pos {
    pub line: u32,
    pub column: u32,
}

impl Pos {
    fn start(span: Span) -> Self {
        Self {
            line: span.line as u32,
            column: span.column as u32,
        }
    }

    fn end(span: Span) -> Self {
        Self {
            line: span.end_line as u32,
            column: span.end_column as u32,
        }
    }
}

/// One region of a module plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Region {
    pub kind: RegionKind,
    pub start: Pos,
    pub end: Pos,
    /// The index of the function in [`ModulePlan::functions`] holding it.
    pub function: u32,
    /// For a branch, where its choice is written and which side it is.
    pub choice: Option<(Pos, u32)>,
    /// The regions whose runs this one's runs are, when it takes no probe of
    /// its own: every run of it enters one of them. Empty for a probed region.
    pub derived: Vec<u32>,
}

/// One function of a module plan: a `fn`, a method, or a closure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedFunction {
    pub name: String,
    pub line: u32,
    /// Its body region.
    pub region: u32,
    /// What a profile's counts for it were recorded against: see
    /// [`function_hash`].
    pub hash: u64,
}

/// The regions a module's source implies, numbered from zero.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ModulePlan {
    /// The module's name, the same on any machine and from any entry of one
    /// package.
    pub name: String,
    /// Where the host reads the module, relative to the entry module's
    /// directory ([`ModuleSource::file`]); for a module that is no file, its
    /// name.
    pub path: String,
    pub functions: Vec<PlannedFunction>,
    pub regions: Vec<Region>,
    /// Each countable line with the innermost region of a statement starting
    /// on it, in source order. A line may appear once per region.
    pub lines: Vec<(u32, u32)>,
}

/// Where reify puts a probe, keyed by the node it reads there.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProbeSite {
    /// At the start of a block.
    BlockStart,
    /// Before a statement of a block.
    BeforeStmt,
    /// Around an expression, as `{ probe; expr }`.
    Around,
    /// In the `else` an `if` without one gets.
    ImplicitElse,
    /// On the early-return path of a `?`.
    TryExit,
}

/// The plans of every measured module of one compile, and the probe each site
/// takes. A probe id is global: module `i`'s region `r` is `bases[i] + r`.
#[derive(Debug, Clone, Default)]
pub struct CoverageMap {
    pub modules: Vec<ModulePlan>,
    pub bases: Vec<u32>,
    /// The module each of `modules` plans.
    sources: Vec<ModuleSource>,
    probes: IndexMap<(ProbeSite, AstId), u32>,
    /// The function holding each probed region, by global id.
    probe_function: IndexMap<u32, usize>,
    /// The probed regions of each function, in the order planned.
    function_probes: Vec<Vec<u32>>,
    /// The regions of each `for-of` body, by the loop's node, as global ids.
    for_of_bodies: IndexMap<AstId, Range<u32>>,
}

impl CoverageMap {
    /// Plan `modules` in the order given, which fixes the global ids. The
    /// stdlib modules `seed` planned keep its plans, ids and probe sites
    /// instead: they come from the stdlib snapshot with their probes already in
    /// place, and reify still reads their sites where it replays a default body
    /// of theirs in another module. The rest number past the seed's last id.
    pub fn build<'a>(
        seed: Option<&CoverageMap>,
        modules: impl IntoIterator<Item = (&'a ModuleSource, &'a ast::Module)>,
        rt_unreachable_calls: &IndexSet<AstId>,
    ) -> Self {
        let mut map = seed.map_or_else(Self::default, Self::without_entry);
        let seeded: IndexSet<ModuleSource> = map.sources.iter().cloned().collect();
        let mut next = map.end();
        for (source, module) in modules {
            if seeded.contains(source) {
                continue;
            }
            let PlannedModule {
                plan,
                sites,
                for_of_bodies,
                ..
            } = plan_module(source, module, rt_unreachable_calls);
            let base = next;
            next += plan.regions.len() as u32;
            let first_function = map.function_probes.len();
            map.function_probes
                .resize(first_function + plan.functions.len(), Vec::new());
            let probed = sites
                .into_iter()
                .filter(|&(_, _, region)| plan.regions[region as usize].derived.is_empty());
            for (site, id, region) in probed {
                let global = base + region;
                let function = first_function + plan.regions[region as usize].function as usize;
                map.probes.insert((site, id), global);
                map.probe_function.insert(global, function);
                map.function_probes[function].push(global);
            }
            for (id, regions) in for_of_bodies {
                map.for_of_bodies
                    .insert(id, base + regions.start..base + regions.end);
            }
            map.push(source, plan, base);
        }
        map
    }

    /// This map less its entry: the stdlib snapshot's, an empty one that every
    /// entry compares equal to.
    fn without_entry(&self) -> Self {
        let mut map = Self {
            probes: self.probes.clone(),
            probe_function: self.probe_function.clone(),
            function_probes: self.function_probes.clone(),
            for_of_bodies: self.for_of_bodies.clone(),
            ..Self::default()
        };
        for ((source, plan), &base) in self.sources.iter().zip(&self.modules).zip(&self.bases) {
            if let ModuleSource::EntryPoint { .. } = source {
                assert!(plan.regions.is_empty(), "the snapshot's entry is empty");
            } else {
                map.push(source, plan.clone(), base);
            }
        }
        map
    }

    fn push(&mut self, source: &ModuleSource, plan: ModulePlan, base: u32) {
        self.modules.push(plan);
        self.bases.push(base);
        self.sources.push(source.clone());
    }

    /// One past the last id any module takes.
    pub(crate) fn end(&self) -> u32 {
        self.modules
            .iter()
            .zip(&self.bases)
            .map(|(plan, base)| base + plan.regions.len() as u32)
            .max()
            .unwrap_or(0)
    }

    /// The probe id reify puts at `site` on node `id`, if one goes there.
    #[must_use]
    pub fn probe(&self, site: ProbeSite, id: AstId) -> Option<u32> {
        self.probes.get(&(site, id)).copied()
    }

    /// The function holding the probed region `probe`.
    #[must_use]
    pub fn function_of(&self, probe: u32) -> Option<usize> {
        self.probe_function.get(&probe).copied()
    }

    /// The probed regions of `function`.
    #[must_use]
    pub fn function_probes(&self, function: usize) -> &[u32] {
        &self.function_probes[function]
    }

    /// The regions of the body of the `for-of` `id`, none where its module
    /// is not measured.
    #[must_use]
    pub fn for_of_body(&self, id: AstId) -> Range<u32> {
        self.for_of_bodies.get(&id).map_or(0..0, Range::clone)
    }

    /// Whether no module is measured.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.modules.is_empty()
    }

    /// The custom section payload.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::default();
        w.u32(self.modules.len() as u32);
        for (plan, &base) in self.modules.iter().zip(&self.bases) {
            w.str(&plan.name);
            w.str(&plan.path);
            w.u32(base);
            w.u32(plan.functions.len() as u32);
            for f in &plan.functions {
                w.str(&f.name);
                w.u32(f.line);
                w.u32(f.region);
                w.u64(f.hash);
            }
            w.u32(plan.regions.len() as u32);
            for r in &plan.regions {
                w.0.push(r.kind.code());
                w.pos(r.start);
                w.pos(r.end);
                w.u32(r.function);
                match r.choice {
                    Some((at, side)) => {
                        w.0.push(1);
                        w.pos(at);
                        w.u32(side);
                    }
                    None => w.0.push(0),
                }
                w.u32(r.derived.len() as u32);
                for &child in &r.derived {
                    w.u32(child);
                }
            }
            w.u32(plan.lines.len() as u32);
            for &(line, region) in &plan.lines {
                w.u32(line);
                w.u32(region);
            }
        }
        w.0
    }
}

/// A decoded section: each module's plan with its first global id.
pub type DecodedPlans = Vec<(ModulePlan, u32)>;

/// Decode a section payload, or `None` if it is malformed.
#[must_use]
pub fn decode(data: &[u8]) -> Option<DecodedPlans> {
    let mut r = Reader { data, pos: 0 };
    let count = r.u32()?;
    let mut out = Vec::new();
    for _ in 0..count {
        let name = r.str()?;
        let path = r.str()?;
        let base = r.u32()?;
        let mut plan = ModulePlan {
            name,
            path,
            ..ModulePlan::default()
        };
        for _ in 0..r.u32()? {
            plan.functions.push(PlannedFunction {
                name: r.str()?,
                line: r.u32()?,
                region: r.u32()?,
                hash: r.u64()?,
            });
        }
        for _ in 0..r.u32()? {
            let kind = RegionKind::from_code(r.u8()?)?;
            let start = r.pos()?;
            let end = r.pos()?;
            let function = r.u32()?;
            let choice = match r.u8()? {
                0 => None,
                _ => Some((r.pos()?, r.u32()?)),
            };
            let mut derived = Vec::new();
            for _ in 0..r.u32()? {
                derived.push(r.u32()?);
            }
            plan.regions.push(Region {
                kind,
                start,
                end,
                function,
                choice,
                derived,
            });
        }
        for _ in 0..r.u32()? {
            plan.lines.push((r.u32()?, r.u32()?));
        }
        out.push((plan, base));
    }
    (r.pos == data.len()).then_some(out)
}

/// Find and decode the coverage section in a component binary.
#[must_use]
pub fn read_from_component(component_bytes: &[u8]) -> Option<DecodedPlans> {
    for payload in wasmparser::Parser::new(0).parse_all(component_bytes) {
        if let Ok(wasmparser::Payload::CustomSection(reader)) = payload
            && reader.name() == SECTION_NAME
        {
            return decode(reader.data());
        }
    }
    None
}

#[derive(Default)]
struct Writer(Vec<u8>);

impl Writer {
    fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }

    fn u64(&mut self, v: u64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }

    fn pos(&mut self, p: Pos) {
        self.u32(p.line);
        self.u32(p.column);
    }

    fn str(&mut self, s: &str) {
        self.u32(s.len() as u32);
        self.0.extend_from_slice(s.as_bytes());
    }
}

struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl Reader<'_> {
    fn u8(&mut self) -> Option<u8> {
        let b = *self.data.get(self.pos)?;
        self.pos += 1;
        Some(b)
    }

    fn u32(&mut self) -> Option<u32> {
        let bytes = self.data.get(self.pos..self.pos + 4)?;
        self.pos += 4;
        Some(u32::from_le_bytes(bytes.try_into().ok()?))
    }

    fn u64(&mut self) -> Option<u64> {
        let bytes = self.data.get(self.pos..self.pos + 8)?;
        self.pos += 8;
        Some(u64::from_le_bytes(bytes.try_into().ok()?))
    }

    fn pos(&mut self) -> Option<Pos> {
        Some(Pos {
            line: self.u32()?,
            column: self.u32()?,
        })
    }

    fn str(&mut self) -> Option<String> {
        let len = self.u32()? as usize;
        let bytes = self.data.get(self.pos..self.pos + len)?;
        self.pos += len;
        String::from_utf8(bytes.to_vec()).ok()
    }
}

// ─────────────────────────────────────────────────────────────────
// Planning
// ─────────────────────────────────────────────────────────────────

type Sites = Vec<(ProbeSite, AstId, u32)>;

/// The regions each `for-of` body holds, by the loop's node.
type ForOfBodies = Vec<(AstId, Range<u32>)>;

/// A cold marker the source states, which a profile can contradict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Marker {
    /// The author's `builtin::cold_path()`.
    ColdPath,
    /// The one reify puts on the failure path of the `?` node.
    Try(AstId),
}

/// One module planned, and where its regions lie in the source.
pub(crate) struct PlannedModule {
    pub plan: ModulePlan,
    /// The site of each region, derived or not.
    pub sites: Sites,
    pub for_of_bodies: ForOfBodies,
    /// The region each region runs inside, `None` for a function's body: a
    /// region runs only while its parent does.
    pub parents: Vec<Option<u32>>,
    /// Where each function's name, or a closure, is written.
    pub names: Vec<Span>,
    /// Each cold marker, with the region whose runs reach it.
    pub markers: Vec<(Marker, Span, u32)>,
}

/// Plan one module: its regions, the site each region's probe goes at, and
/// the regions of each `for-of` body. `rt_unreachable_calls` holds the callee
/// sites that name `core:rt`'s `unreachable`.
#[must_use]
pub(crate) fn plan_module(
    source: &ModuleSource,
    module: &ast::Module,
    rt_unreachable_calls: &IndexSet<AstId>,
) -> PlannedModule {
    let mut planner = Planner {
        plan: ModulePlan {
            name: source.to_string(),
            path: source.file().unwrap_or_else(|| source.to_string()),
            ..ModulePlan::default()
        },
        rt_unreachable_calls,
        sites: Vec::new(),
        parents: Vec::new(),
        names: Vec::new(),
        markers: Vec::new(),
        function: 0,
        region: None,
        owner: None,
        last_choice: Vec::new(),
        for_of_bodies: Vec::new(),
        texts: Vec::new(),
    };
    if !module.has_coverage_off() {
        for item in &module.items {
            planner.item(item);
        }
    }
    planner.plan.lines.sort_unstable();
    planner.plan.lines.dedup();
    let Planner {
        mut plan,
        sites,
        parents,
        names,
        markers,
        for_of_bodies,
        texts,
        ..
    } = planner;
    for (index, ids) in plan.regions_by_function().iter().enumerate() {
        let regions = ids.iter().map(|&id| &plan.regions[id as usize]);
        plan.functions[index].hash = function_hash(module.source(), &texts[index], regions);
    }
    PlannedModule {
        plan,
        sites,
        for_of_bodies,
        parents,
        names,
        markers,
    }
}

struct Planner<'a> {
    plan: ModulePlan,
    rt_unreachable_calls: &'a IndexSet<AstId>,
    sites: Sites,
    parents: Vec<Option<u32>>,
    names: Vec<Span>,
    markers: Vec<(Marker, Span, u32)>,
    function: u32,
    /// The innermost region the walk is in, `None` outside a function.
    region: Option<u32>,
    /// The type or trait a method is declared under.
    owner: Option<String>,
    /// The sides of the choice the walk finished last.
    last_choice: Vec<u32>,
    for_of_bodies: ForOfBodies,
    /// Where each function's text lies, from its name to the end of its body.
    texts: Vec<FunctionText>,
}

struct FunctionText {
    bytes: Range<usize>,
    start: Pos,
}

/// What a profile's counts for a function were recorded against: its text,
/// and each of its `regions`' kind and span relative to where the text starts.
/// The text catches an edit that changes which region runs, the regions what
/// decides them from outside the text, such as a `#[coverage(off)]` on the
/// enclosing `impl`. See WEP 2026-10-25 (Profile-Guided Optimization).
fn function_hash<'r>(
    source: &str,
    text: &FunctionText,
    regions: impl Iterator<Item = &'r Region>,
) -> u64 {
    let mut h = Sha256::new();
    h.update(normalize_line_breaks(&source[text.bytes.clone()]).as_bytes());
    for region in regions {
        h.update([region.kind.code()]);
        for pos in [region.start, region.end] {
            h.update((pos.line - text.start.line).to_le_bytes());
            h.update(pos.column.to_le_bytes());
        }
    }
    let digest = h.finalize();
    u64::from_le_bytes(
        digest[..8]
            .try_into()
            .expect("a digest is longer than 8 bytes"),
    )
}

impl Planner<'_> {
    fn item(&mut self, item: &Item) {
        match item {
            Item::Function(f) => self.function(f),
            Item::Impl(i) if !attribute::coverage_off(&i.attrs) => {
                self.methods(ast::type_head_name(&i.ty), &i.methods);
            }
            Item::Trait(t) => self.methods(Some(&t.name), &t.methods),
            Item::Interface(e) => self.methods(Some(&e.name), &e.methods),
            Item::Resource(r) => self.methods(Some(&r.name), &r.methods),
            _ => {}
        }
    }

    fn methods(&mut self, owner: Option<&str>, methods: &[Function]) {
        let saved = std::mem::replace(&mut self.owner, owner.map(str::to_string));
        for m in methods {
            self.function(m);
        }
        self.owner = saved;
    }

    fn function(&mut self, f: &Function) {
        let Some(body) = &f.body else { return };
        // A contract clause is no coverage target, and a holder is nothing else.
        if f.is_contract_holder()
            || attribute::coverage_off(&f.attrs)
            || self.reaches_only_unreachable(&body.stmts, false)
        {
            return;
        }
        let name = match &self.owner {
            Some(owner) => format!("{owner}::{}", f.name),
            None => f.name.clone(),
        };
        // A local item nests a function in another, so the walk's position is
        // saved around it.
        let saved = (self.function, self.region);
        let region = self.new_function(name, f.name_span, RegionKind::Function, body.span);
        self.sites.push((ProbeSite::BlockStart, body.id, region));
        self.block_of(region, body, false);
        (self.function, self.region) = saved;
    }

    fn new_function(&mut self, name: String, at: Span, kind: RegionKind, body: Span) -> u32 {
        self.function = self.plan.functions.len() as u32;
        self.plan.functions.push(PlannedFunction {
            name,
            line: at.line as u32,
            region: self.plan.regions.len() as u32,
            hash: 0,
        });
        self.texts.push(FunctionText {
            bytes: at.start..body.end,
            start: Pos::start(at),
        });
        self.names.push(at);
        let region = self.new_region(kind, Pos::start(body), Pos::end(body), None);
        self.parents[region as usize] = None;
        region
    }

    fn new_region(
        &mut self,
        kind: RegionKind,
        start: Pos,
        end: Pos,
        choice: Option<(Pos, u32)>,
    ) -> u32 {
        let region = self.plan.regions.len() as u32;
        self.plan.regions.push(Region {
            kind,
            start,
            end,
            function: self.function,
            choice,
            derived: Vec::new(),
        });
        self.parents.push(self.region);
        region
    }

    /// Walk `visit` inside a new region, and return to the current one.
    fn in_region(&mut self, region: u32, visit: impl FnOnce(&mut Self)) {
        let saved = self.region.replace(region);
        visit(self);
        self.region = saved;
    }

    fn line(&mut self, span: Span) {
        let region = self
            .region
            .expect("a statement is planned inside a function");
        self.plan.lines.push((span.line as u32, region));
    }

    /// Whether `expr` calls `unreachable` with nothing to evaluate first: its
    /// author's claim that no run reaches it. `panic` and `assert` are not
    /// such a claim, since a caller breaking a contract reaches them.
    fn is_unreachable_call(&self, expr: &Expr) -> bool {
        let Expr::Call(call) = expr else {
            return false;
        };
        is_builtin_call(call, "unreachable")
            || (call.args.is_empty()
                && call.type_args.is_empty()
                && matches!(&call.callee, Expr::Ident(ident) if self.rt_unreachable_calls.contains(&ident.id)))
    }

    /// Whether a run entering `stmts` reaches nothing but an
    /// [`unreachable` call](Self::is_unreachable_call), where `after` says the
    /// same of what control reaches on leaving them. Such a run is no path to
    /// plan.
    fn reaches_only_unreachable(&self, stmts: &[Stmt], after: bool) -> bool {
        match stmts {
            [] => after,
            [Stmt::Expr(s)] => self.expr_reaches_only_unreachable(&s.expr, after),
            [Stmt::Return(r)] => r
                .value
                .as_ref()
                .is_some_and(|v| self.is_unreachable_call(v)),
            _ => false,
        }
    }

    /// [`Self::reaches_only_unreachable`] for a body written as an expression.
    fn expr_reaches_only_unreachable(&self, body: &Expr, after: bool) -> bool {
        match body {
            Expr::Block(block) => self.reaches_only_unreachable(&block.stmts, after),
            _ => self.is_unreachable_call(body),
        }
    }

    /// The region `block` is, unless it
    /// [reaches only `unreachable`](Self::reaches_only_unreachable).
    fn branch_block(
        &mut self,
        kind: RegionKind,
        block: &Block,
        choice: (Pos, u32),
        after: bool,
    ) -> Option<u32> {
        if self.reaches_only_unreachable(&block.stmts, after) {
            return None;
        }
        let region = self.new_region(
            kind,
            Pos::start(block.span),
            Pos::end(block.span),
            Some(choice),
        );
        self.sites.push((ProbeSite::BlockStart, block.id, region));
        self.block_of(region, block, after);
        Some(region)
    }

    /// An `if`, statement or expression, which control leaves to what `after`
    /// describes. Both branches are regions, and an omitted `else` is one too:
    /// the path that skips the `then` block.
    fn if_branches(
        &mut self,
        id: AstId,
        span: Span,
        then_block: &Block,
        else_block: Option<&Block>,
        after: bool,
    ) {
        let at = Pos::start(span);
        let then_region = self.branch_block(RegionKind::Then, then_block, (at, 0), after);
        let else_region = if let Some(block) = else_block {
            self.branch_block(RegionKind::Else, block, (at, 1), after)
        } else if after {
            None
        } else {
            let region = self.new_region(
                RegionKind::Else,
                Pos::end(span),
                Pos::end(span),
                Some((at, 1)),
            );
            self.sites.push((ProbeSite::ImplicitElse, id, region));
            Some(region)
        };
        self.last_choice = then_region.into_iter().chain(else_region).collect();
    }

    /// An operand that runs only when the ones before it allow, counted in a
    /// region of its own. It is keyed at its own start, which no other operand
    /// shares: `a && b && c` nests `a && b` inside, starting where it does.
    fn conditional_operand(&mut self, operand: &Expr) {
        if self.is_unreachable_call(operand) {
            return self.visit_expr(operand);
        }
        let span = operand.span();
        let start = Pos::start(span);
        let region = self.new_region(RegionKind::Rhs, start, Pos::end(span), Some((start, 0)));
        self.sites.push((ProbeSite::Around, operand.id(), region));
        self.in_region(region, |p| p.visit_expr(operand));
    }

    fn if_stmt(&mut self, s: &ast::IfStmt, after: bool) {
        self.visit_condition(&s.condition);
        self.if_branches(s.id, s.span, &s.then_block, s.else_block.as_ref(), after);
    }

    /// A `match`, statement or expression, which control leaves to what
    /// `after` describes.
    fn match_arms(&mut self, m: &MatchExpr, after: bool) {
        self.visit_expr(&m.expr);
        let at = Pos::start(m.span);
        let mut sides = Vec::new();
        for (side, arm) in m.arms.iter().enumerate() {
            if let Some(guard) = &arm.guard {
                self.visit_expr(guard);
            }
            if self.expr_reaches_only_unreachable(&arm.body, after) {
                continue;
            }
            let body = arm.body.span();
            let region = self.new_region(
                RegionKind::Arm,
                Pos::start(body),
                Pos::end(body),
                Some((at, side as u32)),
            );
            self.sites.push((ProbeSite::Around, arm.body.id(), region));
            self.expr_body(region, &arm.body, after);
            sides.push(region);
        }
        self.last_choice = sides;
    }

    /// The body of `region` written as an expression: a block counts its
    /// statements, any other expression is one line.
    fn expr_body(&mut self, region: u32, body: &Expr, after: bool) {
        match body {
            Expr::Block(block) => self.block_of(region, block, after),
            _ => self.in_region(region, |p| {
                p.line(body.span());
                p.visit_expr(body);
            }),
        }
    }

    fn loop_body(&mut self, body: &Block) {
        if self.reaches_only_unreachable(&body.stmts, false) {
            return;
        }
        let region = self.new_region(
            RegionKind::Loop,
            Pos::start(body.span),
            Pos::end(body.span),
            None,
        );
        self.sites.push((ProbeSite::BlockStart, body.id, region));
        self.block_of(region, body, false);
    }

    /// `block`, walked inside `region`, which it starts.
    fn block_of(&mut self, region: u32, block: &Block, after: bool) {
        self.statements(block, Some(region), after);
    }

    /// Walk `block`'s statements, which control leaves to what `after`
    /// describes. A run of statements that some region starts with, `open`,
    /// derives that region from the first choice it reaches, when nothing
    /// before the choice can leave the block.
    fn statements(&mut self, block: &Block, mut open: Option<u32>, after: bool) {
        let saved = self.region;
        if open.is_some() {
            self.region = open;
        }
        let mut leaves = false;
        for (index, stmt) in block.stmts.iter().enumerate() {
            if stmt.as_contract().is_some() {
                continue;
            }
            if leaves && self.reaches_only_unreachable(&block.stmts[index..], after) {
                break;
            }
            if leaves {
                let region = self.new_region(
                    RegionKind::Continuation,
                    Pos::start(stmt.span()),
                    Pos::end(block.span),
                    None,
                );
                self.sites.push((ProbeSite::BeforeStmt, stmt.id(), region));
                self.region = Some(region);
                open = Some(region);
            }
            if !matches!(stmt, Stmt::Item(_) | Stmt::Error(_)) {
                self.line(stmt.span());
            }
            let rest = self.reaches_only_unreachable(&block.stmts[index + 1..], after);
            match stmt {
                Stmt::If(s) => self.if_stmt(s, rest),
                Stmt::Match(m) => self.match_arms(m, rest),
                _ => self.visit_stmt(stmt),
            }
            leaves = leaves_early(stmt);
            if let Some(region) = open {
                if enters_choice(stmt) {
                    // The choice `stmt` is finishes after any nested in it, so
                    // its sides are the last recorded. A `match` with no arm
                    // has none, and never completes.
                    let sides = std::mem::take(&mut self.last_choice);
                    assert!(sides.iter().all(|&side| side > region));
                    self.plan.regions[region as usize].derived = sides;
                    open = None;
                } else if leaves {
                    open = None;
                }
            }
        }
        self.region = saved;
    }
}

impl AstVisitor<'_> for Planner<'_> {
    fn visit_item(&mut self, item: &Item) {
        self.item(item);
    }

    fn visit_block(&mut self, block: &Block) {
        self.statements(block, None, false);
    }

    fn visit_stmt(&mut self, stmt: &Stmt) {
        match stmt {
            Stmt::Let(s) => {
                if let Some(value) = &s.value {
                    self.visit_expr(value);
                }
                if let Some(block) = s
                    .else_block
                    .as_ref()
                    .filter(|b| !self.reaches_only_unreachable(&b.stmts, false))
                {
                    let region = self.new_region(
                        RegionKind::LetElse,
                        Pos::start(block.span),
                        Pos::end(block.span),
                        Some((Pos::start(s.span), 0)),
                    );
                    self.sites.push((ProbeSite::BlockStart, block.id, region));
                    self.block_of(region, block, false);
                }
            }
            Stmt::If(s) => self.if_stmt(s, false),
            Stmt::While(s) => {
                self.visit_condition(&s.condition);
                self.loop_body(&s.body);
            }
            Stmt::For(s) => {
                if let Some(init) = &s.init {
                    self.visit_stmt(init);
                }
                if let Some(cond) = &s.condition {
                    self.visit_condition(cond);
                }
                if let Some(update) = &s.update {
                    self.visit_expr(update);
                }
                self.loop_body(&s.body);
            }
            Stmt::ForOf(s) => {
                self.visit_expr(&s.iterable);
                let first = self.plan.regions.len() as u32;
                self.loop_body(&s.body);
                self.for_of_bodies
                    .push((s.id, first..self.plan.regions.len() as u32));
            }
            Stmt::Loop(s) => self.loop_body(&s.body),
            Stmt::Match(m) => self.match_arms(m, false),
            // Power-assert rewrites the condition, so its operands are not
            // the source's regions.
            Stmt::Assert(_) => {}
            _ => walk_stmt(self, stmt),
        }
    }

    /// A let chain's operands after the first may not run, as the right side
    /// of `&&` may not.
    fn visit_condition(&mut self, cond: &Condition) {
        let Condition::LetChain { elements, .. } = cond else {
            return walk_condition(self, cond);
        };
        for (index, element) in elements.iter().enumerate() {
            if index == 0 {
                self.visit_expr(element.expr());
            } else {
                self.conditional_operand(element.expr());
            }
            if let ConditionElement::Let { pattern, .. } = element {
                self.visit_pattern(pattern);
            }
        }
    }

    fn visit_expr(&mut self, expr: &Expr) {
        match expr {
            Expr::Binary(b) if b.op.is_short_circuit() => {
                self.visit_expr(&b.left);
                self.conditional_operand(&b.right);
            }
            Expr::If(e) => {
                self.visit_condition(&e.condition);
                self.if_branches(e.id, e.span, &e.then_block, e.else_block.as_ref(), false);
            }
            Expr::Match(m) => self.match_arms(m, false),
            Expr::Closure(c) if self.expr_reaches_only_unreachable(&c.body, false) => {}
            Expr::Closure(c) => {
                let outer_name = &self.plan.functions[self.function as usize].name;
                let name = format!("{outer_name}::{{closure:{}}}", c.span.line);
                let saved = (self.function, self.region);
                let region = self.new_function(name, c.span, RegionKind::Closure, c.body.span());
                self.sites.push((ProbeSite::Around, c.body.id(), region));
                self.expr_body(region, &c.body, false);
                (self.function, self.region) = saved;
            }
            Expr::TryOp(t) => {
                self.visit_expr(&t.expr);
                let region = self.new_region(
                    RegionKind::Try,
                    Pos::start(t.span),
                    Pos::end(t.span),
                    Some((Pos::end(t.span), 0)),
                );
                self.sites.push((ProbeSite::TryExit, t.id, region));
                self.markers.push((Marker::Try(t.id), t.span, region));
            }
            Expr::Call(call) if is_builtin_call(call, "cold_path") => {
                if let Some(region) = self.region {
                    self.markers.push((Marker::ColdPath, call.span, region));
                }
            }
            _ => walk_expr(self, expr),
        }
    }
}

/// Whether `stmt` can leave its block before the next statement: a `return`,
/// `break`, `continue`, `resume` or `?` outside a closure. A `break` of a
/// loop nested in `stmt` counts too, which costs a probe and loses nothing.
fn leaves_early(stmt: &Stmt) -> bool {
    Leaves::find(|l| l.visit_stmt(stmt))
}

/// Whether evaluating `expr` can leave its block, as [`leaves_early`] asks.
fn may_leave(expr: &Expr) -> bool {
    Leaves::find(|l| l.visit_expr(expr))
}

/// Whether every run of `stmt` enters one side of the choice it is: an `if`
/// or a `match`, alone or as the value a `let` binds or a `return` returns,
/// whose condition, scrutinee and guards cannot leave first.
fn enters_choice(stmt: &Stmt) -> bool {
    let choice = match stmt {
        Stmt::If(s) => return !Leaves::find(|l| l.visit_condition(&s.condition)),
        Stmt::Match(m) => return !match_head_may_leave(m),
        Stmt::Expr(s) => &s.expr,
        Stmt::Let(s) if s.else_block.is_none() => match &s.value {
            Some(value) => value,
            None => return false,
        },
        Stmt::Return(r) => match &r.value {
            Some(value) => value,
            None => return false,
        },
        _ => return false,
    };
    match choice {
        Expr::If(e) => !Leaves::find(|l| l.visit_condition(&e.condition)),
        Expr::Match(m) => !match_head_may_leave(m),
        _ => false,
    }
}

/// Whether `call` is `builtin::<name>()`, with no argument and no turbofish.
/// `builtin::` names `core:builtin` whatever the module declares, so the
/// spelling identifies the callee.
fn is_builtin_call(call: &ast::CallExpr, name: &str) -> bool {
    call.args.is_empty()
        && call.type_args.is_empty()
        && matches!(&call.callee, Expr::Ident(ident)
            if ident.name.strip_prefix("builtin::") == Some(name))
}

fn match_head_may_leave(m: &MatchExpr) -> bool {
    may_leave(&m.expr)
        || m.arms
            .iter()
            .any(|arm| arm.guard.as_ref().is_some_and(may_leave))
}

/// Finds what can leave the enclosing block: see [`leaves_early`].
struct Leaves(bool);

impl Leaves {
    fn find(visit: impl FnOnce(&mut Self)) -> bool {
        let mut leaves = Self(false);
        visit(&mut leaves);
        leaves.0
    }
}

impl AstVisitor<'_> for Leaves {
    fn visit_stmt(&mut self, stmt: &Stmt) {
        match stmt {
            Stmt::Return(_) | Stmt::Break(_) | Stmt::Continue(_) => self.0 = true,
            Stmt::Item(_) => {}
            _ => walk_stmt(self, stmt),
        }
    }

    fn visit_expr(&mut self, expr: &Expr) {
        match expr {
            Expr::TryOp(_) => self.0 = true,
            Expr::Closure(_) => {}
            _ => walk_expr(self, expr),
        }
    }
}

// ─────────────────────────────────────────────────────────────────
// Reports
// ─────────────────────────────────────────────────────────────────

/// Plans and hits merged across every compile of one run, keyed by path.
#[derive(Debug, Clone, Default)]
pub struct Coverage {
    pub files: BTreeMap<String, FileCoverage>,
}

/// One module's plan and how often the tests ran each region.
#[derive(Debug, Clone, Default)]
pub struct FileCoverage {
    pub plan: ModulePlan,
    /// The runs of each region that ran; a region absent never ran.
    pub counts: BTreeMap<u32, i64>,
    /// The tests that ran each region, by region.
    pub tests: BTreeMap<u32, BTreeSet<String>>,
}

/// Why two plans cannot be merged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanMismatch {
    pub path: String,
}

/// Where the hits of one registered plan land: the path its file is reported
/// under, and its global ids.
#[derive(Debug, Clone)]
pub struct Registered {
    path: String,
    ids: Range<u32>,
}

impl Coverage {
    /// Add one compile's plans, each under `key(plan)` and counting every
    /// region whether any test runs it.
    pub fn register(
        &mut self,
        plans: &DecodedPlans,
        mut key: impl FnMut(&ModulePlan) -> String,
    ) -> Result<Vec<Registered>, PlanMismatch> {
        plans
            .iter()
            .map(|(plan, base)| {
                let plan = ModulePlan {
                    path: key(plan),
                    ..plan.clone()
                };
                let ids = *base..base + plan.regions.len() as u32;
                let file = self
                    .files
                    .entry(plan.path.clone())
                    .or_insert_with(|| FileCoverage {
                        plan: plan.clone(),
                        counts: BTreeMap::new(),
                        tests: BTreeMap::new(),
                    });
                if file.plan != plan {
                    return Err(PlanMismatch { path: plan.path });
                }
                Ok(Registered {
                    path: plan.path,
                    ids,
                })
            })
            .collect()
    }

    /// Record the runs `test` counted, by global id, in the plans `registered`
    /// names.
    pub fn record(
        &mut self,
        registered: &[Registered],
        counts: &BTreeMap<u32, i64>,
        test: Option<&str>,
    ) {
        for Registered { path, ids } in registered {
            let file = self.files.get_mut(path).expect("`register` added the file");
            let mut local: BTreeMap<u32, i64> = counts
                .range(ids.clone())
                .map(|(id, count)| (id - ids.start, *count))
                .collect();
            file.plan.derive(&mut local);
            for (region, count) in local {
                if let Some(test) = test {
                    file.tests
                        .entry(region)
                        .or_default()
                        .insert(test.to_string());
                }
                let total = file.counts.entry(region).or_default();
                *total = total.saturating_add(count);
            }
        }
    }
}

/// What one test reported through `core:coverage`, by global id: each region as
/// it first ran, and, once the test returned, the runs of each that ran more
/// than once.
#[derive(Debug, Default)]
pub struct TestReport {
    hits: BTreeSet<u32>,
    repeated: BTreeMap<u32, i64>,
}

impl TestReport {
    pub fn hit(&mut self, id: u32) {
        self.hits.insert(id);
    }

    pub fn count(&mut self, id: u32, runs: i64) {
        self.repeated.insert(id, runs);
    }

    /// The runs of each region the test counted: each region it hit ran once,
    /// unless it reported more. A test that traps reports no more. Test code can
    /// call `core:coverage` itself, so a region counted but never hit counts
    /// too, and a count runs at least once.
    #[must_use]
    pub fn counts(&self) -> BTreeMap<u32, i64> {
        let mut counts: BTreeMap<u32, i64> = self.hits.iter().map(|&id| (id, 1)).collect();
        for (&id, &runs) in &self.repeated {
            counts.insert(id, runs.max(1));
        }
        counts
    }
}

impl ModulePlan {
    /// Each function's region ids in plan order, by function; the first is
    /// its body.
    #[must_use]
    pub fn regions_by_function(&self) -> Vec<Vec<u32>> {
        let mut out = vec![Vec::new(); self.functions.len()];
        for (id, region) in self.regions.iter().enumerate() {
            out[region.function as usize].push(id as u32);
        }
        for (function, ids) in self.functions.iter().zip(&out) {
            assert_eq!(
                ids.first(),
                Some(&function.region),
                "a function's body is its first region"
            );
        }
        out
    }

    /// Count each derived region as the sum of its children: every run of it
    /// enters exactly one. A child is numbered after its parent, so one pass
    /// from the last region reaches a derived region's children before it.
    pub fn derive(&self, counts: &mut BTreeMap<u32, i64>) {
        for (index, region) in self.regions.iter().enumerate().rev() {
            let runs = region
                .derived
                .iter()
                .filter_map(|child| counts.get(child))
                .fold(0, |sum: i64, count| sum.saturating_add(*count));
            if runs > 0 {
                counts.insert(index as u32, runs);
            }
        }
    }
}

impl FileCoverage {
    /// How often region `region` ran.
    #[must_use]
    pub fn count(&self, region: u32) -> i64 {
        self.counts.get(&region).copied().unwrap_or(0)
    }

    #[must_use]
    pub fn ran(&self, region: u32) -> bool {
        self.counts.contains_key(&region)
    }

    /// Each countable line and how often the busiest statement on it ran.
    #[must_use]
    pub fn lines(&self) -> BTreeMap<u32, i64> {
        let mut out = BTreeMap::new();
        for &(line, region) in &self.plan.lines {
            let count = out.entry(line).or_insert(0);
            *count = (*count).max(self.count(region));
        }
        out
    }

    /// The countable lines no statement ran on, in order.
    #[must_use]
    pub fn uncovered_lines(&self) -> Vec<u32> {
        self.lines()
            .into_iter()
            .filter_map(|(line, count)| (count == 0).then_some(line))
            .collect()
    }

    /// Each branch region, in source order: where its choice is written, its
    /// side, its kind, and how often it ran.
    #[must_use]
    pub fn branches(&self) -> Vec<Branch> {
        let mut out: Vec<Branch> = self
            .plan
            .regions
            .iter()
            .enumerate()
            .filter_map(|(index, region)| {
                let (at, side) = region.choice?;
                Some(Branch {
                    at,
                    side,
                    kind: region.kind,
                    count: self.count(index as u32),
                })
            })
            .collect();
        out.sort_by_key(|b| (b.at, b.side));
        out
    }

    /// Each function and how often its body ran.
    #[must_use]
    pub fn functions(&self) -> Vec<(&PlannedFunction, i64)> {
        self.plan
            .functions
            .iter()
            .map(|f| (f, self.count(f.region)))
            .collect()
    }
}

/// One side of a choice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Branch {
    pub at: Pos,
    pub side: u32,
    pub kind: RegionKind,
    pub count: i64,
}

impl Branch {
    #[must_use]
    pub fn taken(&self) -> bool {
        self.count > 0
    }

    /// How a fixture and a report name it: `12:5 else`, `20:9 arm 2`.
    #[must_use]
    pub fn describe(&self) -> String {
        let Pos { line, column } = self.at;
        match self.kind {
            RegionKind::Arm => format!("{line}:{column} arm {}", self.side),
            kind => format!("{line}:{column} {}", kind.label()),
        }
    }
}

/// The LCOV trace for `coverage`, one `SF` record per file.
#[must_use]
pub fn to_lcov(coverage: &Coverage) -> String {
    use std::fmt::Write;

    let mut out = String::new();
    for (path, file) in &coverage.files {
        let _ = writeln!(out, "TN:");
        let _ = writeln!(out, "SF:{path}");
        let functions = file.functions();
        for (f, _) in &functions {
            let _ = writeln!(out, "FN:{},{}", f.line, f.name);
        }
        for (f, count) in &functions {
            let _ = writeln!(out, "FNDA:{count},{}", f.name);
        }
        let _ = writeln!(out, "FNF:{}", functions.len());
        let _ = writeln!(
            out,
            "FNH:{}",
            functions.iter().filter(|(_, count)| *count > 0).count()
        );
        let branches = file.branches();
        let mut blocks: Vec<Pos> = branches.iter().map(|b| b.at).collect();
        blocks.dedup();
        for b in &branches {
            let block = blocks
                .iter()
                .position(|at| *at == b.at)
                .expect("`blocks` lists every branch's choice");
            let _ = writeln!(out, "BRDA:{},{block},{},{}", b.at.line, b.side, b.count);
        }
        let _ = writeln!(out, "BRF:{}", branches.len());
        let _ = writeln!(out, "BRH:{}", branches.iter().filter(|b| b.taken()).count());
        let lines = file.lines();
        for (line, count) in &lines {
            let _ = writeln!(out, "DA:{line},{count}");
        }
        let _ = writeln!(out, "LF:{}", lines.len());
        let _ = writeln!(
            out,
            "LH:{}",
            lines.values().filter(|count| **count > 0).count()
        );
        let _ = writeln!(out, "end_of_record");
    }
    out
}

/// `plan` as `wado dump --coverage-plan` prints it.
#[must_use]
pub fn render_plan(plan: &ModulePlan) -> String {
    use std::fmt::Write;

    let mut out = String::new();
    let _ = writeln!(out, "coverage plan: {}", plan.path);
    for (index, region) in plan.regions.iter().enumerate() {
        let function = &plan.functions[region.function as usize];
        let choice = region.choice.map_or(String::new(), |(at, side)| {
            format!(" choice {}:{} side {side}", at.line, at.column)
        });
        let derived = if region.derived.is_empty() {
            String::new()
        } else {
            let children: Vec<String> = region.derived.iter().map(|c| format!("r{c}")).collect();
            format!(" derived from {}", children.join(" "))
        };
        let _ = writeln!(
            out,
            "  r{index} {} {}:{}-{}:{} ({}){choice}{derived}",
            region.kind.label(),
            region.start.line,
            region.start.column,
            region.end.line,
            region.end.column,
            function.name,
        );
    }
    let lines: Vec<String> = plan
        .lines
        .iter()
        .map(|(line, r)| format!("{line}:r{r}"))
        .collect();
    let _ = writeln!(out, "  lines {}", lines.join(" "));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::lex;
    use crate::parser::Parser;

    /// `fn f` whose body derives from an `if` and its omitted `else`.
    fn plan() -> ModulePlan {
        ModulePlan {
            name: "a.wado".to_string(),
            path: "./a.wado".to_string(),
            functions: vec![PlannedFunction {
                name: "f".to_string(),
                line: 1,
                region: 0,
                hash: 0x0123_4567_89ab_cdef,
            }],
            regions: vec![
                Region {
                    kind: RegionKind::Function,
                    start: Pos {
                        line: 1,
                        column: 10,
                    },
                    end: Pos { line: 5, column: 2 },
                    function: 0,
                    choice: None,
                    derived: vec![1, 2],
                },
                Region {
                    kind: RegionKind::Then,
                    start: Pos {
                        line: 2,
                        column: 10,
                    },
                    end: Pos { line: 4, column: 6 },
                    function: 0,
                    choice: Some((Pos { line: 2, column: 5 }, 0)),
                    derived: Vec::new(),
                },
                Region {
                    kind: RegionKind::Else,
                    start: Pos { line: 4, column: 6 },
                    end: Pos { line: 4, column: 6 },
                    function: 0,
                    choice: Some((Pos { line: 2, column: 5 }, 1)),
                    derived: Vec::new(),
                },
            ],
            lines: vec![(2, 0), (3, 1)],
        }
    }

    #[test]
    fn section_round_trips() {
        let plan = plan();
        let map = CoverageMap {
            modules: vec![plan.clone()],
            bases: vec![7],
            ..CoverageMap::default()
        };
        assert_eq!(decode(&map.encode()), Some(vec![(plan, 7)]));
    }

    #[test]
    fn a_derived_region_runs_as_often_as_its_children() {
        let plan = plan();
        let mut counts = BTreeMap::from([(1, 3), (2, 4)]);
        plan.derive(&mut counts);
        assert_eq!(counts, BTreeMap::from([(0, 7), (1, 3), (2, 4)]));
        let mut none = BTreeMap::new();
        plan.derive(&mut none);
        assert!(none.is_empty());
    }

    #[test]
    fn lcov_carries_the_counts_of_every_test() {
        let plan = plan();
        let mut coverage = Coverage::default();
        let registered = coverage
            .register(&vec![(plan, 0)], |plan| plan.name.clone())
            .expect("one plan");
        coverage.record(&registered, &BTreeMap::from([(1, 3)]), None);
        let mut trapped = TestReport::default();
        trapped.hit(1);
        coverage.record(&registered, &trapped.counts(), None);
        let lcov = to_lcov(&coverage);
        for line in [
            "FNDA:4,f",
            "BRDA:2,0,0,4",
            "BRDA:2,0,1,0",
            "DA:2,4",
            "DA:3,4",
        ] {
            assert!(lcov.lines().any(|l| l == line), "{line} in\n{lcov}");
        }
    }

    #[test]
    fn a_report_takes_what_test_code_forged() {
        let mut forged = TestReport::default();
        forged.count(3, 7);
        forged.count(4, -2);
        assert_eq!(forged.counts(), BTreeMap::from([(3, 7), (4, 1)]));
    }

    fn parse(source: &str) -> ast::Module {
        let lexed = lex(source);
        assert!(lexed.errors.is_empty(), "lex error: {:?}", lexed.errors);
        Parser::from_lex(lexed).parse_strict().expect("parse")
    }

    /// Each function's hash in `source`, by name.
    fn hashes(source: &str) -> BTreeMap<String, u64> {
        let module = parse(source);
        let plan = plan_module(&ModuleSource::builtin(), &module, &IndexSet::default()).plan;
        plan.functions
            .into_iter()
            .map(|f| (f.name, f.hash))
            .collect()
    }

    #[test]
    fn a_function_hash_follows_its_own_text_alone() {
        let f = "fn f(x: i32) -> i32 {\n    if x == 0 { return 1; }\n    return 2;\n}\n";
        let g = "fn g() -> i32 {\n    return 3;\n}\n";
        let base = hashes(&format!("{f}{g}"));
        let edited = hashes(&format!("{}{g}", f.replace("x == 0", "x <= 0")));
        assert_ne!(base["f"], edited["f"]);
        assert_eq!(base["g"], edited["g"]);
        let moved = hashes(&format!("{g}\n\n{f}"));
        assert_eq!(base, moved);
        let crlf = hashes(&format!("{f}{g}").replace('\n', "\r\n"));
        assert_eq!(base, crlf);
    }

    /// The kinds of the regions `body` plans as `fn f`'s, with every callee
    /// spelled `unreachable` standing for `core:rt`'s, as resolution says of
    /// the prelude's.
    fn plan_kinds(body: &str) -> Vec<RegionKind> {
        struct Spelled(IndexSet<AstId>);
        impl AstVisitor<'_> for Spelled {
            fn visit_expr(&mut self, expr: &Expr) {
                if let Expr::Ident(ident) = expr
                    && ident.name == "unreachable"
                {
                    self.0.insert(ident.id);
                }
                walk_expr(self, expr);
            }
        }
        let module = parse(&format!("fn f(x: bool, y: bool) -> i32 {{\n{body}\n}}\n"));
        let mut spelled = Spelled(IndexSet::default());
        for item in &module.items {
            spelled.visit_item(item);
        }
        let plan = plan_module(&ModuleSource::builtin(), &module, &spelled.0).plan;
        plan.regions.iter().map(|r| r.kind).collect()
    }

    #[test]
    fn only_an_unreachable_call_claims_a_region_unreached() {
        use RegionKind::{Else, Function, Then};
        let unplanned = vec![Function, Else];
        let planned = vec![Function, Then, Else];
        let shapes = [
            ("unreachable();", &unplanned),
            ("builtin::unreachable();", &unplanned),
            ("return unreachable();", &unplanned),
            ("panic(\"x\");", &planned),
            ("return panic(\"x\");", &planned),
            ("unreachable::<i32>();", &planned),
            ("g(); unreachable();", &planned),
        ];
        for (then, kinds) in shapes {
            let body = format!("    if x {{ {then} }}");
            assert_eq!(&plan_kinds(&body), kinds, "{then}");
        }
    }

    #[test]
    fn a_path_falling_into_unreachable_is_no_branch() {
        use RegionKind::{Arm, Else, Function, Then};
        let shapes = [
            ("if x { return 1; }", vec![Function, Then]),
            (
                "if x { return 1; } else if y { return 2; }",
                vec![Function, Then, Else, Then],
            ),
            (
                "match x { true => { return 1; }, false => {}, }",
                vec![Function, Arm],
            ),
        ];
        for (head, kinds) in shapes {
            let body = format!("    {head}\n    unreachable();");
            assert_eq!(plan_kinds(&body), kinds, "{head}");
        }
    }

    #[test]
    fn truncated_section_is_rejected() {
        assert_eq!(decode(&[1, 0, 0, 0]), None);
    }
}
