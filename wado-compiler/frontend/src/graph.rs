//! Algorithms over a directed graph given as successor lists.

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

#[cfg(test)]
mod tests {
    use super::strongly_connected_components;

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
