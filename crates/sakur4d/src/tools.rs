//! The MCP tool surface (PRD component C7).
//!
//! # Why the tools are what they are
//!
//! Every operation here is one the rest of Sakur4 already performs; the gateway
//! adds no behaviour of its own. That is deliberate and it is C7's entire point in
//! the PRD: a stable tool contract over internals that are expected to change.
//!
//! The symbolic/interpretive split is visible in the surface on purpose:
//!
//! * `code.*` returns deterministic parser output. It cannot be wrong the way a
//!   model is wrong.
//! * `memory.recall` returns stored memory, and every interpretive hit is
//!   labelled — a stale summary comes back with `[STALE]`, its anchor's current
//!   value attached, and a note saying which to trust.
//! * `context.plan_eviction` defaults to *planning only*. Letting an agent see the
//!   eviction decision before it is taken is the difference between a system that
//!   manages context and one that silently rewrites it.
//!
//! # Caching hints
//!
//! List responses carry `ttlMs`/`cacheScope` (SEP-2549). A harness caching the
//! tool catalog is doing the right thing, because the catalog does not vary by
//! configuration — so the TTL is long. The repo map's TTL is short, because a
//! repository moves.

use std::sync::Arc;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::model::{
    CacheScope, CallToolRequestParams, CallToolResponse, GetPromptRequestParams,
    GetPromptResponse, GetPromptResult, Implementation, ListPromptsResult, ListResourcesResult,
    ListToolsResult, PaginatedRequestParams, Prompt, PromptMessage, ProtocolVersion,
    ReadResourceRequestParams, ReadResourceResponse, ReadResourceResult, Resource,
    ResourceContents, ServerCapabilities, ServerInfo,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData, RoleServer, ServerHandler};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use sakur4_core::consolidate::{Consolidator, ConsolidatorConfig};
use sakur4_core::evict::Pressure;
use sakur4_core::memory::anchor::{AnchorKind, PinRequest};
use sakur4_core::memory::episodic::{NewEpisode, Role};
use sakur4_core::memory::semantic::{AnchorType, SemanticWrite};
use sakur4_core::prompt::PromptParts;
use sakur4_core::receipt::Receipt;
use sakur4_core::recall::RecallFilters;
use sakur4_core::{Engine, MCP_PROTOCOL_VERSION};

/// How long a client may cache the tool catalog.
const TOOL_CATALOG_TTL_MS: u64 = 300_000;
/// How long a client may cache the repo map.
const REPO_MAP_TTL_MS: u64 = 10_000;

// ===========================================================================
// Tool input/output types
// ===========================================================================

/// `memory.commit_episode` input.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct CommitEpisodeInput {
    /// `system`, `user`, `assistant`, `tool` or `internal`.
    pub role: String,
    /// The turn or tool result, verbatim.
    pub content: String,
    /// Set for tool results, so the symbolic extractor can pick a parser.
    #[serde(default)]
    pub tool_name: Option<String>,
    #[serde(default)]
    pub slot_id: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
}

/// `memory.commit_episode` output.
#[derive(Debug, Serialize, JsonSchema)]
pub struct CommitEpisodeOutput {
    pub episode_id: String,
    pub seq: i64,
    pub token_count: i64,
    pub symbolic_facts: usize,
    pub symbolic_summary: String,
    pub suggested_anchor: Option<SuggestedAnchor>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct SuggestedAnchor {
    pub kind: String,
    pub excerpt: String,
    pub rule: String,
    pub confidence: f32,
    pub hint: String,
}

/// `memory.pin` input.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct PinInput {
    pub content: String,
    #[serde(default = "default_anchor_kind")]
    pub kind: String,
    #[serde(default)]
    pub session_id: Option<String>,
}

fn default_anchor_kind() -> String {
    "task_contract".into()
}

/// `memory.pin` output.
#[derive(Debug, Serialize, JsonSchema)]
pub struct PinOutput {
    pub anchor_id: String,
    pub kind: String,
    pub token_cost: usize,
    pub note: String,
}

/// `memory.recall` input.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct RecallInput {
    pub query: String,
    #[serde(default)]
    pub k: Option<usize>,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub file_path: Option<String>,
    #[serde(default)]
    pub include_folded: bool,
}

/// `memory.recall` output.
#[derive(Debug, Serialize, JsonSchema)]
pub struct RecallOutput {
    pub query: String,
    pub results: Vec<RecallItem>,
    /// Ids of stale entries; their `current_value` supersedes the stored summary.
    pub stale_flags: Vec<String>,
    pub backends: Vec<String>,
    pub notes: Vec<String>,
    pub rendered: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct RecallItem {
    pub kind: String,
    pub id: String,
    pub score: f64,
    pub retrievers: Vec<String>,
    pub stale: bool,
    pub current_value: Option<String>,
    pub text: String,
    pub token_cost: usize,
    pub reasons: Vec<String>,
}

/// `memory.fold` input.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct FoldInput {
    pub description: String,
    pub goal: String,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub slot_id: Option<String>,
}

/// `memory.fold` output.
#[derive(Debug, Serialize, JsonSchema)]
pub struct FoldOutput {
    pub fold_id: String,
    pub checkpoint: Option<String>,
    pub token_position: i64,
    pub tokens_at_open: usize,
    pub cache_note: String,
    pub next_steps: String,
}

/// `memory.unfold` input.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct UnfoldInput {
    pub fold_id: String,
    pub result_summary: String,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub slot_id: Option<String>,
}

/// `memory.unfold` output.
#[derive(Debug, Serialize, JsonSchema)]
pub struct UnfoldOutput {
    pub fold_id: String,
    pub tokens_reclaimed: usize,
    pub episodes_folded: usize,
    pub rollback_performed: bool,
    pub rollback_detail: String,
    pub trace_retrievable: bool,
}

/// `memory.recall_fold` input.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct RecallFoldInput {
    pub fold_id: String,
}

/// `code.get_repo_map` input.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct RepoMapInput {
    pub token_budget: usize,
    #[serde(default)]
    pub focus_paths: Option<Vec<String>>,
}

/// `code.get_repo_map` output.
#[derive(Debug, Serialize, JsonSchema)]
pub struct RepoMapOutput {
    pub map: String,
    pub tokens_used: usize,
    pub token_budget: usize,
}

/// `code.query_symbol` input.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct QuerySymbolInput {
    pub qualified_name: String,
}

/// `code.query_symbol` output.
#[derive(Debug, Serialize, JsonSchema)]
pub struct QuerySymbolOutput {
    pub found: bool,
    pub qualified_name: String,
    pub kind: Option<String>,
    pub signature: Option<String>,
    pub file: Option<String>,
    pub line: Option<i64>,
    pub ast_hash: Option<String>,
    pub source: Option<String>,
    pub note: String,
}

/// `code.impact_of_change` input.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ImpactInput {
    pub qualified_name: String,
    #[serde(default)]
    pub depth: Option<usize>,
}

/// `code.impact_of_change` output.
#[derive(Debug, Serialize, JsonSchema)]
pub struct ImpactOutput {
    pub symbol: String,
    pub signature: Option<String>,
    pub ast_hash: String,
    pub callers: Vec<ImpactCaller>,
    pub rendered: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ImpactCaller {
    pub qualified_name: String,
    pub file: Option<String>,
    pub line: Option<i64>,
    pub depth: usize,
    pub via: String,
}

/// `session.snapshot` input.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct SnapshotInput {
    #[serde(default)]
    pub slot_id: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
}

/// `session.snapshot` output.
#[derive(Debug, Serialize, JsonSchema)]
pub struct SnapshotOutput {
    pub snapshot_id: String,
    pub file_path: Option<String>,
    pub size_bytes: Option<u64>,
    pub elapsed_ms: i64,
}

/// `session.restore` input.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct RestoreInput {
    /// Warm-restore a slot from a save file.
    pub path: String,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub slot_id: Option<String>,
}

/// `session.restore` output.
#[derive(Debug, Serialize, JsonSchema)]
pub struct RestoreOutput {
    pub restored: bool,
    pub restore_time_ms: i64,
    pub detail: String,
}

/// `context.receipt` input.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ReceiptInput {
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub assemble: bool,
}

/// `context.receipt` output.
#[derive(Debug, Serialize, JsonSchema)]
pub struct ReceiptOutput {
    pub breakdown: BreakdownView,
    pub total_tokens: usize,
    pub context_window: usize,
    pub cache_status: String,
    pub cache_detail: String,
    pub prompt_eval_ms: Option<i64>,
    pub tokens_reused: Option<usize>,
    pub tokens_prefilled: Option<usize>,
    pub eviction: Option<EvictionView>,
    pub rendered: String,
    pub stats: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct BreakdownView {
    pub system_prompt: usize,
    pub pinned_anchors: usize,
    pub retrieved_memory: usize,
    pub repo_map: usize,
    pub raw_recent_history: usize,
    pub tool_schemas: usize,
    pub fold_summaries: usize,
    pub other: usize,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct EvictionView {
    pub applied: usize,
    pub tokens_reclaimed: usize,
    pub snapshot_taken: bool,
    pub boundary: Option<String>,
}

/// `context.plan_eviction` input.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct PlanEvictionInput {
    pub session_id: String,
    #[serde(default)]
    pub slot_id: Option<String>,
    #[serde(default)]
    pub apply: bool,
    #[serde(default)]
    pub pending_recall: Option<String>,
}

/// `context.plan_eviction` output.
#[derive(Debug, Serialize, JsonSchema)]
pub struct PlanEvictionOutput {
    pub pressure: String,
    pub budget: usize,
    pub trigger_at: usize,
    pub target: usize,
    pub live_tokens: usize,
    pub anchor_tokens: usize,
    pub planned_savings: usize,
    pub applied: bool,
    pub cache_status: Option<String>,
    pub cache_reason: Option<String>,
    pub retained_prefix_tokens: usize,
    pub updates: Vec<EvictionUpdateView>,
    pub notes: Vec<String>,
    pub summary: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct EvictionUpdateView {
    pub episode_id: String,
    pub from_tier: String,
    pub to_tier: String,
    pub tokens_before: usize,
    pub tokens_after: usize,
    pub reason: String,
}

/// `memory.staleness` input.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct StalenessInput {
    #[serde(default)]
    pub project_id: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
}

/// `memory.staleness` output.
#[derive(Debug, Serialize, JsonSchema)]
pub struct StalenessOutput {
    pub total: usize,
    pub stale: usize,
    pub stale_rate: f64,
    pub deleted_anchors: usize,
    pub entries: Vec<StaleEntryView>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct StaleEntryView {
    pub atlas_id: String,
    pub anchor_type: String,
    pub anchor_id: String,
    pub reason: String,
}

/// `sakur4.status` output.
#[derive(Debug, Serialize, JsonSchema)]
pub struct StatusOutput {
    pub version: String,
    pub protocol_version: String,
    pub project_id: String,
    pub backend: String,
    pub backend_note: String,
    pub capabilities: String,
    pub cache_coherence: String,
    pub tokenizer: String,
    pub embedder: String,
    pub context_window: usize,
    pub episodes: i64,
    pub symbolic_facts: i64,
    pub atlas_entries: i64,
    pub stale_entries: i64,
    pub anchors: i64,
    pub repo_files: i64,
}

/// `sakur4.dream` input.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct DreamInput {
    /// Ignore the quiet-period gate. Only sensible when the caller knows the
    /// slots are idle; the consolidator still refuses to overlap a generation it
    /// can observe.
    #[serde(default)]
    pub force: bool,
}

/// `sakur4.dream` output.
#[derive(Debug, Serialize, JsonSchema)]
pub struct DreamOutput {
    pub ran: bool,
    pub skipped_reason: Option<String>,
    pub promoted: usize,
    pub regenerated: usize,
    pub reembedded: usize,
    pub archived: usize,
    pub elapsed_ms: i64,
    pub notes: Vec<String>,
    pub summary: String,
}

// ===========================================================================
// Helpers
// ===========================================================================

fn session_or_default(session: Option<String>) -> String {
    session.unwrap_or_else(|| "default".to_string())
}

fn slot_or_default(slot: Option<String>) -> String {
    slot.unwrap_or_else(|| "0".to_string())
}

fn to_error(e: impl std::fmt::Display) -> ErrorData {
    ErrorData::internal_error(e.to_string(), None)
}

/// Assemble the prompt for a session from the Fabric, the way a harness would.
async fn assemble_parts(engine: &Engine, session: &str, extra_recall: Option<&str>) -> Result<PromptParts, ErrorData> {
    let anchors = engine
        .memory()
        .anchors(Some(session))
        .await
        .map_err(to_error)?;
    let anchor_block = anchors
        .iter()
        .map(|a| a.render())
        .collect::<Vec<_>>()
        .join("\n");
    let timeline = engine
        .memory()
        .timeline(session, 1_000_000, engine.tokens(), false)
        .await
        .map_err(to_error)?;
    let mut parts = PromptParts::new()
        .with_system("You are a local coding agent using Sakur4 memory and context management.")
        .with_anchors(anchor_block)
        .with_timeline(timeline.rendered);
    if let Some(recall) = extra_recall {
        parts = parts.with_recall(recall.to_string());
    }
    Ok(parts)
}

/// The Sakur4 MCP server.
#[derive(Clone)]
pub struct Sakur4Server {
    engine: Arc<Engine>,
    tool_router: ToolRouter<Self>,
}

impl Sakur4Server {
    /// Build the server over an engine.
    pub fn new(engine: Engine) -> Self {
        Self {
            engine: Arc::new(engine),
            tool_router: Self::tool_router(),
        }
    }

    pub fn engine(&self) -> &Engine {
        &self.engine
    }
}

// ===========================================================================
// Tools
// ===========================================================================

#[rmcp::tool_router]
impl Sakur4Server {
    /// Append a turn or tool result to the Episodic Stream.
    #[rmcp::tool(
        name = "memory.commit_episode",
        description = "Append a turn or tool result to the Episodic Stream. This is the source \
                       of truth: content is never rewritten. Structured tool output (JSON, CSV, \
                       HTTP headers, diffs, exit codes) is additionally parsed into deterministic \
                       facts for the Symbolic Ledger. If the turn appears to state a constraint, \
                       a pin is *suggested* but never applied automatically."
    )]
    async fn commit_episode(
        &self,
        Parameters(input): Parameters<CommitEpisodeInput>,
    ) -> Result<Json<CommitEpisodeOutput>, ErrorData> {
        let role = Role::parse(&input.role).map_err(to_error)?;
        let session = session_or_default(input.session_id);
        let slot = slot_or_default(input.slot_id);

        let episode = NewEpisode {
            session_id: session.clone(),
            slot_id: Some(slot.clone()),
            role,
            content: input.content,
            tool_name: input.tool_name,
            fold_id: None,
            droppable: false,
            meta: None,
        };
        let out = self
            .engine
            .memory()
            .commit_episode(episode, self.engine.tokens(), true, true)
            .await
            .map_err(to_error)?;

        // Any harness request means the system is not idle.
        self.engine.eviction();

        Ok(Json(CommitEpisodeOutput {
            episode_id: out.episode_id,
            seq: out.seq,
            token_count: out.token_count,
            symbolic_facts: out.facts_extracted,
            symbolic_summary: out.symbolic_summary,
            suggested_anchor: out.anchor_proposal.map(|p| SuggestedAnchor {
                kind: p.kind.as_str().to_string(),
                excerpt: p.excerpt,
                rule: p.rule.to_string(),
                confidence: p.confidence,
                hint: format!(
                    "Call memory.pin with kind \"{}\" if this must survive every compaction.",
                    p.kind.as_str()
                ),
            }),
        }))
    }

    /// Add an entry to the Anchor Set.
    #[rmcp::tool(
        name = "memory.pin",
        description = "Pin a constraint, correction or task contract into the Anchor Set. \
                       Pinned content is exempt from every eviction tier and is rendered \
                       verbatim into every assembled prompt. If the Anchor Set alone would \
                       exceed the context budget, pinning does not silently drop anything — \
                       the system reports the overflow instead."
    )]
    async fn pin(
        &self,
        Parameters(input): Parameters<PinInput>,
    ) -> Result<Json<PinOutput>, ErrorData> {
        let kind = AnchorKind::parse(&input.kind).map_err(to_error)?;
        let mut req = PinRequest::new(kind, input.content).by("agent");
        if let Some(session) = input.session_id {
            req = req.in_session(session);
        }
        let row = self.engine.memory().pin(req).await.map_err(to_error)?;
        let token_cost = row.token_cost(self.engine.tokens());
        Ok(Json(PinOutput {
            anchor_id: row.anchor_id,
            kind: row.kind.as_str().to_string(),
            token_cost,
            note: format!(
                "Pinned. It now costs {token_cost} tokens in every prompt for the rest of the \
                 session, and no compaction can remove it."
            ),
        }))
    }

    /// Hybrid retrieval with staleness-aware reranking.
    #[rmcp::tool(
        name = "memory.recall",
        description = "Search the Memory Fabric with lexical, dense and graph retrieval merged \
                       and reranked. Any interpretation whose source has since changed is \
                       returned with stale=true and the source's CURRENT value attached — trust \
                       current_value, not text. Episodes are returned verbatim; eviction never \
                       alters stored content."
    )]
    async fn recall(
        &self,
        Parameters(input): Parameters<RecallInput>,
    ) -> Result<Json<RecallOutput>, ErrorData> {
        let filters = RecallFilters {
            session_id: input.session_id.clone(),
            file_path: input.file_path.clone(),
            include_folded: if input.include_folded { Some(true) } else { None },
            ..Default::default()
        };
        let result = self
            .engine
            .recall()
            .recall(&input.query, input.k, Some(filters))
            .await
            .map_err(to_error)?;

        let results = result
            .hits
            .iter()
            .map(|h| RecallItem {
                kind: h.kind.as_str().to_string(),
                id: h.id.clone(),
                score: h.score,
                retrievers: h.retrievers.iter().map(|r| r.as_str().to_string()).collect(),
                stale: h.stale,
                current_value: h.stale_replacement.clone(),
                text: h.rendered.clone(),
                token_cost: h.token_cost,
                reasons: h.reasons.clone(),
            })
            .collect();

        // Compute the rendering before moving the fields out of `result`.
        let rendered = result.render();
        Ok(Json(RecallOutput {
            query: result.query,
            results,
            stale_flags: result.stale_superseded,
            backends: result.backends,
            notes: result.notes,
            rendered,
        }))
    }

    /// Open an isolated sub-context for a token-intensive subtask.
    #[rmcp::tool(
        name = "memory.fold",
        description = "Open an isolated sub-context: a checkpoint is taken at the current \
                       position and everything you do until memory.unfold is attributed to the \
                       fold. On unfold, the fold collapses to a single result line and the \
                       slot's KV state is rolled back to the pre-fold checkpoint, so the \
                       subtask's intermediate steps never occupy the main trajectory."
    )]
    async fn fold(
        &self,
        Parameters(input): Parameters<FoldInput>,
    ) -> Result<Json<FoldOutput>, ErrorData> {
        let session = session_or_default(input.session_id);
        let slot = slot_or_default(input.slot_id);
        let out = self
            .engine
            .eviction()
            .fold(&session, &slot, &input.description, &input.goal)
            .await
            .map_err(to_error)?;
        Ok(Json(FoldOutput {
            fold_id: out.fold_id.clone(),
            checkpoint: out.checkpoint,
            token_position: out.token_position,
            tokens_at_open: out.tokens_at_open,
            cache_note: out.cache_note,
            next_steps: format!(
                "Work the subtask, attributing anything of value with memory.commit_episode. \
                 When finished, call memory.unfold with fold_id \"{}\" and a result_summary.",
                out.fold_id
            ),
        }))
    }

    /// Collapse a fold back into the main trajectory.
    #[rmcp::tool(
        name = "memory.unfold",
        description = "Close a fold: its intermediate steps leave the live window and only the \
                       result summary remains. The full trace stays retrievable via \
                       memory.recall_fold, and the inference slot is rolled back to the \
                       checkpoint taken when the fold opened."
    )]
    async fn unfold(
        &self,
        Parameters(input): Parameters<UnfoldInput>,
    ) -> Result<Json<UnfoldOutput>, ErrorData> {
        let session = session_or_default(input.session_id);
        let slot = slot_or_default(input.slot_id);
        let out = self
            .engine
            .eviction()
            .unfold(&session, &slot, &input.fold_id, &input.result_summary)
            .await
            .map_err(to_error)?;
        Ok(Json(UnfoldOutput {
            fold_id: out.fold_id,
            tokens_reclaimed: out.tokens_reclaimed,
            episodes_folded: out.episodes_folded,
            rollback_performed: out.rollback_performed,
            rollback_detail: out.rollback_detail,
            trace_retrievable: out.trace_retrievable,
        }))
    }

    /// Retrieve the full trace of a folded subtask.
    #[rmcp::tool(
        name = "memory.recall_fold",
        description = "Retrieve every episode of a folded subtask, verbatim, in order. Use this \
                       when the collapsed result summary turns out to be insufficient."
    )]
    async fn recall_fold(
        &self,
        Parameters(input): Parameters<RecallFoldInput>,
    ) -> Result<Json<serde_json::Value>, ErrorData> {
        let trace = self
            .engine
            .eviction()
            .recall_fold(&input.fold_id)
            .await
            .map_err(to_error)?;
        serde_json::to_value(trace)
            .map(Json)
            .map_err(to_error)
    }

    /// A token-budgeted, centrality-ranked outline of the repository.
    #[rmcp::tool(
        name = "code.get_repo_map",
        description = "Get a structural outline of the repository, ranked by how load-bearing \
                       each symbol is in the call graph and fitted to a token budget. Use it \
                       before opening files in full. A smaller budget returns a strict prefix of \
                       what a larger budget returns, so it is safe to call repeatedly."
    )]
    async fn get_repo_map(
        &self,
        Parameters(input): Parameters<RepoMapInput>,
    ) -> Result<Json<RepoMapOutput>, ErrorData> {
        let focus = input.focus_paths.as_deref();
        let (map, used) = self
            .engine
            .repo()
            .repo_map(input.token_budget, focus, self.engine.tokens())
            .await
            .map_err(to_error)?;
        Ok(Json(RepoMapOutput {
            map,
            tokens_used: used,
            token_budget: input.token_budget,
        }))
    }

    /// Look up a symbol's current deterministic signature.
    #[rmcp::tool(
        name = "code.query_symbol",
        description = "Look up a symbol's CURRENT signature, location and AST hash from the \
                       Symbolic Ledger. This is parser output, not memory: it cannot be stale, \
                       and it is the right way to check something you only remember from a \
                       summary."
    )]
    async fn query_symbol(
        &self,
        Parameters(input): Parameters<QuerySymbolInput>,
    ) -> Result<Json<QuerySymbolOutput>, ErrorData> {
        let found = self
            .engine
            .recall()
            .query_symbol(&input.qualified_name)
            .await
            .map_err(to_error)?;
        Ok(Json(match found {
            Some(fact) => QuerySymbolOutput {
                found: true,
                qualified_name: fact.qualified_name.clone(),
                kind: Some(fact.kind.as_str().to_string()),
                signature: fact.signature.clone(),
                file: fact.file_path.clone(),
                line: fact.line_start,
                ast_hash: Some(fact.ast_hash.clone()),
                source: Some(fact.source.as_str().to_string()),
                note: "Deterministic parser output — safe to rely on.".into(),
            },
            None => QuerySymbolOutput {
                found: false,
                qualified_name: input.qualified_name.clone(),
                kind: None,
                signature: None,
                file: None,
                line: None,
                ast_hash: None,
                source: None,
                note: "Not in the Symbolic Ledger. The project may not be indexed, or the \
                       qualified name may differ — try code.get_repo_map to see the names in use."
                    .into(),
            },
        }))
    }

    /// Compute the blast radius of changing a symbol.
    #[rmcp::tool(
        name = "code.impact_of_change",
        description = "List every call site that depends on a symbol, transitively, from the \
                       pre-computed call and import graph. Use it before a signature change so \
                       the edit does not break callers you have not read."
    )]
    async fn impact_of_change(
        &self,
        Parameters(input): Parameters<ImpactInput>,
    ) -> Result<Json<ImpactOutput>, ErrorData> {
        let depth = input.depth.unwrap_or(4).clamp(1, 16);
        let report = self
            .engine
            .repo()
            .impact_of_change(&input.qualified_name, depth)
            .await
            .map_err(to_error)?;
        Ok(Json(ImpactOutput {
            symbol: report.symbol.clone(),
            signature: report.signature.clone(),
            ast_hash: report.ast_hash.clone(),
            callers: report
                .affected
                .iter()
                .map(|e| ImpactCaller {
                    qualified_name: e.qualified_name.clone(),
                    file: e.file_path.clone(),
                    line: e.line,
                    depth: e.depth,
                    via: e.via.clone(),
                })
                .collect(),
            rendered: report.render(),
        }))
    }

    /// Force a KV-cache save for a slot.
    #[rmcp::tool(
        name = "session.snapshot",
        description = "Persist the slot's KV cache to disk immediately. Worth calling before a \
                       long generation or before an unavoidable rewrite: it turns a 60-120 second \
                       cold prefill later into a sub-second restore."
    )]
    async fn snapshot(
        &self,
        Parameters(input): Parameters<SnapshotInput>,
    ) -> Result<Json<SnapshotOutput>, ErrorData> {
        let session = session_or_default(input.session_id);
        let slot = slot_or_default(input.slot_id);
        let out = self
            .engine
            .coherence()
            .snapshot(&session, &slot)
            .await
            .map_err(to_error)?;
        Ok(Json(SnapshotOutput {
            snapshot_id: out.snapshot_id,
            file_path: out.file_path,
            size_bytes: out.size_bytes,
            elapsed_ms: out.elapsed_ms,
        }))
    }

    /// Warm-restore a previously snapshotted slot.
    #[rmcp::tool(
        name = "session.restore",
        description = "Reload a slot's KV state from a save file produced by session.snapshot. \
                       Call this before the first turn of a new day to avoid paying for a full \
                       re-prefill of an existing session."
    )]
    async fn restore(
        &self,
        Parameters(input): Parameters<RestoreInput>,
    ) -> Result<Json<RestoreOutput>, ErrorData> {
        let session = session_or_default(input.session_id);
        let slot = slot_or_default(input.slot_id);
        let out = self
            .engine
            .coherence()
            .restore(&session, &slot, &input.path)
            .await
            .map_err(to_error)?;
        Ok(Json(RestoreOutput {
            restored: out.restored,
            restore_time_ms: out.restore_time_ms,
            detail: out.detail,
        }))
    }

    /// Return the most recent Context Ledger Receipt.
    #[rmcp::tool(
        name = "context.receipt",
        description = "Show where the context budget went this turn, category by category, plus \
                       the cache verdict: whether the prompt reused the slot's KV prefix, had to \
                       re-prefill, or was warm-restored. Use it when a turn felt slow, or to \
                       check whether compaction is actually paying for itself."
    )]
    async fn receipt(
        &self,
        Parameters(input): Parameters<ReceiptInput>,
    ) -> Result<Json<ReceiptOutput>, ErrorData> {
        let session = session_or_default(input.session_id);
        let window = self.engine.context_window().await;

        let receipt = if input.assemble {
            let parts = assemble_parts(&self.engine, &session, None).await?;
            let cache = self
                .engine
                .coherence()
                .observe_prompt(&session, "0", &parts.render(), self.engine.tokens())
                .await
                .map_err(to_error)?;
            let mut r = Receipt::build(
                &session,
                Some("0"),
                self.engine
                    .receipts()
                    .next_turn(&session)
                    .await
                    .map_err(to_error)?,
                &parts,
                self.engine.tokens(),
                window,
            )
            .with_cache(cache.cache_status.as_str(), cache.detail.clone())
            .with_cache_numbers(cache.reused_tokens, cache.prefilled_tokens)
            .with_backend(self.engine.backend().name());
            if let Some(ms) = cache.prompt_eval_ms {
                r = r.with_prompt_eval_ms(ms);
            }
            r
        } else {
            match self
                .engine
                .receipts()
                .latest(&session)
                .await
                .map_err(to_error)?
            {
                Some(r) => r,
                None => {
                    let parts = assemble_parts(&self.engine, &session, None).await?;
                    Receipt::build(&session, Some("0"), 0, &parts, self.engine.tokens(), window)
                }
            }
        };

        let stats = self
            .engine
            .receipts()
            .stats(Some(&session))
            .await
            .map_err(to_error)?;

        Ok(Json(ReceiptOutput {
            breakdown: BreakdownView {
                system_prompt: receipt.breakdown.system_prompt,
                pinned_anchors: receipt.breakdown.pinned_anchors,
                retrieved_memory: receipt.breakdown.retrieved_memory,
                repo_map: receipt.breakdown.repo_map,
                raw_recent_history: receipt.breakdown.raw_recent_history,
                tool_schemas: receipt.breakdown.tool_schemas,
                fold_summaries: receipt.breakdown.fold_summaries,
                other: receipt.breakdown.other,
            },
            total_tokens: receipt.total_tokens,
            context_window: receipt.context_window,
            cache_status: receipt.cache_status.clone(),
            cache_detail: receipt.cache_detail.clone(),
            prompt_eval_ms: receipt.prompt_eval_ms,
            tokens_reused: receipt.prompt_tokens_reused,
            tokens_prefilled: receipt.prompt_tokens_prefilled,
            eviction: receipt.eviction.as_ref().map(|e| EvictionView {
                applied: e.applied,
                tokens_reclaimed: e.tokens_reclaimed,
                snapshot_taken: e.snapshot_taken,
                boundary: e.boundary.clone(),
            }),
            rendered: receipt.render(),
            stats: stats.render(),
        }))
    }

    /// Plan (and optionally apply) an eviction.
    #[rmcp::tool(
        name = "context.plan_eviction",
        description = "Ask the Graduated Eviction Engine what it would evict right now, and why. \
                       Defaults to planning only, so nothing changes until you pass apply=true. \
                       Each update names the tier it moves to and the reason, and the response \
                       includes the cache verdict for the resulting boundary."
    )]
    async fn plan_eviction(
        &self,
        Parameters(input): Parameters<PlanEvictionInput>,
    ) -> Result<Json<PlanEvictionOutput>, ErrorData> {
        let slot = slot_or_default(input.slot_id);
        let window = self.engine.context_window().await;
        let parts = assemble_parts(
            &self.engine,
            &input.session_id,
            input.pending_recall.as_deref(),
        )
        .await?;

        let plan = self
            .engine
            .eviction()
            .plan(&input.session_id, &slot, window, &parts)
            .await
            .map_err(to_error)?;

        let mut applied = false;
        if input.apply && !plan.is_empty() && plan.pressure == Pressure::Compacting {
            self.engine
                .eviction()
                .apply(&plan, &parts)
                .await
                .map_err(to_error)?;
            applied = true;
        }

        let cache = plan.coherence.as_ref();
        Ok(Json(PlanEvictionOutput {
            pressure: format!("{:?}", plan.pressure).to_lowercase(),
            budget: plan.budget,
            trigger_at: plan.threshold,
            target: plan.target,
            live_tokens: plan.live_tokens,
            anchor_tokens: plan.anchor_tokens,
            planned_savings: plan.planned_savings,
            applied,
            cache_status: cache.map(|c| c.status.as_str().to_string()),
            cache_reason: cache.map(|c| c.reason.clone()),
            retained_prefix_tokens: plan.retained_prefix_tokens,
            updates: plan
                .updates
                .iter()
                .map(|u| EvictionUpdateView {
                    episode_id: u.episode_id.clone(),
                    from_tier: u.from.as_str().to_string(),
                    to_tier: u.to.as_str().to_string(),
                    tokens_before: u.tokens_before,
                    tokens_after: u.tokens_after,
                    reason: u.reason.clone(),
                })
                .collect(),
            notes: plan.notes.clone(),
            summary: plan.summary(),
        }))
    }

    /// Report staleness between the Semantic Atlas and its anchors.
    #[rmcp::tool(
        name = "memory.staleness",
        description = "Report which stored interpretations no longer match the source they were \
                       derived from. Each entry names its anchor and whether the anchor changed \
                       or disappeared. Use it to decide what to re-read rather than trusting a \
                       summary you wrote a while ago."
    )]
    async fn staleness(
        &self,
        Parameters(input): Parameters<StalenessInput>,
    ) -> Result<Json<StalenessOutput>, ErrorData> {
        let limit = input.limit.unwrap_or(50).clamp(1, 500);
        let report = self
            .engine
            .memory()
            .staleness_report(input.project_id.as_deref(), limit)
            .await
            .map_err(to_error)?;
        Ok(Json(StalenessOutput {
            total: report.total,
            stale: report.stale,
            stale_rate: report.rate(),
            deleted_anchors: report.deleted_anchors,
            entries: report
                .entries
                .iter()
                .map(|e| StaleEntryView {
                    atlas_id: e.atlas_id.clone(),
                    anchor_type: e.anchor_type.as_str().to_string(),
                    anchor_id: e.anchor_id.clone(),
                    reason: format!("{:?}", e.reason).to_lowercase(),
                })
                .collect(),
        }))
    }

    /// Report what Sakur4 is and how it is configured.
    #[rmcp::tool(
        name = "sakur4.status",
        description = "Report the resolved backend, its detected cache capabilities, the \
                       tokenizer and embedder in use, and counts for each Fabric store. Call it \
                       once at the start of a session to learn whether cache-coherent compaction \
                       is available or whether you should pin more aggressively."
    )]
    async fn status(&self) -> Result<Json<StatusOutput>, ErrorData> {
        let s = self.engine.status().await.map_err(to_error)?;
        let coherence = if s.capabilities.can_align_boundaries() {
            format!(
                "checkpoint-aligned compaction available ({})",
                s.cache_summary
            )
        } else if s.capabilities.reachable {
            format!(
                "no usable checkpoint source ({}) — compaction will report full re-prefill",
                s.cache_summary
            )
        } else {
            "cache coherence disabled — every compaction is a full re-prefill".to_string()
        };
        Ok(Json(StatusOutput {
            version: env!("CARGO_PKG_VERSION").to_string(),
            protocol_version: MCP_PROTOCOL_VERSION.to_string(),
            project_id: s.project_id,
            backend: s.backend_spec,
            backend_note: s.backend_note,
            capabilities: s.cache_summary,
            cache_coherence: coherence,
            tokenizer: s.tokenizer,
            embedder: s.embedder,
            context_window: self.engine.context_window().await,
            episodes: s.db.episodes,
            symbolic_facts: s.db.symbolic_facts,
            atlas_entries: s.db.semantic_entries,
            stale_entries: s.db.stale_entries,
            anchors: s.db.anchors,
            repo_files: s.repo_files,
        }))
    }

    /// Run one Idle Consolidator pass.
    #[rmcp::tool(
        name = "sakur4.dream",
        description = "Run one memory-maintenance pass: promote substantial turns into the \
                       Semantic Atlas, regenerate summaries whose source changed, embed anything \
                       missing a vector, and archive long-cold episodes. It refuses to run while \
                       any tracked slot is generating, and is safe to call at any time."
    )]
    async fn dream(
        &self,
        Parameters(input): Parameters<DreamInput>,
    ) -> Result<Json<DreamOutput>, ErrorData> {
        let cfg = ConsolidatorConfig {
            quiet_period_secs: if input.force { 0 } else { 90 },
            ..Default::default()
        };
        let consolidator = Consolidator::new(
            self.engine.db().clone(),
            self.engine.memory().clone(),
            self.engine.backend().clone(),
            self.engine.embedder().clone(),
            self.engine.tokens().clone(),
            cfg,
        );
        let report = consolidator.maybe_run().await.map_err(to_error)?;
        let summary = report.summary();
        Ok(Json(DreamOutput {
            ran: report.ran,
            skipped_reason: report.skipped_reason,
            promoted: report.promoted,
            regenerated: report.regenerated,
            reembedded: report.reembedded,
            archived: report.archived,
            elapsed_ms: report.elapsed_ms,
            notes: report.notes,
            summary,
        }))
    }
}

// ===========================================================================
// ServerHandler
// ===========================================================================

impl ServerHandler for Sakur4Server {
    fn get_info(&self) -> ServerInfo {
        let mut info = ServerInfo::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_resources()
                .enable_prompts()
                .build(),
        );
        info.protocol_version = ProtocolVersion::V_2026_07_28;
        info.server_info = Implementation::new("sakur4", env!("CARGO_PKG_VERSION"));
        info.instructions = Some(
                "Sakur4 is a memory and context layer for long local agent sessions.\n\
                 \n\
                 How to work with it:\n\
                 * Commit each turn and tool result with memory.commit_episode. That is the \
                 only source of truth; everything else is derived from it.\n\
                 * Before a long or uncertain stretch of work, call context.plan_eviction (no \
                 apply) to see what would be evicted and whether the resulting boundary reuses \
                 the inference cache. Pass apply=true only when you accept it.\n\
                 * For a token-intensive subtask, wrap it in memory.fold / memory.unfold so its \
                 intermediate steps never occupy the main trajectory.\n\
                 * Never trust a recalled summary whose source may have changed: memory.recall \
                 attaches the current value of any stale interpretation, and code.query_symbol \
                 gives the current truth for a symbol.\n\
                 * If the user states a rule, correction or hard requirement, pin it with \
                 memory.pin — otherwise it can be compacted away.\n\
                 * When a turn feels slow, call context.receipt to see where the tokens went \
                 and whether the prompt had to be re-prefilled."
                .into(),
        );
        info
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        // The catalog is configuration-independent, so a long TTL is honest, and
        // the bytes are identical between calls — which is what FR-14's second
        // acceptance criterion asks for.
        Ok(ListToolsResult::with_all_items(self.tool_router.list_all())
            .with_ttl_ms(TOOL_CATALOG_TTL_MS)
            .with_cache_scope(CacheScope::Private))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let tcc = rmcp::handler::server::tool::ToolCallContext::new(self, request, context);
        self.tool_router.call(tcc).await
    }

    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, ErrorData> {
        let project = self.engine.project_id().to_string();
        Ok(ListResourcesResult::with_all_items(vec![
            Resource::new(format!("sakur4://repo-map/{project}"), "repo-map")
                .with_description(
                    "Cacheable structural outline of the repository. TTL tracks Repo Cortex's \
                     last re-index.",
                )
                .with_mime_type("text/plain"),
            Resource::new("sakur4://receipt/latest", "context-receipt")
                .with_description(
                    "The most recent Context Ledger Receipt. Never cached: it describes one turn.",
                )
                .with_mime_type("text/plain"),
            Resource::new(format!("sakur4://anchors/{project}"), "anchor-set")
                .with_description("The pinned Anchor Set, invalidated by any memory.pin call.")
                .with_mime_type("text/plain"),
            Resource::new(format!("sakur4://status/{project}"), "status")
                .with_description(
                    "Resolved backend, detected cache capabilities and Fabric counts.",
                )
                .with_mime_type("text/plain"),
        ])
        // The repo map moves with the repository, so the catalog TTL would be
        // wrong here; this is the refresh interval a client should assume.
        .with_ttl_ms(REPO_MAP_TTL_MS)
        .with_cache_scope(CacheScope::Private))
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, ErrorData> {
        let uri = request.uri.clone();
        let text = if uri.starts_with("sakur4://repo-map/") {
            let (map, used) = self
                .engine
                .repo()
                .repo_map(2_000, None, self.engine.tokens())
                .await
                .map_err(to_error)?;
            format!("{map}\n({used} tokens)")
        } else if uri == "sakur4://receipt/latest" {
            match self
                .engine
                .receipts()
                .latest("default")
                .await
                .map_err(to_error)?
            {
                Some(r) => r.render(),
                None => "no receipt recorded yet; call context.receipt to assemble one".into(),
            }
        } else if uri.starts_with("sakur4://anchors/") {
            let anchors = self
                .engine
                .memory()
                .anchors(None)
                .await
                .map_err(to_error)?;
            if anchors.is_empty() {
                "the Anchor Set is empty".to_string()
            } else {
                anchors
                    .iter()
                    .map(|a| a.render())
                    .collect::<Vec<_>>()
                    .join("\n")
            }
        } else if uri.starts_with("sakur4://status/") {
            let s = self.engine.status().await.map_err(to_error)?;
            format!(
                "backend: {} ({})\ncapabilities: {}\ntokenizer: {}\nembedder: {}\n\
                 episodes: {}\nsymbolic facts: {}\natlas entries: {}\nstale entries: {}\n\
                 anchors: {}\nrepo files: {}",
                s.backend_spec,
                s.backend_note,
                s.cache_summary,
                s.tokenizer,
                s.embedder,
                s.db.episodes,
                s.db.symbolic_facts,
                s.db.semantic_entries,
                s.db.stale_entries,
                s.db.anchors,
                s.repo_files
            )
        } else {
            return Err(ErrorData::resource_not_found(
                format!("unknown resource {uri}"),
                None,
            ));
        };

        Ok(ReadResourceResponse::Complete(
            ReadResourceResult::new(vec![ResourceContents::text(text, uri)]),
        ))
    }

    async fn list_prompts(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListPromptsResult, ErrorData> {
        Ok(ListPromptsResult::with_all_items(vec![Prompt::new(
            "sakur4_system_preamble",
            Some(
                "Reusable preamble describing how to use Sakur4's tools in a long session. \
                 Harness adapters may inject it automatically; it is tuned for smaller \
                 instruction-tuned models that under-trigger proactive folding.",
            ),
            None,
        )])
        .with_ttl_ms(TOOL_CATALOG_TTL_MS)
        .with_cache_scope(CacheScope::Private))
    }

    async fn get_prompt(
        &self,
        request: GetPromptRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<GetPromptResponse, ErrorData> {
        if request.name != "sakur4_system_preamble" {
            return Err(ErrorData::invalid_params(
                format!("unknown prompt {}", request.name),
                None,
            ));
        }
        Ok(GetPromptResponse::Complete(
            GetPromptResult::new(vec![PromptMessage::new_text(
                rmcp::model::Role::User,
                PREAMBLE,
            )])
            .with_description("Sakur4 working preamble"),
        ))
    }
}

/// The reusable system-prompt fragment (FR-21's `sakur4_system_preamble`).
///
/// Deliberately specific about *when* to call each tool. The PRD's risk register
/// notes that smaller local models under-trigger agent-directed folding, so the
/// preamble names the trigger conditions rather than describing the tools.
pub const PREAMBLE: &str = "\
You have a memory and context layer (Sakur4) available through its tools.

Working rules:
1. After each turn or tool result, call memory.commit_episode with the content verbatim.
   Structured results (JSON, CSV, headers, diffs, exit codes) become deterministic facts;
   prose is stored as-is.
2. Before starting anything that will take more than a few steps of exploration, call
   memory.fold with a short description and a goal. Do the work inside the fold, then call
   memory.unfold with a concise result summary. The intermediate steps leave your context and
   the inference cache rolls back, so the work costs almost nothing to have done.
3. If the user states a rule, a correction, or a hard requirement, call memory.pin for it
   immediately. Unpinned requirements can be compacted away; pinned ones cannot.
4. When you are unsure whether something you remember is still true, check it:
   code.query_symbol for a signature, memory.recall for anything else. A recalled entry
   marked stale comes with the current value of its source — use that value, not the summary.
5. If a turn takes a long time, call context.receipt and read the cache line: it says whether
   the prompt reused the inference cache or had to be prefilled from scratch.";

/// Compile-time guard: every tool the router exposes must be declared in the
/// prompt's vocabulary.
#[allow(dead_code)]
fn _tool_name_guard() -> [&'static str; 13] {
    [
        "memory.commit_episode",
        "memory.pin",
        "memory.recall",
        "memory.fold",
        "memory.unfold",
        "memory.recall_fold",
        "code.get_repo_map",
        "code.query_symbol",
        "code.impact_of_change",
        "session.snapshot",
        "session.restore",
        "context.receipt",
        "context.plan_eviction",
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_preamble_names_every_tool_it_tells_the_model_to_use() {
        for name in [
            "memory.commit_episode",
            "memory.pin",
            "memory.fold",
            "memory.unfold",
            "memory.recall",
            "code.query_symbol",
            "context.receipt",
        ] {
            assert!(
                PREAMBLE.contains(name),
                "the preamble instructs the model without naming {name}"
            );
        }
    }

    #[test]
    fn the_preamble_states_when_to_fold_not_just_that_it_can() {
        // The PRD's stated mitigation for under-triggering on smaller models.
        assert!(PREAMBLE.contains("more than a few steps"));
        assert!(PREAMBLE.contains("immediately"));
    }
}
