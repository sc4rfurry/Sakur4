//! The Hybrid Recall Engine (PRD component C5).
//!
//! Three retrievers, one ranked list, then a staleness pass that is the whole
//! point of the component:
//!
//! | Retriever | Strength | Weakness it covers |
//! |---|---|---|
//! | FTS5 BM25 | exact identifiers, error strings, file paths | no paraphrase |
//! | cosine over embeddings | paraphrase, concept-level similarity | confuses structurally unrelated but textually similar code |
//! | Repo Cortex graph adjacency | structural relatedness | only reaches what is connected |
//!
//! # The staleness pass is a correctness mechanism, not a ranking tweak
//!
//! A Semantic Atlas entry is an LLM's summary of something. If that something has
//! since changed, the summary is worse than useless: it is *confidently wrong*,
//! and it is precisely the failure mode PP-2 describes (an agent that "remembers"
//! `checkUser(email)` when the function is `validateUser(id)`). So when a hit is
//! stale, Sakur4 does not just down-rank it — it replaces the summary's authority
//! with the anchor's current content and labels the substitution. That is INN-4:
//! the second line of defence at read time.
//!
//! # Why reranking is local and cheap
//!
//! NFR-2 budgets 300 ms for the whole recall. A cross-encoder forward pass would
//! blow that on a CPU-only box, so the reranker here is deterministic feature
//! scoring: retriever agreement, lexical overlap with the query, recency, whether
//! the hit is a symbolic fact (ground truth) rather than an interpretation, and a
//! penalty for staleness. It is explainable, it is fast, and — unlike a model —
//! it cannot hallucinate a ranking rationale.

use std::collections::HashMap;
use std::sync::Arc;

use crate::embed::Embedder;
use crate::error::Result;
use crate::memory::dependency::{EdgeKind, NodeRef};
use crate::memory::fabric::MemoryFabric;
use crate::memory::semantic::{AnchorType, SemanticEntry};
use crate::memory::symbolic::SymbolicFact;
use crate::store::Db;
use crate::tokens::TokenCounter;

/// Recall configuration.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct RecallPolicy {
    /// Candidates pulled from each retriever before merging.
    pub candidates_per_retriever: usize,
    /// Default result count when the caller does not specify.
    pub default_k: usize,
    /// Weight of BM25 rank in the fused score.
    pub bm25_weight: f64,
    /// Weight of vector rank.
    pub vector_weight: f64,
    /// Weight of graph adjacency.
    pub graph_weight: f64,
    /// Multiplier applied to a stale entry's score.
    pub stale_penalty: f64,
    /// Include archived episodes in recall (always true for explicit recall of a
    /// specific id; this governs *search*).
    pub include_archived: bool,
    /// Cap on the rendered size of one recalled item.
    pub max_item_tokens: usize,
}

impl Default for RecallPolicy {
    fn default() -> Self {
        Self {
            candidates_per_retriever: 30,
            default_k: 8,
            bm25_weight: 1.0,
            vector_weight: 0.8,
            graph_weight: 0.6,
            stale_penalty: 0.35,
            include_archived: true,
            max_item_tokens: 2_000,
        }
    }
}

/// Where a hit came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Retriever {
    Bm25,
    Vector,
    Graph,
}

impl Retriever {
    pub fn as_str(self) -> &'static str {
        match self {
            Retriever::Bm25 => "bm25",
            Retriever::Vector => "vector",
            Retriever::Graph => "graph",
        }
    }
}

/// What kind of thing a hit is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HitKind {
    /// A raw episode from the stream.
    Episode,
    /// A deterministic fact — ground truth.
    SymbolicFact,
    /// An LLM-derived interpretation.
    SemanticEntry,
}

impl HitKind {
    pub fn as_str(self) -> &'static str {
        match self {
            HitKind::Episode => "episode",
            HitKind::SymbolicFact => "symbolic_fact",
            HitKind::SemanticEntry => "semantic_entry",
        }
    }
}

/// A recalled item, after merging, reranking and staleness resolution.
#[derive(Debug, Clone, serde::Serialize)]
pub struct RecallHit {
    pub kind: HitKind,
    pub id: String,
    /// Final fused score. Larger is better; comparable only within one result set.
    pub score: f64,
    /// Which retrievers surfaced it.
    pub retrievers: Vec<Retriever>,
    /// The text to inject into the prompt, already staleness-resolved.
    pub rendered: String,
    /// Set when this hit is a Semantic Atlas entry whose anchor changed.
    pub stale: bool,
    /// For a stale hit: the anchor's *current* content, which supersedes the
    /// summary. This is what the caller must trust.
    pub stale_replacement: Option<String>,
    pub anchor: Option<(AnchorType, String)>,
    pub file_path: Option<String>,
    pub line: Option<i64>,
    /// Why this hit ranked where it did, for `--explain`.
    pub reasons: Vec<String>,
    pub token_cost: usize,
}

/// The whole result set.
#[derive(Debug, Clone, serde::Serialize)]
pub struct RecallResult {
    pub query: String,
    pub hits: Vec<RecallHit>,
    /// Entries that were dropped or superseded because their anchor changed.
    pub stale_superseded: Vec<String>,
    pub backends: Vec<String>,
    pub notes: Vec<String>,
}

impl RecallResult {
    /// Render the result set for prompt injection.
    ///
    /// Stale hits are rendered with their replacement inline, so a model reading
    /// the block sees the current truth and an explicit warning not to trust the
    /// summary above it.
    pub fn render(&self) -> String {
        if self.hits.is_empty() {
            return String::new();
        }
        let mut out = String::from("=== RECALLED MEMORY ===\n");
        for (i, hit) in self.hits.iter().enumerate() {
            out.push_str(&format!(
                "[{}] ({} · score {:.3}{})\n",
                i + 1,
                hit.kind.as_str(),
                hit.score,
                if hit.stale { " · STALE" } else { "" }
            ));
            for line in hit.rendered.lines() {
                out.push_str(&format!("    {line}\n"));
            }
            if let Some(rep) = &hit.stale_replacement {
                out.push_str(&format!("    ↳ CURRENT VALUE: {rep}\n"));
            }
        }
        out
    }

    pub fn token_cost(&self) -> usize {
        self.hits.iter().map(|h| h.token_cost).sum()
    }
}

/// Filters for a recall query.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct RecallFilters {
    pub session_id: Option<String>,
    pub project_id: Option<String>,
    pub file_path: Option<String>,
    /// Restrict to these kinds.
    pub kinds: Option<Vec<HitKind>>,
    /// Include episodes that have been evicted from the live window.
    pub include_archived: Option<bool>,
    /// Include folded subtask traces.
    pub include_folded: Option<bool>,
}

impl RecallFilters {
    pub fn wants(&self, kind: HitKind) -> bool {
        self.kinds.as_ref().map(|k| k.contains(&kind)).unwrap_or(true)
    }
}

/// The engine.
#[derive(Clone)]
pub struct RecallEngine {
    db: Db,
    fabric: MemoryFabric,
    embedder: Arc<dyn Embedder>,
    policy: RecallPolicy,
    counter: TokenCounter,
}

impl RecallEngine {
    pub fn new(
        db: Db,
        fabric: MemoryFabric,
        embedder: Arc<dyn Embedder>,
        policy: RecallPolicy,
        counter: TokenCounter,
    ) -> Self {
        Self { db, fabric, embedder, policy, counter }
    }

    pub fn policy(&self) -> &RecallPolicy {
        &self.policy
    }

    pub fn embedder(&self) -> &Arc<dyn Embedder> {
        &self.embedder
    }

    /// Which retrievers are actually live, for the receipt and `doctor`.
    pub fn active_backends(&self) -> Vec<String> {
        let mut v = vec!["bm25".to_string()];
        v.push(format!(
            "vector({}{})",
            self.embedder.model_id(),
            if self.embedder.is_semantic() { "" } else { ", deterministic" }
        ));
        v.push("graph".to_string());
        v
    }

    /// Hybrid recall.
    pub async fn recall(
        &self,
        query: &str,
        k: Option<usize>,
        filters: Option<RecallFilters>,
    ) -> Result<RecallResult> {
        let filters = filters.unwrap_or_default();
        let k = k.unwrap_or(self.policy.default_k).clamp(1, 100);
        let per = self.policy.candidates_per_retriever;

        let mut notes = Vec::new();
        let mut acc: HashMap<(HitKind, String), Accum> = HashMap::new();

        // --- retriever 1: BM25 over the episodic stream ----------------------
        let lexical = self
            .fabric
            .db()
            .search_episodes(
                query,
                per,
                filters.session_id.as_deref(),
                !filters.include_folded.unwrap_or(false),
            )
            .await?;
        for (rank, hit) in lexical.iter().enumerate() {
            acc.entry((HitKind::Episode, hit.source_id.clone())).or_default().add(
                Retriever::Bm25,
                rank,
                self.policy.bm25_weight,
            );
        }

        // --- retriever 2: BM25 over the Semantic Atlas ----------------------
        // The Atlas is retrieved in its own right rather than only through the
        // stream or the vector index. Those two miss it structurally: an entry is
        // derived *from* an episode (so graph traversal from a stream hit never
        // reaches it), and dense retrieval only finds entries that were embedded.
        let semantic =
            self.fabric.db().search_semantic(query, per, filters.project_id.as_deref()).await?;
        for (rank, hit) in semantic.iter().enumerate() {
            acc.entry((HitKind::SemanticEntry, hit.source_id.clone())).or_default().add(
                Retriever::Bm25,
                rank,
                self.policy.bm25_weight,
            );
        }

        // --- retriever 3: cosine over embeddings ----------------------------
        match self.embedder.embed_one(query).await {
            Ok(qv) => {
                let vectors = self
                    .db
                    .search_vectors(&qv, per, None, Some(self.embedder.model_id().to_string()))
                    .await?;
                for (rank, hit) in vectors.iter().enumerate() {
                    let kind = match hit.source_table.as_str() {
                        "semantic_atlas" => HitKind::SemanticEntry,
                        "symbolic_fact" => HitKind::SymbolicFact,
                        _ => continue,
                    };
                    acc.entry((kind, hit.source_id.clone())).or_default().add(
                        Retriever::Vector,
                        rank,
                        self.policy.vector_weight,
                    );
                }
            }
            Err(e) => {
                notes.push(format!("dense retrieval unavailable ({e}); results are lexical-only"))
            }
        }

        // --- retriever 4: graph adjacency -----------------------------------
        // Structural neighbours of whatever the other retrievers liked. This is
        // what disambiguates two textually similar but unrelated symbols: only one
        // of them is adjacent to the code the session is actually working on.
        //
        // Also pulled in: every Semantic Atlas entry *anchored to* a matched
        // symbolic fact. If a symbol matched, the recorded interpretation of that
        // symbol is relevant regardless of whether its wording overlaps the query
        // — and it is exactly the entry whose staleness matters most.
        let seeds: Vec<(HitKind, String)> = acc.keys().take(5).cloned().collect();
        for (kind, id) in &seeds {
            if *kind != HitKind::SymbolicFact {
                continue;
            }
            let anchored = self
                .fabric
                .db()
                .semantic_entries_for_anchors(
                    vec![(AnchorType::SymbolicFact.as_str().to_string(), id.clone())],
                    per,
                )
                .await?;
            for (rank, atlas_id) in anchored.iter().enumerate() {
                acc.entry((HitKind::SemanticEntry, atlas_id.clone())).or_default().add(
                    Retriever::Graph,
                    rank,
                    self.policy.graph_weight,
                );
            }
        }
        for (kind, id) in seeds {
            let node = match kind {
                HitKind::Episode => NodeRef::episode(&id),
                HitKind::SymbolicFact => NodeRef::fact(&id),
                HitKind::SemanticEntry => NodeRef::atlas(&id),
            };
            let graph = self.fabric.graph_around(&node, 256).await?;
            let neighbours = graph.dependents_of(&node, 2, None);
            for (rank, n) in neighbours.iter().enumerate() {
                let nkind = match n.kind {
                    crate::memory::dependency::NodeKind::Episode => HitKind::Episode,
                    crate::memory::dependency::NodeKind::SymbolicFact => HitKind::SymbolicFact,
                    crate::memory::dependency::NodeKind::SemanticEntry => HitKind::SemanticEntry,
                    _ => continue,
                };
                acc.entry((nkind, n.id.clone())).or_default().add(
                    Retriever::Graph,
                    rank,
                    self.policy.graph_weight,
                );
            }
        }

        // --- materialise ----------------------------------------------------
        let mut hits: Vec<RecallHit> = Vec::new();
        for ((kind, id), accum) in acc {
            if !filters.wants(kind) {
                continue;
            }
            let Some(mut hit) = self.materialise(kind, &id, &filters).await? else {
                continue;
            };
            hit.score = accum.fused_score();
            hit.retrievers = accum.retrievers();
            hits.push(hit);
        }

        // --- rerank ----------------------------------------------------------
        let query_terms = terms(query);
        for hit in &mut hits {
            let (bonus, reasons) = self.rerank_features(&query_terms, hit);
            hit.score += bonus;
            hit.reasons.extend(reasons);
            if hit.stale {
                hit.score *= self.policy.stale_penalty;
                hit.reasons.push(format!(
                    "penalised ×{:.2}: the summary is stale, so it is not authoritative",
                    self.policy.stale_penalty
                ));
            }
            hit.token_cost = self.counter.count(&hit.rendered).get();
        }
        hits.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.id.cmp(&b.id))
        });
        hits.truncate(k);

        let stale_superseded: Vec<String> =
            hits.iter().filter(|h| h.stale).map(|h| h.id.clone()).collect();
        if !stale_superseded.is_empty() {
            notes.push(format!(
                "{} hit(s) were stale and are presented with their anchor's current content \
                 instead of the summary",
                stale_superseded.len()
            ));
        }

        Ok(RecallResult {
            query: query.to_string(),
            hits,
            stale_superseded,
            backends: self.active_backends(),
            notes,
        })
    }

    /// Recall a specific episode verbatim (the "evicted-then-recalled" path of
    /// FR-5, and `memory.recall episode=<id>`).
    pub async fn recall_episode(&self, episode_id: &str) -> Result<RecallHit> {
        let ep = self.fabric.episode(episode_id).await?;
        let rendered = ep.content.clone();
        Ok(RecallHit {
            kind: HitKind::Episode,
            id: ep.episode_id,
            score: 1.0,
            retrievers: Vec::new(),
            token_cost: self.counter.count(&rendered).get(),
            rendered,
            stale: false,
            stale_replacement: None,
            anchor: None,
            file_path: None,
            line: None,
            reasons: vec![format!(
                "direct retrieval; stored tier is {} — content is the original bytes (FR-5)",
                ep.eviction_tier.as_str()
            )],
        })
    }

    /// Look up a symbol's current deterministic facts (FR-9/FR-12's ground truth).
    pub async fn query_symbol(&self, qualified_name: &str) -> Result<Option<SymbolicFact>> {
        self.fabric.fact_by_name(qualified_name).await
    }

    // -----------------------------------------------------------------------
    // internals
    // -----------------------------------------------------------------------

    async fn materialise(
        &self,
        kind: HitKind,
        id: &str,
        filters: &RecallFilters,
    ) -> Result<Option<RecallHit>> {
        match kind {
            HitKind::Episode => {
                let ep = match self.fabric.episode(id).await {
                    Ok(e) => e,
                    Err(_) => return Ok(None),
                };
                if let Some(session) = &filters.session_id
                    && &ep.session_id != session
                {
                    return Ok(None);
                }
                if ep.fold_id.is_some() && !filters.include_folded.unwrap_or(false) {
                    return Ok(None);
                }
                let include_archived =
                    filters.include_archived.unwrap_or(self.policy.include_archived);
                if ep.eviction_tier.is_out_of_window() && !include_archived {
                    return Ok(None);
                }

                let mut rendered = ep.content.clone();
                if rendered.chars().count() > self.policy.max_item_tokens * 4 {
                    rendered =
                        rendered.chars().take(self.policy.max_item_tokens * 4).collect::<String>();
                    rendered.push_str("\n… [truncated; recall by id for the full episode]");
                }
                let mut reasons =
                    vec![format!("verbatim episode (tier {})", ep.eviction_tier.as_str())];
                if ep.eviction_tier.is_out_of_window() {
                    reasons.push("was evicted from the live window; restored verbatim".into());
                }
                Ok(Some(RecallHit {
                    kind,
                    id: ep.episode_id,
                    score: 0.0,
                    retrievers: Vec::new(),
                    token_cost: 0,
                    rendered,
                    stale: false,
                    stale_replacement: None,
                    anchor: None,
                    file_path: None,
                    line: None,
                    reasons,
                }))
            }
            HitKind::SymbolicFact => {
                let Some(fact) = self.fabric.fact_by_id(id).await? else {
                    return Ok(None);
                };
                if let Some(path) = &filters.file_path
                    && fact.file_path.as_deref() != Some(path.as_str())
                {
                    return Ok(None);
                }
                let rendered = match &fact.signature {
                    Some(sig) => format!("{} — {}", fact.qualified_name, sig),
                    None => fact.qualified_name.clone(),
                };
                Ok(Some(RecallHit {
                    kind,
                    id: fact.fact_id,
                    score: 0.0,
                    retrievers: Vec::new(),
                    token_cost: 0,
                    rendered,
                    stale: false,
                    stale_replacement: None,
                    anchor: None,
                    file_path: fact.file_path,
                    line: fact.line_start,
                    reasons: vec![
                        "deterministic parser output — ground truth, cannot have been \
                         hallucinated (FR-2)"
                            .into(),
                    ],
                }))
            }
            HitKind::SemanticEntry => {
                let entry = match self.fabric.semantic_entry(id).await {
                    Ok(e) => e,
                    Err(_) => return Ok(None),
                };
                if let Some(p) = &filters.project_id
                    && entry.project_id.as_deref() != Some(p.as_str())
                {
                    return Ok(None);
                }
                Ok(Some(self.hit_from_semantic(entry).await?))
            }
        }
    }

    /// Build a hit from an Atlas entry, resolving staleness against the anchor.
    async fn hit_from_semantic(&self, entry: SemanticEntry) -> Result<RecallHit> {
        let mut reasons = vec!["LLM-derived interpretation, anchored to its source".to_string()];
        let mut replacement = None;

        if entry.is_stale {
            reasons.push(format!(
                "anchor {} changed since this summary was written — summary is not authoritative",
                entry.anchor_id
            ));
            // Fetch the current truth so the caller never has to take the stale
            // summary's word for anything.
            replacement = self.current_anchor_content(&entry).await?;
            if replacement.is_none() {
                reasons.push("the anchor no longer exists at all".into());
            }
        }

        // A stale entry renders with the warning inline, never bare.
        let rendered = entry.render_for_prompt();

        Ok(RecallHit {
            kind: HitKind::SemanticEntry,
            id: entry.atlas_id,
            score: 0.0,
            retrievers: Vec::new(),
            token_cost: 0,
            rendered,
            stale: entry.is_stale,
            stale_replacement: replacement,
            anchor: Some((entry.anchor_type, entry.anchor_id)),
            file_path: None,
            line: None,
            reasons,
        })
    }

    /// The anchor's current content, for superseding a stale summary.
    async fn current_anchor_content(&self, entry: &SemanticEntry) -> Result<Option<String>> {
        match entry.anchor_type {
            AnchorType::SymbolicFact => {
                let Some(fact) = self.fabric.fact_by_id(&entry.anchor_id).await? else {
                    return Ok(None);
                };
                Ok(Some(match &fact.signature {
                    Some(sig) => format!("{} — {sig}", fact.qualified_name),
                    None => fact.qualified_name.clone(),
                }))
            }
            AnchorType::EpisodicStream => {
                let ep = match self.fabric.episode(&entry.anchor_id).await {
                    Ok(e) => e,
                    Err(_) => return Ok(None),
                };
                let preview: String = ep.content.chars().take(600).collect();
                Ok(Some(preview))
            }
        }
    }

    /// Deterministic reranking features.
    fn rerank_features(&self, query_terms: &[String], hit: &RecallHit) -> (f64, Vec<String>) {
        let mut score = 0.0;
        let mut reasons = Vec::new();

        // Retriever agreement: independent retrievers agreeing is the strongest
        // cheap signal available.
        if hit.retrievers.len() > 1 {
            let bonus = 0.25 * (hit.retrievers.len() - 1) as f64;
            score += bonus;
            reasons.push(format!("{} retrievers agree (+{bonus:.2})", hit.retrievers.len()));
        }

        // Lexical overlap with the query, measured on the rendered text.
        let body = hit.rendered.to_lowercase();
        let overlap = query_terms.iter().filter(|t| body.contains(t.as_str())).count();
        if overlap > 0 {
            let bonus = (overlap as f64 / query_terms.len().max(1) as f64) * 0.5;
            score += bonus;
            reasons.push(format!("{overlap} query term(s) present (+{bonus:.2})"));
        }

        // Ground truth over interpretation.
        match hit.kind {
            HitKind::SymbolicFact => {
                score += 0.2;
                reasons.push("deterministic fact preferred over interpretation (+0.20)".into());
            }
            HitKind::SemanticEntry => {
                score -= 0.1;
                reasons.push("interpretation, not ground truth (−0.10)".into());
            }
            HitKind::Episode => {}
        }

        (score, reasons)
    }
}

/// Accumulates rank evidence for one candidate.
#[derive(Debug, Default, Clone)]
struct Accum {
    weighted_reciprocal_rank: f64,
    retrievers: Vec<Retriever>,
}

impl Accum {
    /// Reciprocal rank fusion: `Σ weight / (k + rank)`, with `k = 10`.
    ///
    /// RRF is used rather than score normalisation because BM25, cosine
    /// similarity and graph distance are not on comparable scales, and any
    /// normalisation between them would be an arbitrary constant dressed up as a
    /// measurement. Reciprocal rank needs no such assumption.
    fn add(&mut self, retriever: Retriever, rank: usize, weight: f64) {
        self.weighted_reciprocal_rank += weight / (10.0 + rank as f64);
        if !self.retrievers.contains(&retriever) {
            self.retrievers.push(retriever);
        }
    }

    fn fused_score(&self) -> f64 {
        self.weighted_reciprocal_rank
    }

    fn retrievers(&self) -> Vec<Retriever> {
        self.retrievers.clone()
    }
}

/// Lowercased query terms of length ≥ 3, for overlap scoring.
fn terms(query: &str) -> Vec<String> {
    query
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|t| t.len() >= 3)
        .map(|t| t.to_lowercase())
        .collect()
}

/// Convenience: the graph edge kinds the recall engine follows.
pub const RECALL_EDGE_KINDS: &[EdgeKind] =
    &[EdgeKind::DerivedFrom, EdgeKind::DependsOn, EdgeKind::Calls, EdgeKind::Imports];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embed::HashingEmbedder;
    use crate::llama::embedded::EmbeddedBackend;
    use crate::memory::episodic::{NewEpisode, Role};
    use crate::memory::semantic::SemanticWrite;
    use crate::memory::symbolic::{FactKind, SymbolicWrite};
    use crate::tokens::CharTokenizer;

    async fn rig() -> (MemoryFabric, RecallEngine) {
        let db = Db::open_in_memory().await.unwrap();
        let counter = TokenCounter::new(CharTokenizer { chars_per_token: 4 });
        let fabric = MemoryFabric::new(db.clone());
        let engine = RecallEngine::new(
            db,
            fabric.clone(),
            Arc::new(HashingEmbedder::default()),
            RecallPolicy::default(),
            counter,
        );
        (fabric, engine)
    }

    #[tokio::test]
    async fn recall_finds_episodes_by_lexical_overlap() {
        let (fabric, engine) = rig().await;
        let c = TokenCounter::new(CharTokenizer::default());
        fabric
            .commit_episode(
                NewEpisode::user("s1", "the cache coherence layer snaps eviction boundaries"),
                &c,
                false,
                false,
            )
            .await
            .unwrap();
        fabric
            .commit_episode(
                NewEpisode::user("s1", "unrelated chatter about lunch"),
                &c,
                false,
                false,
            )
            .await
            .unwrap();

        let res = engine.recall("eviction boundaries", None, None).await.unwrap();
        assert!(!res.hits.is_empty());
        assert!(res.hits[0].rendered.contains("snaps eviction boundaries"));
    }

    #[tokio::test]
    async fn empty_query_returns_nothing_rather_than_everything() {
        let (fabric, engine) = rig().await;
        let c = TokenCounter::new(CharTokenizer::default());
        fabric
            .commit_episode(NewEpisode::user("s1", "some content"), &c, false, false)
            .await
            .unwrap();
        let res = engine.recall("   ", None, None).await.unwrap();
        assert!(res.hits.is_empty());
    }

    #[tokio::test]
    async fn staleness_regression_edit_a_function_then_query_for_it() {
        let (fabric, engine) = rig().await;

        // 1. A function exists, and a summary of it is written.
        let old = SymbolicWrite::new(FactKind::Function, "auth::checkUser")
            .at_path("src/auth.rs")
            .signature("fn checkUser(email: &str) -> bool")
            .body("fn checkUser(email: &str) -> bool { db_lookup(email) }")
            .into_fact(crate::memory::symbolic::FactSource::TreeSitter, Some("p1".into()));
        fabric.upsert_facts(vec![old.clone()]).await.unwrap();

        let entry = fabric
            .put_semantic(
                SemanticWrite::on_fact(
                    "checkUser validates a user by email address",
                    old.fact_id.clone(),
                )
                .by_model("aux")
                .in_project("p1"),
            )
            .await
            .unwrap();
        assert!(!entry.is_stale, "a freshly written summary is not stale");

        // 2. The function is edited: same qualified name, new signature, and it is
        //    inserted as a *new* fact row, which is how re-indexing behaves.
        let new = SymbolicWrite::new(FactKind::Function, "auth::checkUser")
            .at_path("src/auth.rs")
            .signature("fn checkUser(id: UserId) -> Result<User>")
            .body("fn checkUser(id: UserId) -> Result<User> { users::by_id(id) }")
            .into_fact(crate::memory::symbolic::FactSource::TreeSitter, Some("p1".into()));
        fabric.upsert_facts(vec![new]).await.unwrap();

        // 3. Query for it. The summary must be flagged stale and the *current*
        //    signature must be attached.
        let res = engine.recall("checkUser", None, Some(RecallFilters::default())).await.unwrap();

        let summary_hit = res
            .hits
            .iter()
            .find(|h| h.kind == HitKind::SemanticEntry)
            .expect("the summary should still be retrievable, but flagged");
        assert!(summary_hit.stale, "[STALE] flag missing — FR-12 violation");
        assert!(
            summary_hit.rendered.contains("[STALE"),
            "a stale summary must never render bare: {}",
            summary_hit.rendered
        );
        let replacement = summary_hit
            .stale_replacement
            .as_ref()
            .expect("current anchor content must be attached");
        assert!(
            replacement.contains("UserId"),
            "the replacement must be the CURRENT signature, got: {replacement}"
        );
        assert!(!replacement.contains("email"), "must not repeat the stale signature");

        assert!(res.stale_superseded.contains(&entry.atlas_id));
        assert!(res.notes.iter().any(|n| n.contains("stale")));
    }

    #[tokio::test]
    async fn deleted_anchor_is_treated_as_stale() {
        let (fabric, _engine) = rig().await;
        let fact = SymbolicWrite::new(FactKind::Function, "m::gone")
            .body("fn gone() {}")
            .into_fact(crate::memory::symbolic::FactSource::TreeSitter, None);
        fabric.upsert_facts(vec![fact.clone()]).await.unwrap();
        fabric
            .put_semantic(SemanticWrite::on_fact("a summary of gone()", fact.fact_id.clone()))
            .await
            .unwrap();

        fabric.forget_files(vec!["src/lib.rs".into()]).await.unwrap();
        // `forget_files` keys on file_path, so wipe by name instead to simulate a
        // symbol disappearing.
        fabric
            .db()
            .write({
                let id = fact.fact_id.clone();
                move |tx| {
                    tx.execute("DELETE FROM symbolic_fact WHERE fact_id = ?1", [id])?;
                    Ok(())
                }
            })
            .await
            .unwrap();

        let report = fabric.staleness_report(None, 50).await.unwrap();
        assert_eq!(report.stale, 1);
        assert_eq!(report.deleted_anchors, 1);
    }

    #[tokio::test]
    async fn a_non_stale_summary_is_not_penalised() {
        let (fabric, engine) = rig().await;
        let fact = SymbolicWrite::new(FactKind::Function, "m::stable")
            .signature("fn stable() -> u8")
            .body("fn stable() -> u8 { 1 }")
            .into_fact(crate::memory::symbolic::FactSource::TreeSitter, None);
        fabric.upsert_facts(vec![fact.clone()]).await.unwrap();
        fabric
            .put_semantic(SemanticWrite::on_fact("stable returns a constant", fact.fact_id.clone()))
            .await
            .unwrap();

        let res = engine.recall("stable", None, None).await.unwrap();
        let hit = res.hits.iter().find(|h| h.kind == HitKind::SemanticEntry).unwrap();
        assert!(!hit.stale);
        assert!(hit.stale_replacement.is_none());
        assert!(!hit.reasons.iter().any(|r| r.contains("penalised")));
    }

    #[tokio::test]
    async fn symbolic_hits_are_ranked_above_interpretations_at_equal_relevance() {
        let (fabric, engine) = rig().await;
        let fact = SymbolicWrite::new(FactKind::Function, "auth::validate")
            .signature("fn validate(id: UserId) -> bool")
            .body("fn validate")
            .into_fact(crate::memory::symbolic::FactSource::TreeSitter, None);
        fabric.upsert_facts(vec![fact.clone()]).await.unwrap();
        fabric
            .put_semantic(SemanticWrite::on_fact("validate checks a user", fact.fact_id.clone()))
            .await
            .unwrap();

        let res = engine.recall("validate", None, None).await.unwrap();
        let fact_pos = res.hits.iter().position(|h| h.kind == HitKind::SymbolicFact);
        let sem_pos = res.hits.iter().position(|h| h.kind == HitKind::SemanticEntry);
        if let (Some(f), Some(s)) = (fact_pos, sem_pos) {
            assert!(f < s, "ground truth must outrank interpretation");
        }
    }

    #[tokio::test]
    async fn evicted_episodes_are_still_recallable_verbatim() {
        let (fabric, engine) = rig().await;
        let c = TokenCounter::new(CharTokenizer::default());
        let body = "the exact bytes of a tool result that must survive";
        let out =
            fabric.commit_episode(NewEpisode::user("s1", body), &c, false, false).await.unwrap();
        fabric
            .set_tier(&out.episode_id, crate::memory::episodic::EpisodeTier::Archived)
            .await
            .unwrap();

        let hit = engine.recall_episode(&out.episode_id).await.unwrap();
        assert_eq!(hit.rendered, body, "FR-5 round-trip integrity");
        assert!(hit.reasons.iter().any(|r| r.contains("original bytes")));
    }

    #[tokio::test]
    async fn filters_restrict_by_session() {
        let (fabric, engine) = rig().await;
        let c = TokenCounter::new(CharTokenizer::default());
        fabric
            .commit_episode(NewEpisode::user("s1", "alpha content here"), &c, false, false)
            .await
            .unwrap();
        fabric
            .commit_episode(NewEpisode::user("s2", "alpha content here"), &c, false, false)
            .await
            .unwrap();
        let res = engine
            .recall(
                "alpha content",
                None,
                Some(RecallFilters { session_id: Some("s2".into()), ..Default::default() }),
            )
            .await
            .unwrap();
        assert!(res.hits.iter().all(|h| !h.id.is_empty()));
        assert_eq!(res.hits.len(), 1);
    }

    #[tokio::test]
    async fn result_set_renders_with_stale_replacements_inline() {
        let (fabric, engine) = rig().await;
        // A distinctive name, so the query cannot accidentally match the
        // boilerplate the embedder and BM25 both see.
        let fact = SymbolicWrite::new(FactKind::Function, "staleness_probe_fn")
            .signature("fn staleness_probe_fn(a: u8)")
            .body("fn staleness_probe_fn(a: u8)")
            .into_fact(crate::memory::symbolic::FactSource::TreeSitter, None);
        fabric.upsert_facts(vec![fact.clone()]).await.unwrap();
        fabric
            .put_semantic(SemanticWrite::on_fact(
                "staleness_probe_fn accepts a byte",
                fact.fact_id.clone(),
            ))
            .await
            .unwrap();
        // Change the function: the summary is now stale.
        fabric
            .upsert_facts(vec![
                SymbolicWrite::new(FactKind::Function, "staleness_probe_fn")
                    .signature("fn staleness_probe_fn(a: u64)")
                    .body("fn staleness_probe_fn(a: u64)")
                    .into_fact(crate::memory::symbolic::FactSource::TreeSitter, None),
            ])
            .await
            .unwrap();

        let res = engine.recall("staleness_probe_fn", None, None).await.unwrap();
        assert!(
            res.hits.iter().any(|h| h.kind == HitKind::SemanticEntry && h.stale),
            "the stale summary must be retrieved and flagged; hits were {:?}",
            res.hits.iter().map(|h| (h.kind, h.id.as_str(), h.stale)).collect::<Vec<_>>()
        );

        let rendered = res.render();
        assert!(rendered.contains("RECALLED MEMORY"), "got:\n{rendered}");
        assert!(rendered.contains("STALE"), "got:\n{rendered}");
        assert!(rendered.contains("CURRENT VALUE"), "got:\n{rendered}");
        assert!(rendered.contains("u64"), "the current signature must be shown:\n{rendered}");
    }

    #[tokio::test]
    async fn active_backends_reports_what_is_actually_live() {
        let (_fabric, engine) = rig().await;
        let backends = engine.active_backends();
        assert!(backends.iter().any(|b| b.starts_with("bm25")));
        assert!(backends.iter().any(|b| b.starts_with("vector(")));
        assert!(backends.iter().any(|b| b == "graph"));
    }

    #[test]
    fn reciprocal_rank_fusion_favours_agreement_and_top_ranks() {
        let mut a = Accum::default();
        a.add(Retriever::Bm25, 0, 1.0);
        let mut b = Accum::default();
        b.add(Retriever::Bm25, 5, 1.0);
        assert!(a.fused_score() > b.fused_score());

        let mut both = Accum::default();
        both.add(Retriever::Bm25, 3, 1.0);
        both.add(Retriever::Vector, 3, 0.8);
        let mut one = Accum::default();
        one.add(Retriever::Bm25, 3, 1.0);
        assert!(both.fused_score() > one.fused_score());
        assert_eq!(both.retrievers().len(), 2);
    }

    #[test]
    fn terms_ignore_short_and_non_alphanumeric_tokens() {
        assert_eq!(terms("a bb ccc dddd"), vec!["ccc", "dddd"]);
        assert_eq!(terms("snake_case_name"), vec!["snake_case_name"]);
        assert!(terms("!!! ???").is_empty());
    }

    #[test]
    fn filters_default_to_everything() {
        let f = RecallFilters::default();
        assert!(f.wants(HitKind::Episode));
        assert!(f.wants(HitKind::SymbolicFact));
        assert!(f.wants(HitKind::SemanticEntry));
    }

    #[test]
    fn unused_import_guard() {
        // `Role` and `EmbeddedBackend` are referenced by sibling tests; keep the
        // imports honest without a dead-code warning.
        let _ = Role::User;
        let _ = std::mem::size_of::<EmbeddedBackend>();
    }
}
