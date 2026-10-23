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
    use super::{DisjointSets, strongly_connected_components};

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
}
