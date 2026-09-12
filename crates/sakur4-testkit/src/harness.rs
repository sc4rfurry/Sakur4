//! A scripted agent session driver.
//!
//! Eviction, receipts and the G1 metric are all *long-run* behaviours: a single
//! turn cannot demonstrate that anchors survive forty compactions, or that the
//! reuse ratio trends the right way. This driver turns a compact script into a
//! realistic session, so those properties can be asserted end to end.

use sakur4_core::cache::{Coherence, CoherenceConfig};
use sakur4_core::evict::EvictionEngine;
use sakur4_core::llama::embedded::EmbeddedBackend;
use sakur4_core::memory::anchor::{AnchorKind, PinRequest};
use sakur4_core::memory::episodic::{NewEpisode, Role};
use sakur4_core::memory::fabric::MemoryFabric;
use sakur4_core::prompt::PromptParts;
use sakur4_core::receipt::{EvictionSummary, Receipt, ReceiptLog};
use sakur4_core::store::Db;
use sakur4_core::tokens::{CharTokenizer, TokenCounter};
use std::sync::Arc;

/// What one scripted step should do.
#[derive(Debug, Clone)]
pub enum Step {
    /// A user turn of roughly `tokens` tokens.
    UserTurn { text: String, tokens: usize },
    /// A tool result of roughly `tokens` tokens.
    ToolResult { tool: String, tokens: usize },
    /// An assistant turn.
    AssistantTurn { text: String },
    /// Pin a constraint.
    Pin { kind: AnchorKind, text: String },
    /// Print the Context Ledger Receipt for the current turn.
    Receipt,
}

/// A scripted session.
pub struct ScriptedSession {
    fabric: MemoryFabric,
    engine: EvictionEngine,
    receipts: ReceiptLog,
    coherence: Coherence,
    counter: TokenCounter,
    backend: Arc<EmbeddedBackend>,
    session_id: String,
    slot_id: String,
    context_window: usize,
    turn: i64,
    /// Receipts produced by [`Step::Receipt`], in order.
    pub receipts_seen: Vec<Receipt>,
    /// Every eviction that happened, as summaries.
    pub evictions: Vec<EvictionSummary>,
}

impl ScriptedSession {
    /// Build a session with an embedded backend whose ring is 512 deep.
    pub async fn new(context_window: usize) -> sakur4_core::error::Result<Self> {
        let db = Db::open_in_memory().await?;
        let counter = TokenCounter::new(CharTokenizer { chars_per_token: 4 });
        let backend = Arc::new(EmbeddedBackend::new().with_ring(512, 12));
        let fabric = MemoryFabric::new(db.clone());
        let coherence = Coherence::new(db.clone(), backend.clone(), CoherenceConfig::default());
        let engine = EvictionEngine::new(
            fabric.clone(),
            coherence.clone(),
            Default::default(),
            counter.clone(),
        );
        let receipts = ReceiptLog::new(db);
        Ok(Self {
            fabric,
            engine,
            receipts,
            coherence,
            counter,
            backend,
            session_id: "scripted".into(),
            slot_id: "0".into(),
            context_window,
            turn: 0,
            receipts_seen: Vec::new(),
            evictions: Vec::new(),
        })
    }

    pub fn fabric(&self) -> &MemoryFabric {
        &self.fabric
    }

    pub fn receipts_log(&self) -> &ReceiptLog {
        &self.receipts
    }

    pub fn backend(&self) -> &Arc<EmbeddedBackend> {
        &self.backend
    }

    /// Run a whole script, returning the receipts it produced.
    pub async fn run(mut self, steps: &[Step]) -> sakur4_core::error::Result<Vec<Receipt>> {
        for step in steps {
            self.step(step).await?;
        }
        Ok(self.receipts_seen)
    }

    /// Execute one step.
    pub async fn step(&mut self, step: &Step) -> sakur4_core::error::Result<()> {
        self.turn += 1;
        match step {
            Step::UserTurn { text, tokens } => {
                let body = if text.is_empty() {
                    "x".repeat(tokens * 4)
                } else {
                    format!(
                        "{text} {}",
                        "y".repeat(tokens.saturating_mul(4).saturating_sub(text.len()))
                    )
                };
                self.fabric
                    .commit_episode(
                        NewEpisode::user(&self.session_id, body).with_slot(&self.slot_id),
                        &self.counter,
                        false,
                        false,
                    )
                    .await?;
                self.backend.advance_by(*tokens as i64);
            }
            Step::ToolResult { tool, tokens } => {
                self.fabric
                    .commit_episode(
                        NewEpisode::tool_result(&self.session_id, tool, "z".repeat(tokens * 4))
                            .with_slot(&self.slot_id),
                        &self.counter,
                        true,
                        false,
                    )
                    .await?;
                self.backend.advance_by(*tokens as i64);
            }
            Step::AssistantTurn { text } => {
                self.fabric
                    .commit_episode(
                        NewEpisode::assistant(&self.session_id, text.clone())
                            .with_slot(&self.slot_id),
                        &self.counter,
                        false,
                        false,
                    )
                    .await?;
                self.backend.advance_by(self.counter.count(text).get() as i64);
            }
            Step::Pin { kind, text } => {
                self.fabric
                    .pin(PinRequest::new(*kind, text.clone()).in_session(&self.session_id))
                    .await?;
            }
            Step::Receipt => {
                let receipt = self.emit_receipt().await?;
                self.receipts_seen.push(receipt);
            }
        }
        Ok(())
    }

    /// Assemble, plan, apply, and record a receipt for the current state.
    pub async fn emit_receipt(&mut self) -> sakur4_core::error::Result<Receipt> {
        let parts = self.assemble_parts(None).await?;
        let plan =
            self.engine.plan(&self.session_id, &self.slot_id, self.context_window, &parts).await?;

        let mut eviction_summary = None;
        if plan.pressure == sakur4_core::evict::Pressure::Compacting && !plan.is_empty() {
            let outcome = self.engine.apply(&plan, &parts).await?;
            let summary = EvictionSummary::from_plan(&plan, outcome.snapshot_taken);
            eviction_summary = Some(summary.clone());
            self.evictions.push(summary);
        }

        let after = self.assemble_parts(None).await?;
        let cache = self
            .coherence
            .observe_prompt(&self.session_id, &self.slot_id, &after.render(), &self.counter)
            .await?;

        let mut receipt = Receipt::build(
            &self.session_id,
            Some(&self.slot_id),
            self.turn,
            &after,
            &self.counter,
            self.context_window,
        )
        .with_cache(cache.cache_status.as_str(), cache.detail.clone())
        .with_cache_numbers(cache.reused_tokens, cache.prefilled_tokens)
        .with_backend(sakur4_core::llama::InferenceBackend::name(self.backend.as_ref()));
        if let Some(ms) = cache.prompt_eval_ms {
            receipt = receipt.with_prompt_eval_ms(ms);
        }
        if let Some(e) = eviction_summary {
            receipt = receipt.with_eviction(e);
        }
        self.receipts.record(&receipt).await?;
        Ok(receipt)
    }

    /// Build the prompt for the current state, optionally supplying extras.
    pub async fn assemble_parts(
        &self,
        extras: Option<(String, String, String)>,
    ) -> sakur4_core::error::Result<PromptParts> {
        let anchors = self.fabric.anchors(Some(&self.session_id)).await?;
        let anchor_block = anchors.iter().map(|a| a.render()).collect::<Vec<_>>().join("\n");
        let timeline =
            self.fabric.timeline(&self.session_id, 10_000_000, &self.counter, false).await?;
        let mut parts = PromptParts::new()
            .with_system("You are a local coding agent using Sakur4 memory.")
            .with_anchors(anchor_block)
            .with_timeline(timeline.rendered);
        if let Some((recall, repo_map, folds)) = extras {
            parts = parts.with_recall(recall).with_repo_map(repo_map).with_folds(folds);
        }
        Ok(parts)
    }

    /// The current live token count as the engine sees it.
    pub async fn live_tokens(&self) -> sakur4_core::error::Result<usize> {
        let parts = self.assemble_parts(None).await?;
        Ok(parts.count(&self.counter))
    }

    /// Session id.
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// The tokenizer in use.
    pub fn counter(&self) -> &TokenCounter {
        &self.counter
    }

    /// The eviction policy in force.
    pub fn policy(&self) -> &sakur4_core::evict::EvictionPolicy {
        self.engine.policy()
    }

    /// Count episodes by role.
    pub async fn role_counts(&self) -> sakur4_core::error::Result<Vec<(String, usize)>> {
        let eps = self.fabric.session_episodes(&self.session_id).await?;
        let mut map: std::collections::BTreeMap<String, usize> = Default::default();
        for e in eps {
            *map.entry(e.role).or_insert(0) += 1;
        }
        Ok(map.into_iter().collect())
    }

    /// Every anchor currently pinned.
    pub async fn anchors(&self) -> sakur4_core::error::Result<Vec<String>> {
        Ok(self
            .fabric
            .anchors(Some(&self.session_id))
            .await?
            .into_iter()
            .map(|a| a.content)
            .collect())
    }
}

/// Build a long script of the shape the PRD's endurance benchmark describes:
/// many sequential small tasks, each with a user turn, a tool result and an
/// assistant turn, punctuated by receipts.
pub fn endurance_script(tasks: usize, turn_tokens: usize, tool_tokens: usize) -> Vec<Step> {
    let mut steps = vec![
        Step::Pin {
            kind: AnchorKind::SafetyConstraint,
            text: "Never force-push to main; never delete the migrations directory.".into(),
        },
        Step::Pin {
            kind: AnchorKind::TaskContract,
            text: "Every change must keep the public API of src/auth.rs backward compatible."
                .into(),
        },
    ];
    for i in 0..tasks {
        steps.push(Step::UserTurn {
            text: format!(
                "Task {i}: update the validate() call path and keep the session table consistent."
            ),
            tokens: turn_tokens,
        });
        steps.push(Step::ToolResult { tool: "read_file".into(), tokens: tool_tokens });
        steps.push(Step::AssistantTurn {
            text: format!("Applied task {i}: edited src/auth.rs and left the API unchanged."),
        });
        // A receipt every few tasks, which is where compaction actually happens.
        if i % 3 == 2 {
            steps.push(Step::Receipt);
        }
    }
    steps.push(Step::Receipt);
    steps
}

/// A short script that deliberately crosses the compaction threshold once.
pub fn single_compaction_script(turn_tokens: usize) -> Vec<Step> {
    let mut steps = vec![Step::Pin {
        kind: AnchorKind::SafetyConstraint,
        text: "Do not touch the production database.".into(),
    }];
    for i in 0..24 {
        steps.push(Step::UserTurn {
            text: format!("Turn {i} of the working session."),
            tokens: turn_tokens,
        });
        steps.push(Step::ToolResult { tool: "grep".into(), tokens: turn_tokens * 2 });
    }
    steps.push(Step::Receipt);
    steps
}

/// Unused-import guard: `Role` is part of the public step vocabulary.
pub fn roles_used() -> [Role; 3] {
    [Role::User, Role::Assistant, Role::Tool]
}
