//! The entry point to Sakur4's functionality, independent of the MCP transport.
//!
//! The `sakur4d` binary's MCP gateway is a thin adapter over this type, which is
//! what keeps the tool contract (C7) stable while the internals (C1-C6, C8)
//! evolve — the PRD calls this out as C7's entire reason for existing.

use std::sync::Arc;
use std::time::Duration;

use crate::cache::{Coherence, CoherenceConfig};
use crate::embed::Embedder;
use crate::error::Result;
use crate::evict::{EvictionEngine, EvictionPolicy};
use crate::llama::{BackendSpec, InferenceBackend, ResolvedBackend};
use crate::memory::MemoryFabric;
use crate::recall::{RecallEngine, RecallPolicy};
use crate::receipt::ReceiptLog;
use crate::repo::RepoCortex;
use crate::store::Db;
use crate::tokens::TokenCounter;

/// Everything the engine needs to run.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct EngineConfig {
    /// Where the memory fabric lives. `:memory:` for an ephemeral run.
    pub db_path: String,
    /// Which inference backend to use: `auto`, `embedded`, `none`, or a URL.
    pub backend: String,
    /// How long to wait when probing a backend at startup.
    pub probe_timeout_ms: u64,
    /// Local embedding endpoint, if any.
    pub embed_url: Option<String>,
    /// Embedding model name for that endpoint.
    pub embed_model: Option<String>,
    /// Repository root for Repo Cortex, if any.
    pub project_root: Option<String>,
    /// Explicit project id; derived from the root path when absent.
    pub project_id: Option<String>,
    /// Default context window when the backend cannot report one.
    pub default_n_ctx: usize,
    /// Whether `default_n_ctx` was set deliberately rather than defaulted.
    ///
    /// Distinguishes "the user told me the window is 12288" from "nobody said, so it is
    /// 32768". Without it a user's explicit setting loses to a backend's *simulated*
    /// answer, which is how `--context-window` came to have no effect on the embedded
    /// backend while appearing to be accepted.
    #[serde(default)]
    pub context_window_explicit: bool,
    pub eviction: EvictionPolicy,
    pub coherence: CoherenceConfig,
    pub recall: RecallPolicy,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            db_path: "sakur4.db".into(),
            backend: "auto".into(),
            probe_timeout_ms: 1500,
            embed_url: None,
            embed_model: None,
            project_root: None,
            project_id: None,
            default_n_ctx: 32_768,
            context_window_explicit: false,
            eviction: EvictionPolicy::default(),
            coherence: CoherenceConfig::default(),
            recall: RecallPolicy::default(),
        }
    }
}

impl EngineConfig {
    /// Load from a TOML file, falling back to defaults for absent keys.
    pub fn from_toml_path(path: &std::path::Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)?;
        Ok(toml::from_str(&raw)?)
    }

    /// Apply environment overrides (`SAKUR4_*`), then CLI overrides.
    pub fn with_env(mut self) -> Self {
        if let Ok(v) = std::env::var("SAKUR4_DB")
            && !v.trim().is_empty()
        {
            self.db_path = v;
        }
        if let Ok(v) = std::env::var("SAKUR4_BACKEND")
            && !v.trim().is_empty()
        {
            self.backend = v;
        }
        if let Ok(v) = std::env::var("SAKUR4_PROJECT_ROOT")
            && !v.trim().is_empty()
        {
            self.project_root = Some(v);
        }
        if let Ok(v) = std::env::var("SAKUR4_EMBED_URL")
            && !v.trim().is_empty()
        {
            self.embed_url = Some(v);
        }
        if let Ok(v) = std::env::var("SAKUR4_EMBED_MODEL")
            && !v.trim().is_empty()
        {
            self.embed_model = Some(v);
        }
        self
    }

    /// Resolve the default project id for the configured root.
    pub fn resolved_project_id(&self) -> String {
        if let Some(id) = &self.project_id {
            return id.clone();
        }
        match &self.project_root {
            Some(root) => format!("proj_{}", crate::ids::short_hash_str(root)),
            None => "proj_default".into(),
        }
    }
}

/// Component status, for `doctor` and the MCP `sakur4://status` resource.
#[derive(Debug, Clone, serde::Serialize)]
pub struct EngineStatus {
    pub project_id: String,
    pub db: crate::store::DbStats,
    pub backend_name: String,
    pub backend_spec: String,
    pub backend_note: String,
    pub capabilities: crate::llama::CapabilitySet,
    pub cache_summary: String,
    pub tokenizer: String,
    pub embedder: String,
    pub repo_files: i64,
    pub symbolic_facts: i64,
    pub recall_backends: Vec<String>,
}

/// The Sakur4 engine.
#[derive(Clone)]
pub struct Engine {
    config: EngineConfig,
    db: Db,
    counter: TokenCounter,
    backend: Arc<dyn InferenceBackend>,
    backend_note: String,
    requested_backend: BackendSpec,
    embedder: Arc<dyn Embedder>,
    memory: MemoryFabric,
    coherence: Coherence,
    eviction: EvictionEngine,
    recall: RecallEngine,
    repo: RepoCortex,
    receipts: ReceiptLog,
    project_id: String,
}

impl Engine {
    /// Open a store and wire every component, probing the backend dynamically.
    pub async fn open(config: EngineConfig) -> Result<Self> {
        let db = if config.db_path == ":memory:" || config.db_path.is_empty() {
            Db::open_in_memory().await?
        } else {
            Db::open(&config.db_path).await?
        };

        let spec = BackendSpec::parse(&config.backend);
        let resolved: ResolvedBackend =
            crate::llama::resolve(&spec, Duration::from_millis(config.probe_timeout_ms)).await;
        let backend = resolved.backend.clone();

        // Exact token accounting when the backend can do it, heuristic otherwise.
        let counter = {
            let b = backend.clone();
            crate::tokens::select_counter(move |text: &str| {
                let b = b.clone();
                let owned = text.to_string();
                async move { b.tokenize(&owned).await }
            })
            .await
        };

        let embedder =
            crate::embed::resolve(config.embed_url.as_deref(), config.embed_model.as_deref()).await;

        let memory = MemoryFabric::new(db.clone());
        let coherence = Coherence::new(db.clone(), backend.clone(), config.coherence.clone());

        // # Choose the eviction profile from what the backend can actually do
        //
        // The default policy is tuned to keep a large working set so a
        // cache-aligned boundary has room. That is the right trade on a backend with
        // a checkpoint ring, and the wrong one on a backend without: measured against
        // a real llama.cpp build exposing no checkpoints, it cost 29% more tokens per
        // turn while buying nothing, because there was no checkpoint to align to and
        // the server reuses prefixes by longest-common-prefix regardless.
        //
        // So the profile follows the capability probe rather than a constant. A user
        // should not have to know their server lacks checkpoints in order to get the
        // right ratios — that is what probing is for. `SAKUR4_EVICTION_PROFILE`
        // overrides, and `docs/DESIGN.md` records the measurements behind each
        // profile.
        let mut policy = config.eviction.clone();
        if policy.profile_explicit {
            // The user named a profile; honour it and change nothing.
        } else if let Some(named) = std::env::var("SAKUR4_EVICTION_PROFILE")
            .ok()
            .as_deref()
            .and_then(crate::evict::EvictionProfile::parse)
        {
            // An explicit choice wins over the probe. `WindowFirst` on a
            // checkpoint-capable server is a legitimate thing to want — it trades
            // cache reuse for window room — so this is not second-guessed.
            named.apply(&mut policy);
            policy.profile_explicit = true;
            tracing::info!(
                profile = named.as_str(),
                "eviction profile set by SAKUR4_EVICTION_PROFILE"
            );
        } else {
            let caps = backend.capabilities();
            let profile =
                crate::evict::EvictionProfile::for_capabilities(caps.can_align_boundaries());
            profile.apply(&mut policy);
            tracing::info!(
                profile = profile.as_str(),
                can_align = caps.can_align_boundaries(),
                trigger = policy.trigger_ratio,
                target = policy.target_ratio,
                "eviction profile selected from backend capabilities"
            );
        }
        let config = EngineConfig { eviction: policy.clone(), ..config };

        let eviction =
            EvictionEngine::new(memory.clone(), coherence.clone(), policy, counter.clone());
        let recall = RecallEngine::new(
            db.clone(),
            memory.clone(),
            embedder.clone(),
            config.recall.clone(),
            counter.clone(),
        );
        let project_id = config.resolved_project_id();
        let repo = RepoCortex::new(db.clone(), memory.clone(), project_id.clone());
        let receipts = ReceiptLog::new(db.clone());

        Ok(Self {
            config,
            db,
            counter,
            backend,
            backend_note: resolved.resolution_note,
            requested_backend: spec,
            embedder,
            memory,
            coherence,
            eviction,
            recall,
            repo,
            receipts,
            project_id,
        })
    }

    // --- accessors ---------------------------------------------------------

    pub fn config(&self) -> &EngineConfig {
        &self.config
    }
    pub fn db(&self) -> &Db {
        &self.db
    }
    pub fn tokens(&self) -> &TokenCounter {
        &self.counter
    }
    pub fn backend(&self) -> &Arc<dyn InferenceBackend> {
        &self.backend
    }
    pub fn embedder(&self) -> &Arc<dyn Embedder> {
        &self.embedder
    }
    pub fn memory(&self) -> &MemoryFabric {
        &self.memory
    }
    pub fn coherence(&self) -> &Coherence {
        &self.coherence
    }
    pub fn eviction(&self) -> &EvictionEngine {
        &self.eviction
    }
    pub fn recall(&self) -> &RecallEngine {
        &self.recall
    }
    pub fn repo(&self) -> &RepoCortex {
        &self.repo
    }
    pub fn receipts(&self) -> &ReceiptLog {
        &self.receipts
    }
    pub fn project_id(&self) -> &str {
        &self.project_id
    }
    pub fn backend_note(&self) -> &str {
        &self.backend_note
    }
    pub fn requested_backend(&self) -> &BackendSpec {
        &self.requested_backend
    }

    /// The context window to plan against.
    ///
    /// Prefer what the backend reports; fall back to configuration. Getting this
    /// wrong in the optimistic direction is what causes a hard context-overflow
    /// failure mid-session, so the fallback is the conservative default.
    pub async fn context_window(&self) -> usize {
        // # An explicit setting wins over the probe
        //
        // The backend is asked first because a real server knows its own `n_ctx` better
        // than a config file does — but "real" is the operative word. The embedded
        // backend *simulates* a slot and reports a fixed 32,768, so asking it first meant
        // `--context-window 12288` was silently overridden by a number that describes
        // nothing. A user who sets the window is stating a fact about their model; a
        // simulation asserting otherwise is not evidence, and a flag that appears to do
        // nothing is worse than one that does not exist.
        //
        // A reachable external backend still wins, because there the probe is real.
        if self.config.context_window_explicit {
            return self.config.default_n_ctx;
        }
        let caps = self.backend.capabilities();
        if caps.reachable {
            // `n_ctx` is reported per slot; slot 0 is the default single-slot case.
            if let Ok(state) = self.backend.slot_state("0").await
                && state.n_ctx > 0
            {
                return state.n_ctx as usize;
            }
        }
        self.config.default_n_ctx
    }

    /// Component status snapshot.
    pub async fn status(&self) -> Result<EngineStatus> {
        let db = self.db.stats().await?;
        let caps = self.backend.capabilities();
        let repo_files: i64 = self
            .db
            .with({
                let p = self.project_id.clone();
                move |c| {
                    Ok(c.query_row(
                        "SELECT COUNT(*) FROM repo_file WHERE project_id = ?1",
                        [p],
                        |r| r.get(0),
                    )
                    .unwrap_or(0))
                }
            })
            .await?;

        Ok(EngineStatus {
            project_id: self.project_id.clone(),
            backend_name: self.backend.name().to_string(),
            backend_spec: self.backend.spec(),
            backend_note: self.backend_note.clone(),
            cache_summary: caps.summary(),
            capabilities: caps,
            tokenizer: self.counter.kind().as_str().to_string(),
            embedder: self.embedder.describe(),
            repo_files,
            symbolic_facts: db.symbolic_facts,
            recall_backends: self.recall.active_backends(),
            db,
        })
    }

    /// Re-probe the backend and refresh the token counter.
    pub async fn refresh_backend(&self) -> Result<crate::llama::CapabilitySet> {
        let caps = self.backend.probe().await?;
        tracing::info!(caps = %caps.summary(), "backend reprobed");
        Ok(caps)
    }
}

impl std::fmt::Debug for Engine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Engine")
            .field("project_id", &self.project_id)
            .field("db", &self.config.db_path)
            .field("backend", &self.backend.name())
            .field("tokenizer", &self.counter.kind())
            .finish()
    }
}
