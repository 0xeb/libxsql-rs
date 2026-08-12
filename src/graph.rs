// Copyright (c) 2024-2026 Elias Bachaalany
// SPDX-License-Identifier: LicenseRef-Human-Origin-Source-1.0
//
// This file is licensed under the Human-Origin Source License v1.0.
// See LICENSE.

//! A general-purpose directed-graph algorithm utility (the Rust port of the C++
//! `xsql::graph` header).
//!
//! This is a **stock** graph library, not a reverse-engineering facility: it knows
//! nothing about addresses, basic blocks, control flow, or any application concept.
//! A graph is a node count plus a directed edge list over opaque integer node ids in
//! `0..node_count`. Any caller that can map its own objects onto integer ids can use
//! it.
//!
//! Provided, all on opaque node ids:
//! - `immediate_dominators` / `dominator_sets` (relative to an entry)
//! - `immediate_post_dominators` (relative to the graph's sinks, via a virtual exit)
//! - `natural_loops` (back-edge + dominator based)
//! - `strongly_connected_components` (Tarjan)
//! - `topological_order` (Kahn; `None` when the graph has a cycle)

/// Sentinel for "no node" (an unreachable node's dominator, a node that
/// post-dominates straight to the virtual exit, etc.).
pub const NO_NODE: usize = usize::MAX;

/// A directed graph over node ids `0..node_count`. Parallel edges and self-loops are
/// permitted; every algorithm below tolerates both.
#[derive(Clone, Debug, Default)]
pub struct DirectedGraph {
    succ: Vec<Vec<usize>>,
    pred: Vec<Vec<usize>>,
}

impl DirectedGraph {
    /// Create a graph with `node_count` nodes and no edges.
    pub fn new(node_count: usize) -> Self {
        DirectedGraph {
            succ: vec![Vec::new(); node_count],
            pred: vec![Vec::new(); node_count],
        }
    }

    /// Number of nodes.
    pub fn node_count(&self) -> usize {
        self.succ.len()
    }

    /// Add a directed edge `from -> to`. Both ids must be `< node_count()`.
    pub fn add_edge(&mut self, from: usize, to: usize) {
        self.succ[from].push(to);
        self.pred[to].push(from);
    }

    /// Successors of `n`.
    pub fn successors(&self, n: usize) -> &[usize] {
        &self.succ[n]
    }

    /// Predecessors of `n`.
    pub fn predecessors(&self, n: usize) -> &[usize] {
        &self.pred[n]
    }
}

/// Iterative DFS from `entry` producing a postorder listing (children finish before
/// their parent; `entry` is last). `succ` selects the traversal direction so the same
/// routine serves forward and reverse graphs.
fn postorder_from(node_count: usize, entry: usize, succ: &[Vec<usize>]) -> Vec<usize> {
    let mut order = Vec::new();
    if entry >= node_count {
        return order;
    }
    let mut visited = vec![false; node_count];
    let mut stack: Vec<(usize, usize)> = Vec::new();
    visited[entry] = true;
    stack.push((entry, 0));
    while let Some(&mut (n, ref mut i)) = stack.last_mut() {
        if *i < succ[n].len() {
            let m = succ[n][*i];
            *i += 1;
            if m < node_count && !visited[m] {
                visited[m] = true;
                stack.push((m, 0));
            }
        } else {
            order.push(n);
            stack.pop();
        }
    }
    order
}

/// Cooper-Harvey-Kennedy immediate dominators over explicit successor/predecessor
/// adjacency (so it serves a forward graph OR a virtual-exit-augmented reverse graph).
/// `idom[entry] == entry`; unreachable nodes keep `NO_NODE`.
fn idom_over(
    node_count: usize,
    entry: usize,
    succ: &[Vec<usize>],
    pred: &[Vec<usize>],
) -> Vec<usize> {
    let mut idom = vec![NO_NODE; node_count];
    if entry >= node_count {
        return idom;
    }

    let post = postorder_from(node_count, entry, succ);
    let mut postnum = vec![NO_NODE; node_count];
    for (i, &n) in post.iter().enumerate() {
        postnum[n] = i;
    }
    // Reverse postorder (entry first) over the reachable set only.
    let rpo: Vec<usize> = post.iter().rev().copied().collect();

    let intersect = |idom: &[usize], mut a: usize, mut b: usize| -> usize {
        while a != b {
            while postnum[a] < postnum[b] {
                a = idom[a];
            }
            while postnum[b] < postnum[a] {
                b = idom[b];
            }
        }
        a
    };

    idom[entry] = entry;
    let mut changed = true;
    while changed {
        changed = false;
        for &b in &rpo {
            if b == entry {
                continue;
            }
            let mut new_idom = NO_NODE;
            for &p in &pred[b] {
                if p >= node_count || postnum[p] == NO_NODE {
                    continue; // unreachable pred
                }
                if idom[p] == NO_NODE && p != entry {
                    continue; // not yet processed
                }
                new_idom = if new_idom == NO_NODE {
                    p
                } else {
                    intersect(&idom, p, new_idom)
                };
            }
            if new_idom != NO_NODE && idom[b] != new_idom {
                idom[b] = new_idom;
                changed = true;
            }
        }
    }
    idom
}

/// Immediate dominators relative to `entry`. `idom[entry] == entry`; a node
/// unreachable from `entry` keeps `NO_NODE`.
pub fn immediate_dominators(g: &DirectedGraph, entry: usize) -> Vec<usize> {
    idom_over(g.node_count(), entry, &g.succ, &g.pred)
}

/// Full dominator sets: `dominators[n]` is the sorted set of all nodes that dominate
/// `n` (including `n` itself). Unreachable nodes get an empty set.
pub fn dominator_sets(g: &DirectedGraph, entry: usize) -> Vec<Vec<usize>> {
    let idom = immediate_dominators(g, entry);
    let mut dom = vec![Vec::new(); g.node_count()];
    for n in 0..g.node_count() {
        if idom[n] == NO_NODE {
            continue; // unreachable
        }
        let mut cur = n;
        dom[n].push(cur);
        while cur != entry {
            cur = idom[cur];
            dom[n].push(cur);
        }
        dom[n].sort_unstable();
    }
    dom
}

/// Immediate post-dominators. Every node's ipdom is the first node on every path from
/// it to a graph sink (a node with no successors). Computed as immediate dominators on
/// the reversed graph rooted at a virtual exit that all sinks feed. `ipdom[n] ==
/// NO_NODE` means `n` post-dominates directly to the virtual exit (it is, or only
/// reaches, a sink); a node that cannot reach any sink also keeps `NO_NODE`.
pub fn immediate_post_dominators(g: &DirectedGraph) -> Vec<usize> {
    let n = g.node_count();
    let virt = n; // virtual exit id

    let mut rev_succ = vec![Vec::new(); n + 1];
    let mut rev_pred = vec![Vec::new(); n + 1];
    // The index `u` addresses several distinct adjacency vectors (rev_succ[v],
    // rev_pred[u], rev_succ[virt]), so a range loop is the clearest form here.
    #[allow(clippy::needless_range_loop)]
    for u in 0..n {
        for &v in g.successors(u) {
            rev_succ[v].push(u); // reverse each edge
            rev_pred[u].push(v);
        }
        if g.successors(u).is_empty() {
            rev_succ[virt].push(u); // sink <- virtual exit
            rev_pred[u].push(virt);
        }
    }

    let ipdom = idom_over(n + 1, virt, &rev_succ, &rev_pred);

    let mut out = vec![NO_NODE; n];
    for x in 0..n {
        let d = ipdom[x];
        out[x] = if d == NO_NODE || d == virt {
            NO_NODE
        } else {
            d
        };
    }
    out
}

/// A natural loop: the header (loop entry that dominates the whole body), the latch
/// (the back-edge tail), and the sorted set of body nodes (header + latch included).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NaturalLoop {
    /// Loop header (dominates every body node).
    pub header: usize,
    /// Back-edge tail.
    pub latch: usize,
    /// Sorted body node ids (includes header and latch).
    pub body: Vec<usize>,
}

/// Natural loops of the graph relative to `entry`. For each edge `latch -> header`
/// where `header` dominates `latch` (a back edge), the loop body is `header` plus
/// every node that reaches `latch` without passing through `header`.
pub fn natural_loops(g: &DirectedGraph, entry: usize) -> Vec<NaturalLoop> {
    let n = g.node_count();
    let idom = immediate_dominators(g, entry);

    let dominates = |a: usize, b: usize| -> bool {
        if idom[b] == NO_NODE {
            return false; // b unreachable
        }
        let mut cur = b;
        loop {
            if cur == a {
                return true;
            }
            if cur == entry {
                return a == entry;
            }
            cur = idom[cur];
        }
    };

    let mut loops = Vec::new();
    for latch in 0..n {
        if idom[latch] == NO_NODE {
            continue; // unreachable latch
        }
        for &header in g.successors(latch) {
            if header >= n || !dominates(header, latch) {
                continue; // not a back edge
            }
            let mut in_body = vec![false; n];
            let mut stack = Vec::new();
            in_body[header] = true; // header bounds the backward walk
            if !in_body[latch] {
                in_body[latch] = true;
                stack.push(latch);
            }
            while let Some(m) = stack.pop() {
                for &p in g.predecessors(m) {
                    if p < n && idom[p] != NO_NODE && !in_body[p] {
                        in_body[p] = true;
                        stack.push(p);
                    }
                }
            }
            let body: Vec<usize> = (0..n).filter(|&x| in_body[x]).collect();
            loops.push(NaturalLoop {
                header,
                latch,
                body,
            });
        }
    }
    loops
}

/// Strongly connected components (iterative Tarjan). Returns `comp[n]` = a component
/// id in `0..component_count`; nodes in the same SCC share an id.
pub fn strongly_connected_components(g: &DirectedGraph) -> Vec<usize> {
    let n = g.node_count();
    let mut comp = vec![NO_NODE; n];
    let mut index = vec![NO_NODE; n];
    let mut low = vec![0usize; n];
    let mut on_stack = vec![false; n];
    let mut scc_stack: Vec<usize> = Vec::new();
    let mut next_index = 0usize;
    let mut next_comp = 0usize;

    // Explicit work stack of (node, next-successor-index) for iterative Tarjan.
    let mut work: Vec<(usize, usize)> = Vec::new();
    for root in 0..n {
        if index[root] != NO_NODE {
            continue;
        }
        work.push((root, 0));
        while let Some(&(v, i)) = work.last() {
            if i == 0 {
                index[v] = next_index;
                low[v] = next_index;
                next_index += 1;
                scc_stack.push(v);
                on_stack[v] = true;
            }
            let succ = g.successors(v);
            if i < succ.len() {
                work.last_mut().unwrap().1 += 1;
                let w = succ[i];
                if w >= n {
                    continue;
                }
                if index[w] == NO_NODE {
                    work.push((w, 0)); // recurse
                } else if on_stack[w] {
                    low[v] = low[v].min(index[w]);
                }
            } else {
                if low[v] == index[v] {
                    loop {
                        let w = scc_stack.pop().unwrap();
                        on_stack[w] = false;
                        comp[w] = next_comp;
                        if w == v {
                            break;
                        }
                    }
                    next_comp += 1;
                }
                work.pop();
                if let Some(&(parent, _)) = work.last() {
                    low[parent] = low[parent].min(low[v]);
                }
            }
        }
    }
    comp
}

/// Topological order (Kahn). Returns `None` if the graph has a directed cycle;
/// otherwise a full ordering of all nodes with every edge pointing forward
/// (smallest-id-first among ready nodes, for a deterministic result).
pub fn topological_order(g: &DirectedGraph) -> Option<Vec<usize>> {
    let n = g.node_count();
    let mut indeg = vec![0usize; n];
    for u in 0..n {
        for &v in g.successors(u) {
            if v < n {
                indeg[v] += 1;
            }
        }
    }

    // Min-heap behavior via a sorted-descending vector we pop from the back.
    let mut ready: Vec<usize> = (0..n).filter(|&u| indeg[u] == 0).collect();
    ready.sort_unstable_by(|a, b| b.cmp(a));

    let mut order = Vec::with_capacity(n);
    while let Some(u) = ready.pop() {
        order.push(u);
        for &v in g.successors(u) {
            if v >= n {
                continue;
            }
            indeg[v] -= 1;
            if indeg[v] == 0 {
                // insert keeping the descending invariant so pop() yields the smallest.
                let pos = ready.partition_point(|&x| x > v);
                ready.insert(pos, v);
            }
        }
    }
    if order.len() == n {
        Some(order)
    } else {
        None // cycle
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn set(v: &[usize]) -> BTreeSet<usize> {
        v.iter().copied().collect()
    }

    #[test]
    fn dominators_linear_chain() {
        let mut g = DirectedGraph::new(4);
        g.add_edge(0, 1);
        g.add_edge(1, 2);
        g.add_edge(2, 3);
        let idom = immediate_dominators(&g, 0);
        assert_eq!(idom, vec![0, 0, 1, 2]);
    }

    #[test]
    fn dominators_diamond() {
        let mut g = DirectedGraph::new(4);
        g.add_edge(0, 1);
        g.add_edge(0, 2);
        g.add_edge(1, 3);
        g.add_edge(2, 3);
        let idom = immediate_dominators(&g, 0);
        assert_eq!(idom, vec![0, 0, 0, 0]);
    }

    #[test]
    fn dominators_unreachable_node() {
        let mut g = DirectedGraph::new(3);
        g.add_edge(0, 1);
        let idom = immediate_dominators(&g, 0);
        assert_eq!(idom[2], NO_NODE);
    }

    #[test]
    fn dominators_self_loop_tolerated() {
        let mut g = DirectedGraph::new(2);
        g.add_edge(0, 0);
        g.add_edge(0, 1);
        let idom = immediate_dominators(&g, 0);
        assert_eq!(idom, vec![0, 0]);
    }

    #[test]
    fn dominator_sets_diamond() {
        let mut g = DirectedGraph::new(4);
        g.add_edge(0, 1);
        g.add_edge(0, 2);
        g.add_edge(1, 3);
        g.add_edge(2, 3);
        let dom = dominator_sets(&g, 0);
        assert_eq!(set(&dom[3]), set(&[0, 3]));
        assert_eq!(set(&dom[1]), set(&[0, 1]));
    }

    #[test]
    fn post_dominators_diamond_converges() {
        let mut g = DirectedGraph::new(4);
        g.add_edge(0, 1);
        g.add_edge(0, 2);
        g.add_edge(1, 3);
        g.add_edge(2, 3);
        let ipdom = immediate_post_dominators(&g);
        assert_eq!(ipdom[0], 3);
        assert_eq!(ipdom[1], 3);
        assert_eq!(ipdom[2], 3);
        assert_eq!(ipdom[3], NO_NODE);
    }

    #[test]
    fn natural_loop_single() {
        let mut g = DirectedGraph::new(3);
        g.add_edge(0, 1);
        g.add_edge(1, 2);
        g.add_edge(2, 1);
        let loops = natural_loops(&g, 0);
        assert_eq!(loops.len(), 1);
        assert_eq!(loops[0].header, 1);
        assert_eq!(loops[0].latch, 2);
        assert_eq!(set(&loops[0].body), set(&[1, 2]));
    }

    #[test]
    fn natural_loops_nested() {
        let mut g = DirectedGraph::new(5);
        g.add_edge(0, 1);
        g.add_edge(1, 2);
        g.add_edge(2, 3);
        g.add_edge(3, 2); // inner back edge
        g.add_edge(3, 4);
        g.add_edge(4, 1); // outer back edge
        let loops = natural_loops(&g, 0);
        assert_eq!(loops.len(), 2);
        let inner = loops.iter().find(|l| l.header == 2).unwrap();
        let outer = loops.iter().find(|l| l.header == 1).unwrap();
        assert_eq!(set(&inner.body), set(&[2, 3]));
        assert_eq!(set(&outer.body), set(&[1, 2, 3, 4]));
    }

    #[test]
    fn scc_simple_cycle() {
        let mut g = DirectedGraph::new(5);
        g.add_edge(0, 1);
        g.add_edge(1, 2);
        g.add_edge(2, 0);
        g.add_edge(2, 3);
        g.add_edge(3, 4);
        let comp = strongly_connected_components(&g);
        assert_eq!(comp[0], comp[1]);
        assert_eq!(comp[1], comp[2]);
        assert_ne!(comp[0], comp[3]);
        assert_ne!(comp[3], comp[4]);
    }

    #[test]
    fn scc_irreducible_loop() {
        let mut g = DirectedGraph::new(3);
        g.add_edge(0, 1);
        g.add_edge(0, 2);
        g.add_edge(1, 2);
        g.add_edge(2, 1);
        let comp = strongly_connected_components(&g);
        assert_eq!(comp[1], comp[2]);
        assert_ne!(comp[0], comp[1]);
    }

    #[test]
    fn topo_dag_every_edge_forward() {
        let mut g = DirectedGraph::new(4);
        g.add_edge(0, 1);
        g.add_edge(0, 2);
        g.add_edge(1, 3);
        g.add_edge(2, 3);
        let order = topological_order(&g).expect("DAG has a topo order");
        let mut pos = [0usize; 4];
        for (i, &node) in order.iter().enumerate() {
            pos[node] = i;
        }
        assert!(pos[0] < pos[1]);
        assert!(pos[0] < pos[2]);
        assert!(pos[1] < pos[3]);
        assert!(pos[2] < pos[3]);
    }

    #[test]
    fn topo_cycle_is_none() {
        let mut g = DirectedGraph::new(2);
        g.add_edge(0, 1);
        g.add_edge(1, 0);
        assert!(topological_order(&g).is_none());
    }

    #[test]
    fn topo_self_loop_is_cycle() {
        let mut g = DirectedGraph::new(1);
        g.add_edge(0, 0);
        assert!(topological_order(&g).is_none());
    }
}
