//! Algorithms over a graph: components of a directed one given as successor
//! lists, and of an undirected one built edge by edge.

/// The strongly connected components of `successors`, each after every
/// component it reaches, so a callee's component comes before its callers'.
pub fn strongly_connected_components(successors: &[Vec<usize>]) -> Vec<Vec<usize>> {
    const UNVISITED: usize = usize::MAX;
    let n = successors.len();
    let mut index = vec![UNVISITED; n];
    let mut low = vec![0; n];
    let mut on_stack = vec![false; n];
    let mut stack = Vec::new();
    let mut next_index = 0;
    let mut out = Vec::new();
    // An explicit stack of (node, next successor): a call graph is deep enough
    // to overflow a recursive walk.
    let mut work: Vec<(usize, usize)> = Vec::new();
    for root in 0..n {
        if index[root] != UNVISITED {
            continue;
        }
        work.push((root, 0));
        while let Some((v, child)) = work.pop() {
            if child == 0 {
                index[v] = next_index;
                low[v] = next_index;
                next_index += 1;
                stack.push(v);
                on_stack[v] = true;
            }
            if let Some(&w) = successors[v].get(child) {
                work.push((v, child + 1));
                if index[w] == UNVISITED {
                    work.push((w, 0));
                } else if on_stack[w] {
                    low[v] = low[v].min(index[w]);
                }
                continue;
            }
            if low[v] == index[v] {
                let mut component = Vec::new();
                loop {
                    let w = stack.pop().expect("the Tarjan stack holds the component");
                    on_stack[w] = false;
                    component.push(w);
                    if w == v {
                        break;
                    }
                }
                out.push(component);
            }
            if let Some(&(parent, _)) = work.last() {
                low[parent] = low[parent].min(low[v]);
            }
        }
    }
    out
}

/// A least fixpoint over the components of a graph, solved in sweeps: each
/// sweep visits the stale nodes rank by rank, so a change reaches every
/// predecessor ranked above it in the same sweep, and only one along a dropped
/// edge waits for the next.
pub struct RankedSweep {
    rank: Vec<usize>,
    /// Each node's predecessors in its own component.
    predecessors_within: Vec<Vec<usize>>,
    stale: Vec<bool>,
}

impl RankedSweep {
    pub fn new(successors: &[Vec<usize>], component_of: &[usize]) -> Self {
        let mut predecessors_within: Vec<Vec<usize>> = vec![Vec::new(); successors.len()];
        for (n, nexts) in successors.iter().enumerate() {
            for &s in nexts {
                if component_of[s] == component_of[n] {
                    predecessors_within[s].push(n);
                }
            }
        }
        Self {
            rank: ranks_within_components(successors, component_of),
            predecessors_within,
            stale: vec![false; successors.len()],
        }
    }

    /// Solves `nodes`, whole components, to their fixpoint. `visit` takes a
    /// batch of one rank, nodes it may visit in any order, and answers those
    /// whose value changed, whose predecessors are then stale.
    pub fn solve(&mut self, nodes: &[usize], mut visit: impl FnMut(&[usize]) -> Vec<usize>) {
        let ranks = nodes.iter().map(|&n| self.rank[n] + 1).max().unwrap_or(0);
        let mut by_rank: Vec<Vec<usize>> = vec![Vec::new(); ranks];
        for &n in nodes {
            self.stale[n] = true;
            by_rank[self.rank[n]].push(n);
        }
        while by_rank.iter().any(|batch| !batch.is_empty()) {
            for at in 0..ranks {
                let batch = std::mem::take(&mut by_rank[at]);
                if batch.is_empty() {
                    continue;
                }
                for &n in &batch {
                    self.stale[n] = false;
                }
                for n in visit(&batch) {
                    for &p in &self.predecessors_within[n] {
                        if !self.stale[p] {
                            self.stale[p] = true;
                            by_rank[self.rank[p]].push(p);
                        }
                    }
                }
            }
        }
    }
}

/// Each node's rank within its component of `successors`, `component_of`
/// naming it: one past the highest rank among its successors there, an edge
/// that closes a cycle aside. A depth-first search drops those edges, the ones
/// it meets to a node it has not finished, so what is left orders every
/// component, and nodes of one rank are joined only by a dropped edge.
fn ranks_within_components(successors: &[Vec<usize>], component_of: &[usize]) -> Vec<usize> {
    #[derive(Clone, Copy, PartialEq)]
    enum Visit {
        New,
        Open,
        Done,
    }
    let mut rank = vec![0; successors.len()];
    let mut visit = vec![Visit::New; successors.len()];
    // An explicit stack of (node, next successor): a call graph is deep enough
    // to overflow a recursive walk.
    let mut work: Vec<(usize, usize)> = Vec::new();
    for root in 0..successors.len() {
        if visit[root] != Visit::New {
            continue;
        }
        visit[root] = Visit::Open;
        work.push((root, 0));
        while let Some(&mut (n, ref mut next)) = work.last_mut() {
            let next_within = successors[n][*next..]
                .iter()
                .position(|&s| component_of[s] == component_of[n]);
            if let Some(at) = next_within {
                let s = successors[n][*next + at];
                *next += at + 1;
                if visit[s] == Visit::New {
                    visit[s] = Visit::Open;
                    work.push((s, 0));
                }
                continue;
            }
            work.pop();
            rank[n] = successors[n]
                .iter()
                .filter(|&&s| component_of[s] == component_of[n] && visit[s] == Visit::Done)
                .map(|&s| rank[s] + 1)
                .max()
                .unwrap_or(0);
            visit[n] = Visit::Done;
        }
    }
    rank
}

/// A partition of the nodes `0..n` into sets that only ever merge.
#[derive(Clone, Default)]
pub struct DisjointSets {
    parent: Vec<u32>,
}

impl DisjointSets {
    pub fn new(len: usize) -> Self {
        Self {
            parent: (0..u32::try_from(len).expect("node count fits u32")).collect(),
        }
    }

    /// A new node, alone in its set.
    pub fn push(&mut self) -> u32 {
        let node = u32::try_from(self.parent.len()).expect("node count fits u32");
        self.parent.push(node);
        node
    }

    /// The node standing for `node`'s set.
    pub fn find(&mut self, mut node: u32) -> u32 {
        while self.parent[node as usize] != node {
            let grand = self.parent[self.parent[node as usize] as usize];
            self.parent[node as usize] = grand;
            node = grand;
        }
        node
    }

    /// Merge the set `absorbed` stands for into the one `kept` stands for.
    pub fn link(&mut self, absorbed: u32, kept: u32) {
        assert!(
            self.parent[absorbed as usize] == absorbed && self.parent[kept as usize] == kept,
            "only the node standing for a set links"
        );
        self.parent[absorbed as usize] = kept;
    }

    /// Merge `a`'s set into `b`'s, answering the nodes that stood for them,
    /// absorbed then kept, where they were apart.
    pub fn union(&mut self, a: u32, b: u32) -> Option<(u32, u32)> {
        let (a, b) = (self.find(a), self.find(b));
        (a != b).then(|| {
            self.link(a, b);
            (a, b)
        })
    }

    /// Point every node straight at the node standing for its set, so
    /// [`Self::root`] reads it without a `&mut`.
    pub fn flatten(&mut self) {
        for node in 0..self.parent.len() as u32 {
            let root = self.find(node);
            self.parent[node as usize] = root;
        }
    }

    /// The node standing for `node`'s set, once flattened.
    pub fn root(&self, node: u32) -> u32 {
        let root = self.parent[node as usize];
        assert!(
            self.parent[root as usize] == root,
            "a root is read only from flattened sets"
        );
        root
    }
}

#[cfg(test)]
mod tests {
    use super::{
        DisjointSets, RankedSweep, ranks_within_components, strongly_connected_components,
    };

    #[test]
    fn sets_merge_and_flatten() {
        let mut sets = DisjointSets::new(4);
        assert_eq!(sets.union(0, 1), Some((0, 1)));
        assert_eq!(sets.union(1, 2), Some((1, 2)));
        assert_eq!(sets.union(0, 2), None);
        assert_eq!(sets.push(), 4);
        sets.flatten();
        assert_eq!([0, 1, 2, 3, 4].map(|n| sets.root(n)), [2, 2, 2, 3, 4]);
    }

    #[test]
    fn components_come_callees_first() {
        // 0 -> 1 <-> 2 -> 3, and 3 loops on itself.
        let graph = vec![vec![1], vec![2], vec![1, 3], vec![3]];
        let mut components = strongly_connected_components(&graph);
        for c in &mut components {
            c.sort_unstable();
        }
        assert_eq!(components, vec![vec![3], vec![1, 2], vec![0]]);
    }

    #[test]
    fn ranks_order_a_component_but_the_edges_closing_cycles() {
        // One component: 0 -> 1 -> 2 -> 0 closes the cycle, and 1 -> 3 -> 2 is
        // a second path into 2. Node 4 loops on itself, and 5 calls into the
        // component from outside it.
        let graph = vec![vec![1], vec![2, 3], vec![0], vec![2], vec![4], vec![0]];
        let component_of = [0, 0, 0, 0, 1, 2];
        let rank = ranks_within_components(&graph, &component_of);
        assert_eq!(rank, vec![3, 2, 0, 1, 0, 0]);
        let dropped = [(2, 0), (4, 4)];
        for (n, callees) in graph.iter().enumerate() {
            for &s in callees {
                if component_of[s] == component_of[n] && !dropped.contains(&(n, s)) {
                    assert!(rank[n] > rank[s], "{n} -> {s} is kept, so it orders them");
                }
            }
        }
    }

    #[test]
    fn a_sweep_carries_a_change_back_along_a_dropped_edge() {
        // The graph of the ranking test. Each node's value is itself and every
        // value it reaches, so 2 learns of 0 and 1 only along the dropped 2 -> 0.
        let graph = vec![vec![1], vec![2, 3], vec![0], vec![2], vec![4], vec![0]];
        let component_of = [0, 0, 0, 0, 1, 2];
        let mut values: Vec<Vec<usize>> = vec![Vec::new(); graph.len()];
        let mut sweep = RankedSweep::new(&graph, &component_of);
        for component in [vec![0, 1, 2, 3], vec![4], vec![5]] {
            sweep.solve(&component, |batch| {
                let fresh: Vec<Vec<usize>> = batch
                    .iter()
                    .map(|&n| {
                        let mut value = vec![n];
                        value.extend(graph[n].iter().flat_map(|&s| values[s].iter().copied()));
                        value.sort_unstable();
                        value.dedup();
                        value
                    })
                    .collect();
                let mut changed = Vec::new();
                for (&n, value) in batch.iter().zip(fresh) {
                    if values[n] != value {
                        values[n] = value;
                        changed.push(n);
                    }
                }
                changed
            });
        }
        let cycle = vec![0, 1, 2, 3];
        assert_eq!(values, [cycle.clone(), cycle.clone(), cycle.clone(), cycle, vec![4], vec![0, 1, 2, 3, 5]]);
    }
}
