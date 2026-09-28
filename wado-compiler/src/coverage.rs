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

use crate::ast::{
    self, AstId, AstVisitor, BinaryOp, Block, Expr, Function, Item, MatchExpr, Stmt, walk_expr,
    walk_stmt,
};
use crate::attribute;
use crate::hashmap::IndexMap;
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
    /// The innermost region holding this one, `None` for a body.
    pub parent: Option<u32>,
    /// For a branch, where its choice is written and which side it is.
    pub choice: Option<(Pos, u32)>,
}

/// One function of a module plan: a `fn`, a method, or a closure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedFunction {
    pub name: String,
    pub line: u32,
    /// Its body region.
    pub region: u32,
}

/// The regions a module's source implies, numbered from zero.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ModulePlan {
    /// The module's source path: relative to the entry module's directory
    /// for a file, as the loader reads it, or the import path (`core:json`).
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
    probes: IndexMap<(ProbeSite, AstId), u32>,
    /// The global ids of each function's regions, by the global id of its body.
    function_regions: IndexMap<u32, Vec<u32>>,
}

impl CoverageMap {
    /// Plan `modules` in the order given, which fixes the global ids.
    pub fn build<'a>(
        modules: impl IntoIterator<Item = (&'a ModuleSource, &'a ast::Module)>,
    ) -> Self {
        let mut map = Self::default();
        for (source, module) in modules {
            let (plan, sites) = plan_module(source, module);
            let base = map.modules.iter().map(|m| m.regions.len() as u32).sum();
            for (site, id, region) in sites {
                map.probes.insert((site, id), base + region);
            }
            for function in &plan.functions {
                let regions = (0..plan.regions.len() as u32)
                    .filter(|&r| {
                        plan.functions[plan.regions[r as usize].function as usize].region
                            == function.region
                    })
                    .map(|r| base + r)
                    .collect();
                map.function_regions.insert(base + function.region, regions);
            }
            map.modules.push(plan);
            map.bases.push(base);
        }
        map
    }

    /// The probe id reify puts at `site` on node `id`, if one goes there.
    #[must_use]
    pub fn probe(&self, site: ProbeSite, id: AstId) -> Option<u32> {
        self.probes.get(&(site, id)).copied()
    }

    /// The regions of the function whose body region is `body`, when `body`
    /// is one.
    #[must_use]
    pub fn function_regions(&self, body: u32) -> Option<&[u32]> {
        self.function_regions.get(&body).map(Vec::as_slice)
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
            w.str(&plan.path);
            w.u32(base);
            w.u32(plan.functions.len() as u32);
            for f in &plan.functions {
                w.str(&f.name);
                w.u32(f.line);
                w.u32(f.region);
            }
            w.u32(plan.regions.len() as u32);
            for r in &plan.regions {
                w.0.push(r.kind.code());
                w.pos(r.start);
                w.pos(r.end);
                w.u32(r.function);
                w.u32(r.parent.map_or(0, |p| p + 1));
                match r.choice {
                    Some((at, side)) => {
                        w.0.push(1);
                        w.pos(at);
                        w.u32(side);
                    }
                    None => w.0.push(0),
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
        let path = r.str()?;
        let base = r.u32()?;
        let mut plan = ModulePlan {
            path,
            ..ModulePlan::default()
        };
        for _ in 0..r.u32()? {
            plan.functions.push(PlannedFunction {
                name: r.str()?,
                line: r.u32()?,
                region: r.u32()?,
            });
        }
        for _ in 0..r.u32()? {
            let kind = RegionKind::from_code(r.u8()?)?;
            let start = r.pos()?;
            let end = r.pos()?;
            let function = r.u32()?;
            let parent = r.u32()?.checked_sub(1);
            let choice = match r.u8()? {
                0 => None,
                _ => Some((r.pos()?, r.u32()?)),
            };
            plan.regions.push(Region {
                kind,
                start,
                end,
                function,
                parent,
                choice,
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

/// The path a plan names its module by. The loader reads a local module
/// relative to the entry module's directory, so the entry is named by its file
/// name alone to share that base.
fn plan_path(source: &ModuleSource) -> String {
    match source {
        ModuleSource::EntryPoint { .. } => source.to_path_string(),
        _ => source.source_path(),
    }
}

/// Plan one module: its regions, and the site each region's probe goes at.
#[must_use]
pub fn plan_module(source: &ModuleSource, module: &ast::Module) -> (ModulePlan, Sites) {
    let mut planner = Planner {
        plan: ModulePlan {
            path: plan_path(source),
            ..ModulePlan::default()
        },
        sites: Vec::new(),
        function: 0,
        region: None,
        owner: None,
    };
    let off = module
        .inner_attributes
        .iter()
        .any(|attr| attr.name == attribute::COVERAGE);
    if !off {
        for item in &module.items {
            planner.item(item);
        }
    }
    planner.plan.lines.sort_unstable();
    planner.plan.lines.dedup();
    (planner.plan, planner.sites)
}

struct Planner {
    plan: ModulePlan,
    sites: Sites,
    function: u32,
    /// The innermost region the walk is in, `None` outside a function.
    region: Option<u32>,
    /// The type or trait a method is declared under.
    owner: Option<String>,
}

impl Planner {
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
        if attribute::coverage_off(&f.attrs) {
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
        self.region = Some(region);
        self.block(body);
        (self.function, self.region) = saved;
    }

    fn new_function(&mut self, name: String, at: Span, kind: RegionKind, body: Span) -> u32 {
        self.function = self.plan.functions.len() as u32;
        let region = self.plan.regions.len() as u32;
        self.plan.functions.push(PlannedFunction {
            name,
            line: at.line as u32,
            region,
        });
        self.plan.regions.push(Region {
            kind,
            start: Pos::start(body),
            end: Pos::end(body),
            function: self.function,
            parent: None,
            choice: None,
        });
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
            parent: self.region,
            choice,
        });
        region
    }

    /// Walk `visit` inside a new region, and return to the current one.
    fn in_region(&mut self, region: u32, visit: impl FnOnce(&mut Self)) {
        let saved = self.region.replace(region);
        visit(self);
        self.region = saved;
    }

    fn line(&mut self, span: Span) {
        if let Some(region) = self.region {
            self.plan.lines.push((span.line as u32, region));
        }
    }

    fn branch_block(&mut self, kind: RegionKind, block: &Block, choice: (Pos, u32)) {
        let region = self.new_region(
            kind,
            Pos::start(block.span),
            Pos::end(block.span),
            Some(choice),
        );
        self.sites.push((ProbeSite::BlockStart, block.id, region));
        self.in_region(region, |p| p.block(block));
    }

    /// An `if`, statement or expression. Both branches are regions, and an
    /// omitted `else` is one too: the path that skips the `then` block.
    fn if_branches(
        &mut self,
        id: AstId,
        span: Span,
        then_block: &Block,
        else_block: Option<&Block>,
    ) {
        let at = Pos::start(span);
        self.branch_block(RegionKind::Then, then_block, (at, 0));
        match else_block {
            Some(block) => self.branch_block(RegionKind::Else, block, (at, 1)),
            None => {
                let region = self.new_region(
                    RegionKind::Else,
                    Pos::end(span),
                    Pos::end(span),
                    Some((at, 1)),
                );
                self.sites.push((ProbeSite::ImplicitElse, id, region));
            }
        }
    }

    fn match_arms(&mut self, m: &MatchExpr) {
        self.visit_expr(&m.expr);
        let at = Pos::start(m.span);
        for (side, arm) in m.arms.iter().enumerate() {
            if let Some(guard) = &arm.guard {
                self.visit_expr(guard);
            }
            let body = arm.body.span();
            let region = self.new_region(
                RegionKind::Arm,
                Pos::start(body),
                Pos::end(body),
                Some((at, side as u32)),
            );
            self.sites.push((ProbeSite::Around, arm.body.id(), region));
            self.in_region(region, |p| p.expr_body(&arm.body));
        }
    }

    /// A body written as an expression: a block counts its statements, any
    /// other expression is one line.
    fn expr_body(&mut self, body: &Expr) {
        if !matches!(body, Expr::Block(_)) {
            self.line(body.span());
        }
        self.visit_expr(body);
    }

    fn loop_body(&mut self, body: &Block) {
        let region = self.new_region(
            RegionKind::Loop,
            Pos::start(body.span),
            Pos::end(body.span),
            None,
        );
        self.sites.push((ProbeSite::BlockStart, body.id, region));
        self.in_region(region, |p| p.block(body));
    }

    fn block(&mut self, block: &Block) {
        let saved = self.region;
        let mut leaves = false;
        for stmt in &block.stmts {
            if leaves {
                let region = self.new_region(
                    RegionKind::Continuation,
                    Pos::start(stmt.span()),
                    Pos::end(block.span),
                    None,
                );
                self.sites.push((ProbeSite::BeforeStmt, stmt.id(), region));
                self.region = Some(region);
            }
            if !matches!(stmt, Stmt::Item(_) | Stmt::Error(_)) {
                self.line(stmt.span());
            }
            self.visit_stmt(stmt);
            leaves = leaves_early(stmt);
        }
        self.region = saved;
    }
}

impl AstVisitor for Planner {
    fn visit_item(&mut self, item: &Item) {
        self.item(item);
    }

    fn visit_block(&mut self, block: &Block) {
        self.block(block);
    }

    fn visit_stmt(&mut self, stmt: &Stmt) {
        match stmt {
            Stmt::Let(s) => {
                if let Some(value) = &s.value {
                    self.visit_expr(value);
                }
                if let Some(block) = &s.else_block {
                    let region = self.new_region(
                        RegionKind::LetElse,
                        Pos::start(block.span),
                        Pos::end(block.span),
                        Some((Pos::start(s.span), 0)),
                    );
                    self.sites.push((ProbeSite::BlockStart, block.id, region));
                    self.in_region(region, |p| p.block(block));
                }
            }
            Stmt::If(s) => {
                self.visit_condition(&s.condition);
                self.if_branches(s.id, s.span, &s.then_block, s.else_block.as_ref());
            }
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
                self.loop_body(&s.body);
            }
            Stmt::Loop(s) => self.loop_body(&s.body),
            Stmt::Match(m) => self.match_arms(m),
            // Power-assert rewrites the condition, so its operands are not
            // the source's regions.
            Stmt::Assert(_) => {}
            _ => walk_stmt(self, stmt),
        }
    }

    fn visit_expr(&mut self, expr: &Expr) {
        match expr {
            Expr::Binary(b) if matches!(b.op, BinaryOp::And | BinaryOp::Or) => {
                self.visit_expr(&b.left);
                let right = b.right.span();
                let region = self.new_region(
                    RegionKind::Rhs,
                    Pos::start(right),
                    Pos::end(right),
                    Some((Pos::start(b.span), 0)),
                );
                self.sites.push((ProbeSite::Around, b.right.id(), region));
                self.in_region(region, |p| p.visit_expr(&b.right));
            }
            Expr::If(e) => {
                self.visit_condition(&e.condition);
                self.if_branches(e.id, e.span, &e.then_block, e.else_block.as_ref());
            }
            Expr::Match(m) => self.match_arms(m),
            Expr::Closure(c) => {
                let Some(outer) = self.region else { return };
                let outer_name = self.plan.functions
                    [self.plan.regions[outer as usize].function as usize]
                    .name
                    .clone();
                let saved = (self.function, self.region);
                let name = format!("{outer_name}::{{closure:{}}}", c.span.line);
                let region = self.new_function(name, c.span, RegionKind::Closure, c.body.span());
                self.plan.regions[region as usize].parent = Some(outer);
                self.sites.push((ProbeSite::Around, c.body.id(), region));
                self.region = Some(region);
                self.expr_body(&c.body);
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
            }
            _ => walk_expr(self, expr),
        }
    }
}

/// Whether `stmt` can leave its block before the next statement: a `return`,
/// `break`, `continue` or `?` outside a closure. A `break` of a loop nested in
/// `stmt` counts too, which costs a probe and loses nothing.
fn leaves_early(stmt: &Stmt) -> bool {
    struct Leaves(bool);

    impl AstVisitor for Leaves {
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

    let mut leaves = Leaves(false);
    leaves.visit_stmt(stmt);
    leaves.0
}

// ─────────────────────────────────────────────────────────────────
// Reports
// ─────────────────────────────────────────────────────────────────

/// Plans and hits merged across every compile of one run, keyed by path.
#[derive(Debug, Clone, Default)]
pub struct Coverage {
    pub files: BTreeMap<String, FileCoverage>,
}

/// One module's plan and the regions any test ran.
#[derive(Debug, Clone, Default)]
pub struct FileCoverage {
    pub plan: ModulePlan,
    pub hit: BTreeSet<u32>,
    /// The tests that ran each region, by region.
    pub tests: BTreeMap<u32, BTreeSet<String>>,
}

/// Why two plans cannot be merged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanMismatch {
    pub path: String,
}

impl Coverage {
    /// Add one compile's plans under `rename(path)`, and the global ids
    /// `test` hit in them. Plans with no hits still count every region.
    pub fn add(
        &mut self,
        plans: &DecodedPlans,
        hits: &BTreeSet<u32>,
        test: Option<&str>,
        mut rename: impl FnMut(&str) -> String,
    ) -> Result<(), PlanMismatch> {
        for (plan, base) in plans {
            let (mut plan, base) = (plan.clone(), *base);
            let count = plan.regions.len() as u32;
            let local: Vec<u32> = hits.range(base..base + count).map(|id| id - base).collect();
            plan.path = rename(&plan.path);
            let file = self
                .files
                .entry(plan.path.clone())
                .or_insert_with(|| FileCoverage {
                    plan: plan.clone(),
                    hit: BTreeSet::new(),
                    tests: BTreeMap::new(),
                });
            if file.plan != plan {
                return Err(PlanMismatch { path: plan.path });
            }
            if let Some(test) = test {
                for &region in &local {
                    file.tests
                        .entry(region)
                        .or_default()
                        .insert(test.to_string());
                }
            }
            file.hit.extend(local);
        }
        Ok(())
    }
}

impl FileCoverage {
    /// Each countable line and whether a statement on it ran.
    #[must_use]
    pub fn lines(&self) -> BTreeMap<u32, bool> {
        let mut out = BTreeMap::new();
        for &(line, region) in &self.plan.lines {
            *out.entry(line).or_insert(false) |= self.hit.contains(&region);
        }
        out
    }

    /// Each branch region, in source order: where its choice is written, its
    /// side, its kind, and whether it ran.
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
                    taken: self.hit.contains(&(index as u32)),
                })
            })
            .collect();
        out.sort_by_key(|b| (b.at, b.side));
        out
    }

    /// Each function and whether its body ran.
    #[must_use]
    pub fn functions(&self) -> Vec<(&PlannedFunction, bool)> {
        self.plan
            .functions
            .iter()
            .map(|f| (f, self.hit.contains(&f.region)))
            .collect()
    }
}

/// One side of a choice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Branch {
    pub at: Pos,
    pub side: u32,
    pub kind: RegionKind,
    pub taken: bool,
}

impl Branch {
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
        for (f, ran) in &functions {
            let _ = writeln!(out, "FNDA:{},{}", u8::from(*ran), f.name);
        }
        let _ = writeln!(out, "FNF:{}", functions.len());
        let _ = writeln!(
            out,
            "FNH:{}",
            functions.iter().filter(|(_, ran)| *ran).count()
        );
        let branches = file.branches();
        let mut blocks: Vec<Pos> = branches.iter().map(|b| b.at).collect();
        blocks.dedup();
        for b in &branches {
            let block = blocks.iter().position(|at| *at == b.at).unwrap_or(0);
            let taken = if b.taken { "1" } else { "0" };
            let _ = writeln!(out, "BRDA:{},{block},{},{taken}", b.at.line, b.side);
        }
        let _ = writeln!(out, "BRF:{}", branches.len());
        let _ = writeln!(out, "BRH:{}", branches.iter().filter(|b| b.taken).count());
        let lines = file.lines();
        for (line, ran) in &lines {
            let _ = writeln!(out, "DA:{line},{}", u8::from(*ran));
        }
        let _ = writeln!(out, "LF:{}", lines.len());
        let _ = writeln!(out, "LH:{}", lines.values().filter(|ran| **ran).count());
        let _ = writeln!(out, "end_of_record");
    }
    out
}

/// The plan of `module` as `wado dump --coverage-plan` prints it.
#[must_use]
pub fn render_plan(plan: &ModulePlan) -> String {
    use std::fmt::Write;

    let mut out = String::new();
    let _ = writeln!(out, "coverage plan: {}", plan.path);
    for (index, region) in plan.regions.iter().enumerate() {
        let function = &plan.functions[region.function as usize];
        let parent = region.parent.map_or(String::new(), |p| format!(" in r{p}"));
        let choice = region.choice.map_or(String::new(), |(at, side)| {
            format!(" choice {}:{} side {side}", at.line, at.column)
        });
        let _ = writeln!(
            out,
            "  r{index} {} {}:{}-{}:{} ({}){parent}{choice}",
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

    #[test]
    fn section_round_trips() {
        let plan = ModulePlan {
            path: "a.wado".to_string(),
            functions: vec![PlannedFunction {
                name: "f".to_string(),
                line: 1,
                region: 0,
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
                    parent: None,
                    choice: None,
                },
                Region {
                    kind: RegionKind::Then,
                    start: Pos {
                        line: 2,
                        column: 10,
                    },
                    end: Pos { line: 4, column: 6 },
                    function: 0,
                    parent: Some(0),
                    choice: Some((Pos { line: 2, column: 5 }, 0)),
                },
            ],
            lines: vec![(2, 0), (3, 1)],
        };
        let map = CoverageMap {
            modules: vec![plan.clone()],
            bases: vec![7],
            ..CoverageMap::default()
        };
        assert_eq!(decode(&map.encode()), Some(vec![(plan, 7)]));
    }

    #[test]
    fn truncated_section_is_rejected() {
        assert_eq!(decode(&[1, 0, 0, 0]), None);
    }
}
