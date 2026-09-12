//! The Dependency Graph: one edge table for code and memory (PRD §data_model).
//!
//! The PRD is emphatic that the code call graph and the memory dependency graph
//! share a table, so that "code-structural facts and general agent memory share
//! one dependency graph and one eviction policy instead of being two
//! disconnected subsystems". Concretely, that means the eviction engine can walk
//! `episode --derived_from--> symbolic_fact --calls--> symbolic_fact` in one
//! traversal, and answer "is anything still depending on this tool output?" the
//! same way it answers "who calls this function?".
//!
//! Traversal is implemented here as a BFS over an in-memory adjacency map loaded
//! per query. For the documented scale ceiling (tens of thousands of edges) that
//! is both faster and simpler than recursive SQL, and it makes the depth cap and
//! cycle handling explicit rather than emergent.

use std::collections::{HashMap, HashSet, VecDeque};

use crate::error::{Error, Result};
use crate::ids::now_rfc3339;

/// What kind of node an id refers to. Kept as a plain string on the row so the
/// table does not need a schema change when a new memory type appears, but
/// parsed into this enum at the API boundary.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    serde::Serialize,
    serde::Deserialize,
    schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum NodeKind {
    Episode,
    SymbolicFact,
    SemanticEntry,
    Anchor,
    Fold,
    RepoFile,
    Project,
}

impl NodeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            NodeKind::Episode => "episode",
            NodeKind::SymbolicFact => "symbolic_fact",
            NodeKind::SemanticEntry => "semantic_entry",
            NodeKind::Anchor => "anchor",
            NodeKind::Fold => "fold",
            NodeKind::RepoFile => "repo_file",
            NodeKind::Project => "project",
        }
    }

    pub fn parse(s: &str) -> Result<Self> {
        Ok(match s {
            "episode" => NodeKind::Episode,
            "symbolic_fact" => NodeKind::SymbolicFact,
            "semantic_entry" => NodeKind::SemanticEntry,
            "anchor" => NodeKind::Anchor,
            "fold" => NodeKind::Fold,
            "repo_file" => NodeKind::RepoFile,
            "project" => NodeKind::Project,
            other => return Err(Error::Invalid(format!("unknown node kind: {other}"))),
        })
    }
}

/// A typed node reference.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize)]
pub struct NodeRef {
    pub kind: NodeKind,
    pub id: String,
}

impl NodeRef {
    pub fn new(kind: NodeKind, id: impl Into<String>) -> Self {
        Self { kind, id: id.into() }
    }

    pub fn episode(id: impl Into<String>) -> Self {
        Self::new(NodeKind::Episode, id)
    }

    pub fn fact(id: impl Into<String>) -> Self {
        Self::new(NodeKind::SymbolicFact, id)
    }

    pub fn atlas(id: impl Into<String>) -> Self {
        Self::new(NodeKind::SemanticEntry, id)
    }

    pub fn file(path: impl Into<String>) -> Self {
        Self::new(NodeKind::RepoFile, path)
    }

    pub fn key(&self) -> String {
        format!("{}:{}", self.kind.as_str(), self.id)
    }
}

/// The edge vocabulary, unified across code and memory.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    serde::Serialize,
    serde::Deserialize,
    schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum EdgeKind {
    /// A symbols-level call.
    Calls,
    /// A module-level import.
    Imports,
    /// A memory item was derived from another (the anchor relation).
    DerivedFrom,
    /// A generic dependency used for eviction ordering.
    DependsOn,
    /// One episode supersedes another (a re-read of a file, a corrected result).
    Supersedes,
    /// One episode corrects another.
    Corrects,
    /// An episode or fact belongs to a fold.
    FoldedFrom,
}

impl EdgeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            EdgeKind::Calls => "calls",
            EdgeKind::Imports => "imports",
            EdgeKind::DerivedFrom => "derived_from",
            EdgeKind::DependsOn => "depends_on",
            EdgeKind::Supersedes => "supersedes",
            EdgeKind::Corrects => "corrects",
            EdgeKind::FoldedFrom => "folded_from",
        }
    }

    pub fn parse(s: &str) -> Result<Self> {
        Ok(match s {
            "calls" => EdgeKind::Calls,
            "imports" => EdgeKind::Imports,
            "derived_from" => EdgeKind::DerivedFrom,
            "depends_on" => EdgeKind::DependsOn,
            "supersedes" => EdgeKind::Supersedes,
            "corrects" => EdgeKind::Corrects,
            "folded_from" => EdgeKind::FoldedFrom,
            other => return Err(Error::Invalid(format!("unknown edge kind: {other}"))),
        })
    }

    /// Whether an edge of this kind makes the *destination* depend on the
    /// *source*, i.e. whether evicting the destination while the source is live
    /// would lose information.
    ///
    /// This is what the eviction engine consults before dropping anything. A
    /// `derived_from` edge means the source was the ground truth for the
    /// destination; a `calls` edge means the destination's behaviour depends on
    /// the source's signature.
    pub fn creates_dependency(self) -> bool {
        true
    }
}

/// One edge.
#[derive(Debug, Clone, serde::Serialize)]
pub struct EdgeRow {
    pub src_type: String,
    pub src_id: String,
    pub dst_type: String,
    pub dst_id: String,
    pub edge_kind: EdgeKind,
    pub weight: f64,
}

impl EdgeRow {
    pub fn new(src: &NodeRef, dst: &NodeRef, kind: EdgeKind) -> Self {
        Self {
            src_type: src.kind.as_str().to_string(),
            src_id: src.id.clone(),
            dst_type: dst.kind.as_str().to_string(),
            dst_id: dst.id.clone(),
            edge_kind: kind,
            weight: 1.0,
        }
    }

    pub fn with_weight(mut self, weight: f64) -> Self {
        self.weight = weight;
        self
    }

    /// The SQL to insert this edge idempotently.
    pub fn insert_sql() -> &'static str {
        "INSERT INTO dependency_graph_edge
             (src_type, src_id, dst_type, dst_id, edge_kind, weight, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT(src_type, src_id, dst_type, dst_id, edge_kind)
         DO UPDATE SET weight = excluded.weight"
    }

    pub fn params(&self) -> (String, String, String, String, String, f64, String) {
        (
            self.src_type.clone(),
            self.src_id.clone(),
            self.dst_type.clone(),
            self.dst_id.clone(),
            self.edge_kind.as_str().to_string(),
            self.weight,
            now_rfc3339(),
        )
    }
}

/// An in-memory, per-query view of the graph.
#[derive(Debug, Default, Clone)]
pub struct DependencyGraph {
    /// src key -> outgoing edges
    out: HashMap<String, Vec<(String, EdgeKind, NodeRef)>>,
    /// dst key -> incoming edges
    inc: HashMap<String, Vec<(String, EdgeKind, NodeRef)>>,
}

/// A node reached during traversal, with the hop count that reached it.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ReachedNode {
    pub kind: NodeKind,
    pub id: String,
    pub depth: usize,
    /// The edge kind traversed to reach this node from its parent.
    pub via: EdgeKind,
}

impl DependencyGraph {
    pub fn new() -> Self {
        Self::default()
    }

    /// Build from stored edges (typically all edges touching a working set).
    pub fn from_edges(edges: Vec<EdgeRow>) -> Self {
        let mut g = Self::default();
        for e in edges {
            let src_kind = NodeKind::parse(&e.src_type).unwrap_or(NodeKind::Episode);
            let dst_kind = NodeKind::parse(&e.dst_type).unwrap_or(NodeKind::Episode);
            let src = NodeRef::new(src_kind, e.src_id.clone());
            let dst = NodeRef::new(dst_kind, e.dst_id.clone());
            g.out.entry(src.key()).or_default().push((dst.key(), e.edge_kind, dst.clone()));
            g.inc.entry(dst.key()).or_default().push((src.key(), e.edge_kind, src.clone()));
        }
        g
    }

    pub fn add_edge(&mut self, src: &NodeRef, dst: &NodeRef, kind: EdgeKind) {
        self.out.entry(src.key()).or_default().push((dst.key(), kind, dst.clone()));
        self.inc.entry(dst.key()).or_default().push((src.key(), kind, src.clone()));
    }

    /// Nodes this node depends on (outgoing), transitively.
    ///
    /// `kinds` filters which edge kinds are followed; `None` follows all.
    pub fn dependents_of(
        &self,
        start: &NodeRef,
        max_depth: usize,
        kinds: Option<&[EdgeKind]>,
    ) -> Vec<ReachedNode> {
        self.walk(start, max_depth, kinds, Direction::Out)
    }

    /// Nodes that depend on this node (incoming), transitively — the
    /// blast-radius direction for `code.impact_of_change` (FR-11).
    pub fn dependents_on(
        &self,
        start: &NodeRef,
        max_depth: usize,
        kinds: Option<&[EdgeKind]>,
    ) -> Vec<ReachedNode> {
        self.walk(start, max_depth, kinds, Direction::In)
    }

    /// True when anything else in the graph depends on `node`.
    ///
    /// The eviction engine calls this before permitting a `Drop` tier: FR-5
    /// forbids dropping "anything with unresolved dependents".
    pub fn has_dependents(&self, node: &NodeRef) -> bool {
        self.inc.get(&node.key()).map(|v| !v.is_empty()).unwrap_or(false)
    }

    /// Edges leaving `node`, for display.
    pub fn out_edges(&self, node: &NodeRef) -> &[(String, EdgeKind, NodeRef)] {
        self.out.get(&node.key()).map(|v| v.as_slice()).unwrap_or(&[])
    }

    /// Number of stored edges.
    pub fn len(&self) -> usize {
        self.out.values().map(|v| v.len()).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn walk(
        &self,
        start: &NodeRef,
        max_depth: usize,
        kinds: Option<&[EdgeKind]>,
        dir: Direction,
    ) -> Vec<ReachedNode> {
        let mut seen: HashSet<String> = HashSet::new();
        let mut out = Vec::new();
        let mut queue: VecDeque<(NodeRef, usize, EdgeKind)> = VecDeque::new();
        seen.insert(start.key());

        let first = match dir {
            Direction::Out => &self.out,
            Direction::In => &self.inc,
        };
        if let Some(neighbors) = first.get(&start.key()) {
            for (key, kind, node) in neighbors {
                if !kinds.map(|k| k.contains(kind)).unwrap_or(true) {
                    continue;
                }
                if seen.insert(key.clone()) {
                    queue.push_back((node.clone(), 1, *kind));
                }
            }
        }

        while let Some((node, depth, via)) = queue.pop_front() {
            out.push(ReachedNode { kind: node.kind, id: node.id.clone(), depth, via });
            if depth >= max_depth {
                continue;
            }
            let next = match dir {
                Direction::Out => &self.out,
                Direction::In => &self.inc,
            };
            if let Some(neighbors) = next.get(&node.key()) {
                for (key, kind, n) in neighbors {
                    if !kinds.map(|k| k.contains(kind)).unwrap_or(true) {
                        continue;
                    }
                    // `seen` both caps cycles and guarantees each node is
                    // reported once at its shortest depth.
                    if seen.insert(key.clone()) {
                        queue.push_back((n.clone(), depth + 1, *kind));
                    }
                }
            }
        }
        out
    }
}

#[derive(Debug, Clone, Copy)]
enum Direction {
    Out,
    In,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn graph() -> DependencyGraph {
        // a -> b -> c, a -> c, and x -> a (i.e. x depends on a).
        let mut g = DependencyGraph::new();
        let a = NodeRef::fact("a");
        let b = NodeRef::fact("b");
        let c = NodeRef::fact("c");
        let x = NodeRef::episode("x");
        g.add_edge(&a, &b, EdgeKind::Calls);
        g.add_edge(&b, &c, EdgeKind::Calls);
        g.add_edge(&a, &c, EdgeKind::Calls);
        g.add_edge(&x, &a, EdgeKind::DerivedFrom);
        g
    }

    #[test]
    fn transitive_outgoing_walk_reports_shortest_depth() {
        let g = graph();
        let reached = g.dependents_of(&NodeRef::fact("a"), 5, None);
        let b = reached.iter().find(|r| r.id == "b").unwrap();
        let c = reached.iter().find(|r| r.id == "c").unwrap();
        assert_eq!(b.depth, 1);
        assert_eq!(c.depth, 1, "a->c is a direct edge, so depth 1 wins over a->b->c");
    }

    #[test]
    fn depth_is_capped() {
        let g = graph();
        let reached = g.dependents_of(&NodeRef::fact("a"), 1, None);
        assert!(reached.iter().all(|r| r.depth <= 1));
        assert!(reached.iter().any(|r| r.id == "b"));
    }

    #[test]
    fn incoming_walk_is_the_blast_radius_direction() {
        let g = graph();
        let impact = g.dependents_on(&NodeRef::fact("c"), 5, None);
        let ids: Vec<&str> = impact.iter().map(|r| r.id.as_str()).collect();
        assert!(ids.contains(&"b"));
        assert!(ids.contains(&"a"));
        assert!(ids.contains(&"x"), "x reached via a->c? no: via a. depth 2");
    }

    #[test]
    fn edge_kind_filter_is_respected() {
        let g = graph();
        let only_calls = g.dependents_of(&NodeRef::fact("a"), 5, Some(&[EdgeKind::Calls]));
        assert!(only_calls.iter().all(|r| r.via == EdgeKind::Calls));
        let only_imports = g.dependents_of(&NodeRef::fact("a"), 5, Some(&[EdgeKind::Imports]));
        assert!(only_imports.is_empty());
    }

    #[test]
    fn cycles_do_not_loop_forever() {
        let mut g = DependencyGraph::new();
        let a = NodeRef::fact("a");
        let b = NodeRef::fact("b");
        g.add_edge(&a, &b, EdgeKind::Calls);
        g.add_edge(&b, &a, EdgeKind::Calls);
        let reached = g.dependents_of(&a, 100, None);
        assert_eq!(reached.len(), 1);
        assert_eq!(reached[0].id, "b");
    }

    #[test]
    fn has_dependents_drives_the_drop_gate() {
        let g = graph();
        assert!(g.has_dependents(&NodeRef::fact("a")));
        assert!(g.has_dependents(&NodeRef::fact("c")));
        assert!(!g.has_dependents(&NodeRef::episode("x")));
    }

    #[test]
    fn unknown_nodes_traverse_to_nothing_rather_than_failing() {
        let g = graph();
        assert!(g.dependents_of(&NodeRef::fact("nope"), 3, None).is_empty());
        assert!(!g.has_dependents(&NodeRef::fact("nope")));
    }

    #[test]
    fn node_and_edge_kinds_round_trip() {
        for k in [
            NodeKind::Episode,
            NodeKind::SymbolicFact,
            NodeKind::SemanticEntry,
            NodeKind::Anchor,
            NodeKind::Fold,
            NodeKind::RepoFile,
            NodeKind::Project,
        ] {
            assert_eq!(NodeKind::parse(k.as_str()).unwrap(), k);
        }
        for k in [
            EdgeKind::Calls,
            EdgeKind::Imports,
            EdgeKind::DerivedFrom,
            EdgeKind::DependsOn,
            EdgeKind::Supersedes,
            EdgeKind::Corrects,
            EdgeKind::FoldedFrom,
        ] {
            assert_eq!(EdgeKind::parse(k.as_str()).unwrap(), k);
        }
        assert!(NodeKind::parse("bogus").is_err());
        assert!(EdgeKind::parse("bogus").is_err());
    }
}
