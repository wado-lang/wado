//! DAG construction, topological ordering, and cycle detection for Kiln
//! invocations.
//!
//! An edge `A -> B` means A depends on B: B must run first so A sees B's
//! output directory populated. See WEP 2026-04-12 §"Build pipeline
//! integration" (the five-step plan).

use super::invocation::{DeclSite, Invocation, InvocationPath};

/// A scheduling plan for a set of invocations.
#[derive(Debug, Clone)]
pub struct Plan {
    /// Invocations in execution order (parents before dependents).
    pub order: Vec<Invocation>,
}

/// Error from [`build_plan`].
#[derive(Debug)]
pub enum PlanError {
    /// A cycle was detected. The reported cycle lists every invocation
    /// involved with its decl site for diagnostics.
    Cycle { participants: Vec<DeclSite> },
    /// Two invocations share a `from` path but disagree on configuration
    /// (module / inputs / options / `output_dir`). Per WEP line 368-371 such
    /// declarations must be merged; when they cannot the compiler refuses.
    DuplicateGenerator { from: String, sites: Vec<DeclSite> },
}

impl core::fmt::Display for PlanError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            PlanError::Cycle { participants } => {
                write!(f, "kiln cycle detected among generators:")?;
                for p in participants {
                    write!(f, "\n  - {p}")?;
                }
                Ok(())
            }
            PlanError::DuplicateGenerator { from, sites } => {
                write!(
                    f,
                    "duplicate kiln generator for schema {from:?} with conflicting configuration:"
                )?;
                for s in sites {
                    write!(f, "\n  - {s}")?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for PlanError {}

/// Deduplicate, then topologically sort `invocations`.
///
/// Dedup rule: two invocations with matching `identity_tuple` are merged
/// (the first decl site wins). Two that share `from` but have different
/// identity tuples raise [`PlanError::DuplicateGenerator`].
pub fn build_plan(invocations: Vec<Invocation>) -> Result<Plan, PlanError> {
    let deduped = dedup_invocations(invocations)?;
    topo_sort(deduped)
}

/// Apply WEP's dedup semantics and return the surviving invocation list.
///
/// Preserves declaration order of the first occurrence.
fn dedup_invocations(invocations: Vec<Invocation>) -> Result<Vec<Invocation>, PlanError> {
    let mut by_from: Vec<(String, Vec<usize>)> = Vec::new();
    for (i, inv) in invocations.iter().enumerate() {
        if let Some(entry) = by_from.iter_mut().find(|(f, _)| f == inv.from.as_str()) {
            entry.1.push(i);
        } else {
            by_from.push((inv.from.as_str().to_string(), vec![i]));
        }
    }

    let mut keep = vec![true; invocations.len()];
    for (from, indices) in &by_from {
        if indices.len() < 2 {
            continue;
        }
        let first = &invocations[indices[0]];
        let mut sites = vec![first.decl_site.clone()];
        let mut all_equal = true;
        for &i in &indices[1..] {
            let other = &invocations[i];
            sites.push(other.decl_site.clone());
            if other.identity_tuple() != first.identity_tuple() {
                all_equal = false;
            }
        }
        if !all_equal {
            return Err(PlanError::DuplicateGenerator {
                from: from.clone(),
                sites,
            });
        }
        for &i in &indices[1..] {
            keep[i] = false;
        }
    }

    Ok(invocations
        .into_iter()
        .enumerate()
        .filter_map(|(i, inv)| if keep[i] { Some(inv) } else { None })
        .collect())
}

/// Kahn-style topological sort.
///
/// Edge `A -> B` exists iff any of A's declared input paths (primary, inputs,
/// or prior-reads — passed separately) lies lexically inside B's output
/// directory. Here we consider only `primary` + `inputs`; prior-reads are
/// threaded in by the pipeline driver when composing the full graph.
///
/// The queue is FIFO (`VecDeque::pop_front`) so that, among invocations whose
/// relative order is not pinned by edges, the declaration order is preserved.
/// Downstream consumers (the lockfile writer) rely on this stability.
fn topo_sort(invocations: Vec<Invocation>) -> Result<Plan, PlanError> {
    use std::collections::VecDeque;

    let n = invocations.len();
    let mut edges: Vec<Vec<usize>> = vec![Vec::new(); n];
    let mut indegree: Vec<usize> = vec![0; n];
    for (i, a) in invocations.iter().enumerate() {
        for (j, b) in invocations.iter().enumerate() {
            if i == j {
                continue;
            }
            if depends_on(a, b) {
                edges[j].push(i);
                indegree[i] += 1;
            }
        }
    }

    let mut queue: VecDeque<usize> = indegree
        .iter()
        .enumerate()
        .filter_map(|(i, &d)| if d == 0 { Some(i) } else { None })
        .collect();

    let mut order_idx: Vec<usize> = Vec::with_capacity(n);
    let mut processed: Vec<bool> = vec![false; n];
    while let Some(i) = queue.pop_front() {
        order_idx.push(i);
        processed[i] = true;
        for &succ in &edges[i] {
            indegree[succ] -= 1;
            if indegree[succ] == 0 {
                queue.push_back(succ);
            }
        }
    }

    if order_idx.len() != n {
        // Every node Kahn could not process is reported. This over-reports
        // when a node is merely downstream of a cycle (not part of it); SCC
        // detection would narrow it but is not worth the extra code until a
        // real cycle report fires in practice.
        let participants: Vec<DeclSite> = (0..n)
            .filter(|&i| !processed[i])
            .map(|i| invocations[i].decl_site.clone())
            .collect();
        return Err(PlanError::Cycle { participants });
    }

    let mut slots: Vec<Option<Invocation>> = invocations.into_iter().map(Some).collect();
    let order: Vec<Invocation> = order_idx
        .into_iter()
        .map(|i| slots[i].take().expect("each index is taken exactly once"))
        .collect();
    Ok(Plan { order })
}

/// Does `a`'s inputs (primary + declared inputs) fall inside `b`'s output
/// directory?
///
/// Path comparison is purely lexical: `b.output_dir` is treated as a prefix.
/// "Inside" means the input path equals `b.output_dir` or is a child.
#[must_use]
pub fn depends_on(a: &Invocation, b: &Invocation) -> bool {
    if path_inside(&a.from, &b.output_dir) {
        return true;
    }
    for p in &a.inputs {
        if path_inside(p, &b.output_dir) {
            return true;
        }
    }
    false
}

fn path_inside(candidate: &InvocationPath, dir: &InvocationPath) -> bool {
    let c = candidate.as_str();
    let d = dir.as_str();
    if d.is_empty() {
        return false;
    }
    if c == d {
        return true;
    }
    if !c.starts_with(d) {
        return false;
    }
    let rest = &c[d.len()..];
    rest.starts_with('/')
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kiln::invocation::{DeclSite, GeneratorModule};

    fn inv(name: &str, from: &str, inputs: &[&str], out: &str) -> Invocation {
        Invocation {
            decl_site: DeclSite::Manifest {
                name: name.to_string(),
            },
            module: GeneratorModule::Spec(format!("ns:{name}@1.0.0")),
            from: InvocationPath::normalize(from),
            inputs: inputs
                .iter()
                .map(|p| InvocationPath::normalize(p))
                .collect(),
            output_dir: InvocationPath::normalize(out),
            options_canonical: vec![],
        }
    }

    #[test]
    fn path_inside_direct_child() {
        let dir = InvocationPath::normalize("build/kiln/a");
        let file = InvocationPath::normalize("build/kiln/a/out.wado");
        assert!(path_inside(&file, &dir));
    }

    #[test]
    fn path_inside_equal() {
        let dir = InvocationPath::normalize("build/kiln/a");
        assert!(path_inside(&dir, &dir));
    }

    #[test]
    fn path_inside_rejects_prefix_without_slash() {
        let dir = InvocationPath::normalize("build/kiln/a");
        let sibling = InvocationPath::normalize("build/kiln/abc");
        assert!(!path_inside(&sibling, &dir));
    }

    #[test]
    fn path_inside_rejects_unrelated() {
        let dir = InvocationPath::normalize("build/kiln/a");
        let file = InvocationPath::normalize("src/main.wado");
        assert!(!path_inside(&file, &dir));
    }

    #[test]
    fn no_edges_when_outputs_disjoint() {
        let a = inv("a", "s1.proto", &[], "build/kiln/a");
        let b = inv("b", "s2.proto", &[], "build/kiln/b");
        assert!(!depends_on(&a, &b));
        assert!(!depends_on(&b, &a));
    }

    #[test]
    fn a_depends_on_b_when_a_reads_b_output() {
        let a = inv("a", "build/kiln/b/schema.proto", &[], "build/kiln/a");
        let b = inv("b", "s2.proto", &[], "build/kiln/b");
        assert!(depends_on(&a, &b));
    }

    #[test]
    fn plan_preserves_independent_order() {
        let a = inv("a", "s1.proto", &[], "build/kiln/a");
        let b = inv("b", "s2.proto", &[], "build/kiln/b");
        let plan = build_plan(vec![a, b]).unwrap();
        assert_eq!(plan.order.len(), 2);
    }

    #[test]
    fn plan_orders_dependent_after_producer() {
        let consumer = inv(
            "consumer",
            "build/kiln/producer/out.proto",
            &[],
            "build/kiln/consumer",
        );
        let producer = inv("producer", "s.proto", &[], "build/kiln/producer");
        let plan = build_plan(vec![consumer, producer]).unwrap();
        let positions: Vec<_> = plan
            .order
            .iter()
            .map(|i| match &i.decl_site {
                DeclSite::Manifest { name } => name.clone(),
                _ => panic!(),
            })
            .collect();
        let producer_pos = positions.iter().position(|s| s == "producer").unwrap();
        let consumer_pos = positions.iter().position(|s| s == "consumer").unwrap();
        assert!(producer_pos < consumer_pos);
    }

    #[test]
    fn plan_detects_cycle() {
        let a = inv("a", "build/kiln/b/x.proto", &[], "build/kiln/a");
        let b = inv("b", "build/kiln/a/y.proto", &[], "build/kiln/b");
        let err = build_plan(vec![a, b]).unwrap_err();
        match err {
            PlanError::Cycle { participants } => {
                assert_eq!(participants.len(), 2);
            }
            other => panic!("expected cycle, got {other:?}"),
        }
    }

    #[test]
    fn dedup_merges_identical_duplicates() {
        let a1 = inv("a", "s.proto", &[], "build/kiln/a");
        let a2 = inv("a", "s.proto", &[], "build/kiln/a");
        let plan = build_plan(vec![a1, a2]).unwrap();
        assert_eq!(plan.order.len(), 1);
    }

    #[test]
    fn dedup_rejects_conflicting_duplicates() {
        let a1 = inv("a", "s.proto", &[], "build/kiln/a");
        let mut a2 = inv("a", "s.proto", &[], "build/kiln/a");
        a2.options_canonical = vec![0xde, 0xad, 0xbe, 0xef];
        let err = build_plan(vec![a1, a2]).unwrap_err();
        match err {
            PlanError::DuplicateGenerator { from, sites } => {
                assert_eq!(from, "s.proto");
                assert_eq!(sites.len(), 2);
            }
            other => panic!("expected duplicate, got {other:?}"),
        }
    }

    fn names(plan: &Plan) -> Vec<String> {
        plan.order
            .iter()
            .map(|i| match &i.decl_site {
                DeclSite::Manifest { name } => name.clone(),
                _ => panic!("expected manifest decl site"),
            })
            .collect()
    }

    #[test]
    fn topo_sort_respects_declaration_order_among_independent_invocations() {
        let alpha = inv("alpha", "a.proto", &[], "build/kiln/alpha");
        let beta = inv("beta", "b.proto", &[], "build/kiln/beta");
        let plan = build_plan(vec![alpha, beta]).unwrap();
        assert_eq!(names(&plan), vec!["alpha", "beta"]);
    }

    #[test]
    fn topo_sort_order_is_deterministic_across_runs() {
        let build_diamond = || {
            let producer = inv("producer", "s.proto", &[], "build/kiln/producer");
            let left = inv(
                "left",
                "build/kiln/producer/l.proto",
                &[],
                "build/kiln/left",
            );
            let right = inv(
                "right",
                "build/kiln/producer/r.proto",
                &[],
                "build/kiln/right",
            );
            let sink = inv(
                "sink",
                "build/kiln/left/x.proto",
                &["build/kiln/right/y.proto"],
                "build/kiln/sink",
            );
            vec![producer, left, right, sink]
        };
        let a = build_plan(build_diamond()).unwrap();
        let b = build_plan(build_diamond()).unwrap();
        assert_eq!(names(&a), names(&b));
        let positions = names(&a);
        let pos = |name: &str| positions.iter().position(|s| s == name).unwrap();
        assert!(pos("producer") < pos("left"));
        assert!(pos("producer") < pos("right"));
        assert!(pos("left") < pos("sink"));
        assert!(pos("right") < pos("sink"));
    }
}
