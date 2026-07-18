//! The directed acyclic graph of a Taskfile and its transitive includes.
//!
//! Each vertex is a parsed [`Taskfile`] keyed by its URI. Each edge points from
//! an including Taskfile to an included one and carries the list of [`Include`]
//! directives that produced it, so merging can replay every import in order.

use std::collections::HashMap;

use super::include::Include;
use super::taskfile::Taskfile;

/// A vertex on the Taskfile DAG.
#[derive(Clone, Debug)]
pub struct TaskfileVertex {
    pub uri: String,
    pub taskfile: Taskfile,
}

/// An edge from a base Taskfile to an included one, tagged with the includes
/// that created it.
#[derive(Clone, Debug)]
struct Edge {
    source: String,
    target: String,
    includes: Vec<Include>,
}

/// A rooted, directed, acyclic graph of Taskfiles.
///
/// The first vertex added becomes the root. Edges must not introduce a cycle;
/// [`add_edge`](TaskfileGraph::add_edge) rejects any edge that would.
#[derive(Debug, Default)]
pub struct TaskfileGraph {
    vertices: HashMap<String, TaskfileVertex>,
    /// Outgoing edges keyed by source URI, preserving insertion order.
    edges: HashMap<String, Vec<Edge>>,
    /// Insertion order of vertices; the first is the root.
    order: Vec<String>,
}

impl TaskfileGraph {
    /// Creates an empty graph.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds (or replaces) a vertex, using the given URI as its hash. The first
    /// vertex added is treated as the root.
    pub fn add_vertex(&mut self, vertex: TaskfileVertex) {
        let uri = vertex.uri.clone();
        if self.vertices.insert(uri.clone(), vertex).is_none() {
            self.order.push(uri);
        }
    }

    /// Returns the vertex with the given URI, if present.
    pub fn vertex(&self, uri: &str) -> Option<&TaskfileVertex> {
        self.vertices.get(uri)
    }

    /// Iterates every vertex (all read Taskfiles) in insertion order, root first.
    pub fn vertices(&self) -> impl Iterator<Item = &TaskfileVertex> {
        self.order.iter().filter_map(|uri| self.vertices.get(uri))
    }

    /// Adds a directed edge from `source` to `target` carrying `includes`.
    /// Returns an error if the edge would create a cycle.
    pub fn add_edge(
        &mut self,
        source: &str,
        target: &str,
        includes: Vec<Include>,
    ) -> Result<(), String> {
        // Adding source -> target creates a cycle if target already reaches
        // source.
        if self.reaches(target, source) {
            return Err(format!(
                "task: edge {source} -> {target} would create a cycle"
            ));
        }
        self.edges
            .entry(source.to_string())
            .or_default()
            .push(Edge {
                source: source.to_string(),
                target: target.to_string(),
                includes,
            });
        Ok(())
    }

    /// Reports whether `to` is reachable from `from` following directed edges.
    fn reaches(&self, from: &str, to: &str) -> bool {
        if from == to {
            return true;
        }
        let mut stack = vec![from.to_string()];
        let mut seen: HashMap<String, ()> = HashMap::new();
        while let Some(node) = stack.pop() {
            if seen.insert(node.clone(), ()).is_some() {
                continue;
            }
            if let Some(out) = self.edges.get(&node) {
                for edge in out {
                    if edge.target == to {
                        return true;
                    }
                    stack.push(edge.target.clone());
                }
            }
        }
        false
    }

    /// Returns the vertices in topological order (a source appears before every
    /// vertex it points to). Returns an error if the graph contains a cycle.
    fn topological_sort(&self) -> Result<Vec<String>, String> {
        let mut in_degree: HashMap<String, usize> = HashMap::new();
        for uri in &self.order {
            in_degree.entry(uri.clone()).or_insert(0);
        }
        for edges in self.edges.values() {
            for edge in edges {
                *in_degree.entry(edge.target.clone()).or_insert(0) = in_degree
                    .get(&edge.target)
                    .copied()
                    .unwrap_or(0)
                    .saturating_add(1);
            }
        }

        // Seed the queue with zero-in-degree vertices, keeping insertion order
        // for a deterministic result.
        let mut queue: Vec<String> = self
            .order
            .iter()
            .filter(|uri| in_degree.get(*uri).copied().unwrap_or(0) == 0)
            .cloned()
            .collect();

        let mut sorted: Vec<String> = Vec::with_capacity(self.order.len());
        let mut head = 0usize;
        while let Some(node) = queue.get(head).cloned() {
            head = head.saturating_add(1);
            sorted.push(node.clone());
            if let Some(out) = self.edges.get(&node) {
                for edge in out {
                    let degree = in_degree.get(&edge.target).copied().unwrap_or(0);
                    let degree = degree.saturating_sub(1);
                    in_degree.insert(edge.target.clone(), degree);
                    if degree == 0 {
                        queue.push(edge.target.clone());
                    }
                }
            }
        }

        if sorted.len() != self.order.len() {
            return Err("task: Taskfile graph contains a cycle".to_string());
        }
        Ok(sorted)
    }

    /// Maps every vertex to the incoming edges that point at it.
    fn predecessor_map(&self) -> HashMap<String, Vec<Edge>> {
        let mut map: HashMap<String, Vec<Edge>> = HashMap::new();
        for uri in &self.order {
            map.entry(uri.clone()).or_default();
        }
        for edges in self.edges.values() {
            for edge in edges {
                map.entry(edge.target.clone())
                    .or_default()
                    .push(edge.clone());
            }
        }
        map
    }

    /// Merges every included Taskfile into its parent following reverse
    /// topological order, then returns the fully merged root Taskfile.
    pub fn merge(&mut self) -> Result<Taskfile, String> {
        let hashes = self.topological_sort()?;
        let predecessor_map = self.predecessor_map();

        // Walk every non-root vertex in reverse topological order, which is a
        // safe order to fold each included Taskfile into the ones that import
        // it.
        let mut i = hashes.len();
        while i > 1 {
            i = i.saturating_sub(1);
            let Some(hash) = hashes.get(i) else { continue };

            let included_taskfile = self
                .vertex(hash)
                .ok_or_else(|| format!("task: vertex {hash} not found"))?
                .taskfile
                .clone();

            let predecessors = predecessor_map.get(hash).cloned().unwrap_or_default();
            for edge in predecessors {
                let base_uri = edge.source.clone();
                let mut base = self
                    .vertex(&base_uri)
                    .ok_or_else(|| format!("task: vertex {base_uri} not found"))?
                    .taskfile
                    .clone();
                for include in &edge.includes {
                    base.merge(&included_taskfile, include)?;
                }
                if let Some(vertex) = self.vertices.get_mut(&base_uri) {
                    vertex.taskfile = base;
                }
            }
        }

        let root_hash = hashes
            .first()
            .ok_or_else(|| "task: empty Taskfile graph".to_string())?;
        Ok(self
            .vertex(root_hash)
            .ok_or_else(|| format!("task: vertex {root_hash} not found"))?
            .taskfile
            .clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tf() -> Taskfile {
        Taskfile::default()
    }

    #[test]
    fn topological_order_root_first() {
        let mut g = TaskfileGraph::new();
        g.add_vertex(TaskfileVertex {
            uri: "root".to_string(),
            taskfile: tf(),
        });
        g.add_vertex(TaskfileVertex {
            uri: "child".to_string(),
            taskfile: tf(),
        });
        g.add_edge("root", "child", Vec::new()).unwrap();
        let sorted = g.topological_sort().unwrap();
        assert_eq!(sorted, vec!["root".to_string(), "child".to_string()]);
    }

    #[test]
    fn cycle_is_rejected() {
        let mut g = TaskfileGraph::new();
        g.add_vertex(TaskfileVertex {
            uri: "a".to_string(),
            taskfile: tf(),
        });
        g.add_vertex(TaskfileVertex {
            uri: "b".to_string(),
            taskfile: tf(),
        });
        g.add_edge("a", "b", Vec::new()).unwrap();
        let err = g.add_edge("b", "a", Vec::new()).unwrap_err();
        assert!(err.contains("cycle"));
    }

    #[test]
    fn merge_single_vertex_returns_root() {
        let mut g = TaskfileGraph::new();
        g.add_vertex(TaskfileVertex {
            uri: "root".to_string(),
            taskfile: tf(),
        });
        let merged = g.merge().unwrap();
        assert!(merged.tasks.is_empty());
    }
}
