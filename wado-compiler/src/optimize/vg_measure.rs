//! Measurement-only instrumentation for the live-ValueGraph decision.
//!
//! Gated by `WADO_MEASURE_VG`. Records, at each value-graph build a pass would
//! do, how much of the function changed since that function was last built —
//! the share of build cost an incremental rebuild (reuse the clean prefix,
//! re-walk from the first disturbed root statement to the end) could save.
//!
//! For every build of function `fid` it computes a structural hash + node count
//! per root-block statement, compares to the previous build's, finds the first
//! statement that differs (or where the lists misalign — an insert/delete), and
//! attributes the node counts before it to "savable" (reusable prefix) and from
//! it to the end to "re-walked". Off by default (zero cost).

use std::cell::RefCell;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::OnceLock;

use crate::nir_arena::{Body, ExprKind, NodeRef, StmtKind};

pub fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("WADO_MEASURE_VG").is_some())
}

#[derive(Default)]
struct Acc {
    /// Per-function root-statement (hash, `node_count`) at its last build.
    prev: Vec<Option<Vec<(u64, u64)>>>,
    builds: u64,
    first_builds: u64,
    /// Per-pass `(builds, first_builds)`, so the rebuild count can be attributed
    /// to the passes that build (`cse`, `licm`) — targeting for the live-graph
    /// work (which redundant rebuilds to eliminate first).
    per_pass: std::collections::BTreeMap<&'static str, (u64, u64)>,
    total_nodes: u64,
    savable_nodes: u64,
    rewalked_nodes: u64,
    /// Savable nodes from rebuilds whose body was *fully* unchanged
    /// (savable == total) — a cache hit a better revision scheme would catch,
    /// not an incremental-rebuild saving.
    savable_full_nodes: u64,
    /// Savable nodes from rebuilds that were *partially* changed — the genuine
    /// incremental-rebuild-specific saving (reuse prefix, re-walk suffix).
    savable_partial_nodes: u64,
    /// Histogram of per-build savable fraction (10 buckets, 0–100%).
    frac_buckets: [u64; 11],
    /// Node-weighted: builds where >0 was savable.
    incremental_builds: u64,
    /// Genuinely-new skeleton nodes the edit API creates across the whole
    /// optimize (every `Engine::alloc_{expr,stmt,block,pat}`). This is the
    /// *maintenance lower bound*: under graph-as-source-of-truth, only these
    /// new nodes need value-node construction; the untouched remainder is never
    /// re-walked. Compared against `total_nodes` (the rebuild cost) it shows how
    /// much cheaper pointwise maintenance is than the 2.67× rebuild it replaces.
    created_nodes: u64,
    /// In-place structural edits (`replace_expr_kind` / `set_block_stmts`). Each
    /// re-interns one node and propagates to its users — O(1)-ish maintenance,
    /// not a re-walk. Counted to make the maintenance bound airtight.
    inplace_edits: u64,
}

thread_local! {
    static ACC: RefCell<Acc> = RefCell::new(Acc::default());
    /// The `(fid, pass)` an actual `builder::build` should be attributed to.
    /// Set by a value pass (`cse` / `licm`) around its per-function engine
    /// session; an actual build inside that session (via `ensure_value_graph`)
    /// reads it. `None` outside a session, so a build the freeze does is not
    /// counted — keeping the meter scoped to the value passes, as before.
    static BUILD_CTX: RefCell<Option<(usize, &'static str)>> = const { RefCell::new(None) };
}

/// RAII guard naming the `(fid, pass)` an actual value-graph build should be
/// attributed to. While it is alive, the first `builder::build` of this engine
/// session (`Engine::ensure_value_graph`) records itself against `fid`/`pass`;
/// reuse of a persisted graph records nothing — so under build-once the meter
/// drops, honestly counting real builds rather than pass entries.
pub struct BuildScope;

impl BuildScope {
    pub fn enter(fid: usize, pass: &'static str) -> Self {
        if enabled() {
            BUILD_CTX.with(|c| *c.borrow_mut() = Some((fid, pass)));
        }
        BuildScope
    }
}

impl Drop for BuildScope {
    fn drop(&mut self) {
        if enabled() {
            BUILD_CTX.with(|c| *c.borrow_mut() = None);
        }
    }
}

/// Record an actual `builder::build`, attributed to the active [`BuildScope`].
/// Called from `Engine::ensure_value_graph` exactly when it builds (not on
/// reuse). No-op when off or outside a value-pass session.
pub fn record_actual_build(body: &Body) {
    if !enabled() {
        return;
    }
    let ctx = BUILD_CTX.with(|c| *c.borrow());
    if let Some((fid, pass)) = ctx {
        record_build(fid, body, pass);
    }
}

/// Record one genuinely-new skeleton node created by the edit API. Called from
/// `Engine::alloc_{expr,stmt,block,pat}`. Off by default (zero cost).
#[inline]
pub(crate) fn record_node_created() {
    if !enabled() {
        return;
    }
    ACC.with(|a| a.borrow_mut().created_nodes += 1);
}

/// Record one in-place structural edit (`replace_expr_kind` / `set_block_stmts`).
/// Off by default (zero cost).
#[inline]
pub(crate) fn record_inplace_edit() {
    if !enabled() {
        return;
    }
    ACC.with(|a| a.borrow_mut().inplace_edits += 1);
}

/// Record a value-graph build of function `fid` over `body`. Compares to the
/// previous build of the same function and attributes node counts to the
/// reusable prefix vs the re-walked suffix.
pub fn record_build(fid: usize, body: &Body, pass: &'static str) {
    if !enabled() {
        return;
    }
    let cur = root_stmt_sigs(body);
    let cur_total: u64 = cur.iter().map(|(_, n)| n).sum();
    ACC.with(|a| {
        let a = &mut *a.borrow_mut();
        if a.prev.len() <= fid {
            a.prev.resize(fid + 1, None);
        }
        a.builds += 1;
        a.total_nodes += cur_total;
        let pp = a.per_pass.entry(pass).or_default();
        pp.0 += 1;
        let is_first = a.prev[fid].is_none();
        if is_first {
            pp.1 += 1;
        }
        match a.prev[fid].take() {
            None => {
                // First build of this function: nothing reusable.
                a.first_builds += 1;
                a.rewalked_nodes += cur_total;
                a.frac_buckets[0] += 1;
            }
            Some(prev) => {
                let first_dirty = first_divergence(&prev, &cur);
                let savable: u64 = cur[..first_dirty].iter().map(|(_, n)| n).sum();
                let rewalked: u64 = cur[first_dirty..].iter().map(|(_, n)| n).sum();
                a.savable_nodes += savable;
                a.rewalked_nodes += rewalked;
                if savable > 0 {
                    a.incremental_builds += 1;
                }
                if rewalked == 0 {
                    // Fully unchanged body — a cache hit, not incremental.
                    a.savable_full_nodes += savable;
                } else {
                    a.savable_partial_nodes += savable;
                }
                let frac = if cur_total == 0 {
                    0.0
                } else {
                    savable as f64 / cur_total as f64
                };
                let bucket = ((frac * 10.0).round() as usize).min(10);
                a.frac_buckets[bucket] += 1;
            }
        }
        a.prev[fid] = Some(cur);
    });
}

/// Index of the first root statement that differs from `prev` (by structural
/// hash), or where the lists misalign (length change) — exactly where
/// `rebuild_incremental` would stop reusing and start re-walking.
fn first_divergence(prev: &[(u64, u64)], cur: &[(u64, u64)]) -> usize {
    let n = prev.len().min(cur.len());
    for i in 0..n {
        if prev[i].0 != cur[i].0 {
            return i;
        }
    }
    // Common prefix matched; if lengths differ, the first extra/removed stmt is
    // the divergence. If identical, divergence is the end (nothing re-walked).
    n.min(cur.len())
}

/// Per-root-statement (structural hash, subtree node count), in document order.
fn root_stmt_sigs(body: &Body) -> Vec<(u64, u64)> {
    body.blocks[body.root]
        .stmts
        .iter()
        .map(|&s| {
            let mut h = DefaultHasher::new();
            let mut n = 0u64;
            hash_node(body, NodeRef::Stmt(s), &mut h, &mut n);
            (h.finish(), n)
        })
        .collect()
}

fn hash_node(body: &Body, node: NodeRef, h: &mut DefaultHasher, n: &mut u64) {
    *n += 1;
    // Feed a structural fingerprint: the node kind's discriminant plus the leaf
    // payloads that distinguish two same-shaped nodes (literal values, local
    // index, operators, field indices).
    match node {
        NodeRef::Expr(id) => {
            let k = &body.exprs[id].kind;
            std::mem::discriminant(k).hash(h);
            match k {
                ExprKind::Local { index, .. } => index.hash(h),
                ExprKind::Binary { op, .. } => std::mem::discriminant(op).hash(h),
                ExprKind::Unary { op, .. } => std::mem::discriminant(op).hash(h),
                ExprKind::FieldAccess { field_index, .. } => field_index.hash(h),
                _ => {}
            }
        }
        NodeRef::Stmt(id) => {
            let k = &body.stmts[id].kind;
            std::mem::discriminant(k).hash(h);
            if let StmtKind::Let { local_index, .. } = k {
                local_index.hash(h);
            }
        }
        NodeRef::Block(_) | NodeRef::Pat(_) => {}
    }
    let mut kids = Vec::new();
    body.for_each_child(node, |c| kids.push(c));
    for c in kids {
        hash_node(body, c, h, n);
    }
}

/// Print the accumulated summary to stderr. Called once at the end of the loop.
pub fn report() {
    if !enabled() {
        return;
    }
    ACC.with(|a| {
        let a = a.borrow();
        let savable_pct = if a.total_nodes == 0 {
            0.0
        } else {
            a.savable_nodes as f64 / a.total_nodes as f64 * 100.0
        };
        eprintln!("=== WADO_MEASURE_VG: value-graph build incremental potential ===");
        eprintln!(
            "builds={} (first_builds={}, rebuilds={})",
            a.builds,
            a.first_builds,
            a.builds - a.first_builds
        );
        for (pass, (builds, firsts)) in &a.per_pass {
            eprintln!(
                "  by pass {pass}: builds={builds} (first={firsts}, rebuilds={})",
                builds - firsts
            );
        }
        eprintln!(
            "total build node-walks = {} ; savable (reusable prefix) = {} ({:.1}%) ; re-walked = {}",
            a.total_nodes, a.savable_nodes, savable_pct, a.rewalked_nodes
        );
        let pct = |x: u64| {
            if a.total_nodes == 0 {
                0.0
            } else {
                x as f64 / a.total_nodes as f64 * 100.0
            }
        };
        eprintln!(
            "  of savable: fully-unchanged (cache-hit territory) = {} ({:.1}% of total) ; partial (incremental-only) = {} ({:.1}% of total)",
            a.savable_full_nodes,
            pct(a.savable_full_nodes),
            a.savable_partial_nodes,
            pct(a.savable_partial_nodes),
        );
        eprintln!(
            "rebuilds with any savable prefix = {} / {} ({:.1}%)",
            a.incremental_builds,
            a.builds - a.first_builds,
            if a.builds > a.first_builds {
                a.incremental_builds as f64 / (a.builds - a.first_builds) as f64 * 100.0
            } else {
                0.0
            }
        );
        eprint!("savable-fraction histogram (per rebuild, 0%..100%): ");
        for (i, c) in a.frac_buckets.iter().enumerate() {
            eprint!("{i}0%={c} ");
        }
        eprintln!();
        let edits = a.created_nodes + a.inplace_edits;
        eprintln!(
            "maintenance bound: new nodes = {} + in-place edits = {} = {} total edits ({:.1}% of the {} rebuild node-walks). \
             Pointwise maintenance touches only these; the rebuild re-walks all {} to stay current.",
            a.created_nodes,
            a.inplace_edits,
            edits,
            if a.total_nodes == 0 {
                0.0
            } else {
                edits as f64 / a.total_nodes as f64 * 100.0
            },
            a.total_nodes,
            a.total_nodes,
        );
    });
}
