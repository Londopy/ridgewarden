//! Topology analysis: articulation points, bridges and "what if this dies".
//!
//! Hand-rolled Tarjan rather than a graph crate: the mesh is tens of sites, and the
//! whole thing is a page of code that the planner (spec 03) will reuse.

use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, Default)]
pub struct Graph {
    pub adj: BTreeMap<String, BTreeSet<String>>,
}

pub type Edge = (String, String);

pub fn edge(a: &str, b: &str) -> Edge {
    if a <= b {
        (a.into(), b.into())
    } else {
        (b.into(), a.into())
    }
}

impl Graph {
    pub fn add_node(&mut self, n: &str) {
        self.adj.entry(n.to_string()).or_default();
    }

    pub fn add_edge(&mut self, a: &str, b: &str) {
        if a == b {
            return;
        }
        self.adj
            .entry(a.to_string())
            .or_default()
            .insert(b.to_string());
        self.adj
            .entry(b.to_string())
            .or_default()
            .insert(a.to_string());
    }

    pub fn nodes(&self) -> impl Iterator<Item = &String> {
        self.adj.keys()
    }

    pub fn edges(&self) -> BTreeSet<Edge> {
        self.adj
            .iter()
            .flat_map(|(a, ns)| ns.iter().map(move |b| edge(a, b)))
            .collect()
    }

    /// Articulation points and bridges in one DFS.
    pub fn cut_points(&self) -> (BTreeSet<String>, BTreeSet<Edge>) {
        let names: Vec<&String> = self.adj.keys().collect();
        let ix: BTreeMap<&String, usize> = names.iter().enumerate().map(|(i, n)| (*n, i)).collect();
        let adj: Vec<Vec<usize>> = names
            .iter()
            .map(|n| self.adj[*n].iter().map(|m| ix[m]).collect())
            .collect();
        let n = names.len();
        let mut disc = vec![usize::MAX; n];
        let mut low = vec![0; n];
        let mut time = 0;
        let mut aps = BTreeSet::new();
        let mut bridges = BTreeSet::new();

        // iterative DFS so a long chain of sites can't blow the stack
        for root in 0..n {
            if disc[root] != usize::MAX {
                continue;
            }
            let mut stack: Vec<(usize, usize, usize)> = vec![(root, usize::MAX, 0)];
            disc[root] = time;
            low[root] = time;
            time += 1;
            let mut root_children = 0;
            while let Some(&mut (u, parent, ref mut i)) = stack.last_mut() {
                if *i < adj[u].len() {
                    let v = adj[u][*i];
                    *i += 1;
                    if v == parent {
                        continue;
                    }
                    if disc[v] == usize::MAX {
                        disc[v] = time;
                        low[v] = time;
                        time += 1;
                        if u == root {
                            root_children += 1;
                        }
                        stack.push((v, u, 0));
                    } else {
                        low[u] = low[u].min(disc[v]);
                    }
                } else {
                    stack.pop();
                    if parent != usize::MAX {
                        low[parent] = low[parent].min(low[u]);
                        if low[u] > disc[parent] {
                            bridges.insert(edge(names[parent], names[u]));
                        }
                        if parent != root && low[u] >= disc[parent] {
                            aps.insert(names[parent].clone());
                        }
                    }
                }
            }
            if root_children > 1 {
                aps.insert(names[root].clone());
            }
        }
        (aps, bridges)
    }

    /// Connected components after removing some nodes and edges, largest first.
    pub fn components(
        &self,
        gone_nodes: &BTreeSet<String>,
        gone_edges: &BTreeSet<Edge>,
    ) -> Vec<BTreeSet<String>> {
        let mut seen = BTreeSet::new();
        let mut out = vec![];
        for start in self.adj.keys() {
            if gone_nodes.contains(start) || seen.contains(start) {
                continue;
            }
            let mut comp = BTreeSet::new();
            let mut todo = vec![start.clone()];
            seen.insert(start.clone());
            while let Some(u) = todo.pop() {
                for v in &self.adj[&u] {
                    if gone_nodes.contains(v) || gone_edges.contains(&edge(&u, v)) {
                        continue;
                    }
                    if seen.insert(v.clone()) {
                        todo.push(v.clone());
                    }
                }
                comp.insert(u);
            }
            out.push(comp);
        }
        out.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
        out
    }

    /// Which sites lose contact with the main (largest) part of the network if the given
    /// sites and links fail. `anchor`, if given, decides which side counts as "main" when
    /// it survives (the side the viewer is on).
    pub fn what_if(
        &self,
        gone_nodes: &BTreeSet<String>,
        gone_edges: &BTreeSet<Edge>,
        anchor: Option<&str>,
    ) -> BTreeSet<String> {
        let comps = self.components(gone_nodes, gone_edges);
        let main = anchor
            .and_then(|a| comps.iter().position(|c| c.contains(a)))
            .unwrap_or(0);
        comps
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != main)
            .flat_map(|(_, c)| c.iter().cloned())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(xs: &[&str]) -> BTreeSet<String> {
        xs.iter().map(|s| s.to_string()).collect()
    }

    // the demo topology:  lopez - cuesta - madonna - shack
    //                               \        /
    //                               black - north - paso
    fn demo() -> Graph {
        let mut g = Graph::default();
        for (a, b) in [
            ("cuesta", "lopez"),
            ("cuesta", "madonna"),
            ("cuesta", "black"),
            ("madonna", "black"),
            ("madonna", "shack"),
            ("black", "north"),
            ("north", "paso"),
        ] {
            g.add_edge(a, b);
        }
        g
    }

    #[test]
    fn finds_articulation_points_and_bridges() {
        let (aps, bridges) = demo().cut_points();
        assert_eq!(aps, set(&["black", "cuesta", "madonna", "north"]));
        let expect: BTreeSet<Edge> = [
            edge("cuesta", "lopez"),
            edge("madonna", "shack"),
            edge("black", "north"),
            edge("north", "paso"),
        ]
        .into_iter()
        .collect();
        assert_eq!(bridges, expect);
    }

    #[test]
    fn cycle_has_no_cut_points() {
        let mut g = Graph::default();
        g.add_edge("a", "b");
        g.add_edge("b", "c");
        g.add_edge("c", "a");
        let (aps, bridges) = g.cut_points();
        assert!(aps.is_empty() && bridges.is_empty());
    }

    #[test]
    fn what_if_reports_the_cut_off_side() {
        let g = demo();
        assert_eq!(
            g.what_if(&set(&["black"]), &BTreeSet::new(), None),
            set(&["north", "paso"])
        );
        let cut = [edge("black", "north")].into_iter().collect();
        // viewed from paso, the "main" side is paso's own island
        assert_eq!(
            g.what_if(&BTreeSet::new(), &cut, Some("paso")),
            set(&["black", "cuesta", "lopez", "madonna", "shack"])
        );
        assert!(
            g.what_if(&set(&["shack"]), &BTreeSet::new(), None)
                .is_empty()
        );
    }

    #[test]
    fn long_chain_does_not_overflow() {
        let mut g = Graph::default();
        for i in 0..50_000 {
            g.add_edge(&format!("n{i}"), &format!("n{}", i + 1));
        }
        let (aps, bridges) = g.cut_points();
        assert_eq!(aps.len(), 49_999);
        assert_eq!(bridges.len(), 50_000);
    }
}
