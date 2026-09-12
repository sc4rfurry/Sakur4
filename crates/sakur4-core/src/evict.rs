//! The Graduated Eviction Engine (PRD component C2).
//!
//! # What this replaces
//!
//! Default harness compaction asks a model to summarise the transcript and hopes
//! the result is faithful. The PRD's problem statement lists four ways that goes
//! wrong: unpredictable lossiness, structural destruction, blocking cost, and
//! compression-induced hallucination. Addressable as one design:
//!
//! * **Deterministic.** The eviction plan is computed from token counts, recency,
//!   graph in-degree and explicit droppability. No model is consulted, so the
//!   plan is reproducible and auditable (NFR-13).
//! * **Structural, not textual.** Escalating an episode changes how it is
//!   *rendered*, never what is stored. Structural destruction is impossible
//!   because nothing is destroyed.
//! * **Cheap.** Planning is arithmetic over an in-memory list; it never blocks on
//!   generation, so it cannot be the reason a turn is slow.
//! * **Fabrication-free by construction.** The engine has no mechanism to produce
//!   text, so it cannot invent any.
//!
//! # The eviction shape is not arbitrary
//!
//! Naive compaction evicts from the oldest end, which changes the prompt's *first*
//! token and therefore throws away the entire KV prefix. Sakur4's default shape is
//! a **prefix-preserving middle-out**: the oldest `keep_prefix_tokens` stay
//! verbatim (they are the pinned context and the cached prefix), the newest
//! `keep_recent_tokens` stay verbatim (recency is the strongest relevance signal
//! available without a model), and pressure is absorbed by escalating the *middle*.
//!
//! That choice is what makes FR-7 coherent: a boundary the cache can snap to only
//! exists if there is a prefix worth preserving.

use std::collections::HashMap;

use crate::cache::{BoundaryPlan, CacheStatus, Coherence};
use crate::llama::snap_to_checkpoint;
use crate::error::{Error, Result};
use crate::ids::new_id;
use crate::memory::dependency::{EdgeKind, NodeRef};
use crate::memory::episodic::{EpisodeRow, EpisodeTier};
use crate::memory::fabric::MemoryFabric;
use crate::prompt::PromptParts;
use crate::tokens::TokenCounter;

/// Eviction policy knobs.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct EvictionPolicy {
    /// Fraction of the context window at which planning starts (FR-5's
    /// "configured threshold"). 0.75 rather than 0.9 because context rot degrades
    /// reliability well before the window is full (PP-3), and a local 27-35B model
    /// starts from a lower long-context baseline than a frontier model.
    pub trigger_ratio: f64,
    /// Fraction to compact down to, leaving headroom for tool results mid-turn.
    pub target_ratio: f64,
    /// Tokens of the oldest context to keep verbatim regardless of pressure.
    pub keep_prefix_tokens: usize,
    /// Minimum prefix to preserve *for cache reuse* (FR-7).
    ///
    /// # Why a separate knob from `keep_prefix_tokens`
    ///
    /// That one is about information value — how much of the session's opening is
    /// worth keeping in the window. This one is about *cache coherence*: with no
    /// prefix floor, a greedy engine escalates the oldest turns first, the eviction
    /// boundary lands at token ~0, and there is no surviving prefix for a
    /// checkpoint to align to. Every compaction then reports a full re-prefill —
    /// which is precisely the failure the project exists to remove, arrived at by
    /// Sakur4's own machinery.
    ///
    /// The floor is also raised to meet whichever checkpoint the slot already
    /// holds, so two consecutive compactions agree about where the boundary is
    /// instead of each cutting whichever side of the checkpoint it happens to
    /// prefer. An engine that oscillates across a cache boundary re-prefills every
    /// turn.
    pub cache_prefix_reserve_tokens: usize,
    /// Tokens of the newest context to keep verbatim.
    pub keep_recent_tokens: usize,
    /// Upper bound on the preserved prefix, as a fraction of the live window.
    ///
    /// A prefix larger than this leaves nothing in the middle to evict, so the
    /// plan would announce "nothing to do" while the window overflowed.
    pub max_prefix_ratio: f64,
    /// Absolute ceiling on the preserved prefix, in tokens.
    ///
    /// This is the knob that decides whether a compaction reuses the cache at all.
    /// The reserve above is where Sakur4 would *like* the boundary to fall;
    /// whether it can is decided by the server's checkpoint ring, because a ring
    /// only spans `interval × depth` tokens. A ring holding 8,448 tokens of
    /// history has no checkpoint near a 1,024-token boundary, so a boundary that
    /// strict cannot be aligned and the compaction reports a full re-prefill.
    ///
    /// Raising the ceiling lets the boundary move forward to meet the oldest
    /// checkpoint the ring still holds. The trade is explicit — more tokens kept
    /// verbatim in exchange for reusing the entire cached prefix — and it is the
    /// right side of the trade whenever the window has room, which is why the
    /// ratio above still applies as an independent bound.
    pub cache_prefix_max_tokens: usize,
    /// Permit the `Drop` tier. Off means the floor is `Archived`, which is the
    /// conservative default: dropping is only ever legitimate for output the
    /// producer itself declared droppable.
    pub allow_drop: bool,
    /// Refuse to escalate a single episode by more than one tier per plan, so a
    /// large episode cannot jump straight to archived on its first eviction.
    pub max_tier_step: u8,
    /// Ask the Cache-Coherence Layer where the boundary should actually fall.
    pub cache_align: bool,
}

impl Default for EvictionPolicy {
    fn default() -> Self {
        Self {
            trigger_ratio: 0.75,
            target_ratio: 0.55,
            keep_prefix_tokens: 0,
            // Where Sakur4 would like the compaction boundary to fall.
            //
            // The value is a trade, not a target: a larger prefix survives verbatim
            // (costing window space, and costing it on every future turn) in
            // exchange for a larger cached prefix the server can keep. Below roughly
            // one checkpoint interval the boundary lands short of the first ring
            // entry and the reuse is nominal; well above the ceiling in
            // `cache_prefix_max_tokens` the plan stops being able to evict anything.
            // 4096 sits between the two for the windows the PRD targets, and both
            // bounds are enforced independently.
            cache_prefix_reserve_tokens: 4096,
            keep_recent_tokens: 4096,
            max_prefix_ratio: 0.35,
            cache_prefix_max_tokens: 8192,
            allow_drop: false,
            max_tier_step: 1,
            cache_align: true,
        }
    }
}

/// How much pressure exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Pressure {
    /// Comfortably inside the window.
    Relaxed,
    /// Past the trigger threshold; a plan should be produced now.
    Compacting,
    /// The Anchor Set alone exceeds the budget; no plan can help.
    AnchorOverflow,
}

/// One candidate for eviction, with its deterministic score.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Candidate {
    pub episode_id: String,
    pub seq: i64,
    pub role: String,
    pub current_tier: EpisodeTier,
    pub tokens: usize,
    /// Deterministic value score. Higher = keep.
    pub value: f64,
    /// How many other nodes depend on this episode.
    pub dependents: usize,
    pub droppable: bool,
    pub in_fold: bool,
    /// Why this episode's score is what it is.
    pub notes: Vec<String>,
}

/// One planned tier change.
#[derive(Debug, Clone, serde::Serialize)]
pub struct TierUpdate {
    pub episode_id: String,
    pub from: EpisodeTier,
    pub to: EpisodeTier,
    pub tokens_before: usize,
    pub tokens_after: usize,
    pub reason: String,
}

/// The plan.
#[derive(Debug, Clone, serde::Serialize)]
pub struct EvictionPlan {
    pub session_id: String,
    pub slot_id: String,
    pub pressure: Pressure,
    /// Context window in tokens.
    pub budget: usize,
    /// The threshold that triggered planning.
    pub threshold: usize,
    /// Where the caller wants to land.
    pub target: usize,
    pub live_tokens: usize,
    /// Tokens held verbatim by the Anchor Set.
    pub anchor_tokens: usize,
    /// Tokens of non-evictable prompt parts (system, repo map, tool schemas).
    pub fixed_tokens: usize,
    pub updates: Vec<TierUpdate>,
    pub planned_savings: usize,
    /// The boundary in rendered-timeline tokens below which context survives.
    pub retained_prefix_tokens: usize,
    /// What the cache layer said about that boundary.
    pub coherence: Option<BoundaryPlan>,
    /// Human-readable explanation, one line per decision class.
    pub notes: Vec<String>,
}

impl EvictionPlan {
    pub fn is_empty(&self) -> bool {
        self.updates.is_empty()
    }

    pub fn token_after_plan(&self) -> usize {
        self.live_tokens.saturating_sub(self.planned_savings)
    }

    /// Whether the plan fits inside the target.
    pub fn reaches_target(&self) -> bool {
        self.token_after_plan() + self.anchor_tokens + self.fixed_tokens <= self.target
    }

    /// Whether the plan resolved to a cache-reuse boundary (G1's metric).
    pub fn is_cache_cheap(&self) -> bool {
        self.coherence.as_ref().map(|c| c.is_reuse()).unwrap_or(false)
    }

    pub fn summary(&self) -> String {
        let cache = match &self.coherence {
            Some(c) => format!("{} — {}", c.status.headline(), c.reason),
            None => "cache alignment not evaluated".into(),
        };
        format!(
            "{} episode(s), {} tokens reclaimed ({} → {} of {} target). {}",
            self.updates.len(),
            self.planned_savings,
            self.live_tokens + self.anchor_tokens + self.fixed_tokens,
            self.token_after_plan() + self.anchor_tokens + self.fixed_tokens,
            self.target,
            cache
        )
    }
}

/// Outcome of executing a plan.
#[derive(Debug, Clone, serde::Serialize)]
pub struct EvictionOutcome {
    pub applied: usize,
    pub tokens_reclaimed: usize,
    pub snapshot_taken: bool,
    pub cache_status: CacheStatus,
    pub detail: String,
}

/// Result of `fold`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct FoldOutcome {
    pub fold_id: String,
    pub checkpoint: Option<String>,
    pub token_position: i64,
    pub tokens_at_open: usize,
    pub cache_note: String,
}

/// Result of `unfold`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct UnfoldOutcome {
    pub fold_id: String,
    pub tokens_reclaimed: usize,
    pub episodes_folded: usize,
    pub rollback_performed: bool,
    pub rollback_detail: String,
    pub trace_retrievable: bool,
}

/// The engine.
#[derive(Clone)]
pub struct EvictionEngine {
    fabric: MemoryFabric,
    coherence: Coherence,
    policy: EvictionPolicy,
    counter: TokenCounter,
}

impl EvictionEngine {
    pub fn new(
        fabric: MemoryFabric,
        coherence: Coherence,
        policy: EvictionPolicy,
        counter: TokenCounter,
    ) -> Self {
        Self {
            fabric,
            coherence,
            policy,
            counter,
        }
    }

    pub fn policy(&self) -> &EvictionPolicy {
        &self.policy
    }

    pub fn coherence(&self) -> &Coherence {
        &self.coherence
    }

    /// The threshold at which planning should start for a given window.
    pub fn threshold_for(&self, context_window: usize) -> usize {
        ((context_window as f64) * self.policy.trigger_ratio).round() as usize
    }

    /// The size the engine aims to reach.
    pub fn target_for(&self, context_window: usize) -> usize {
        ((context_window as f64) * self.policy.target_ratio).round() as usize
    }

    /// Assess pressure without planning.
    pub async fn assess(
        &self,
        session_id: &str,
        context_window: usize,
        parts: &PromptParts,
    ) -> Result<Pressure> {
        let anchor_tokens = self.anchor_tokens(session_id).await?;
        let fixed = parts.fixed_tokens(&self.counter);
        if anchor_tokens + fixed >= context_window {
            return Ok(Pressure::AnchorOverflow);
        }
        let live = parts.total_tokens(&self.counter) + anchor_tokens;
        Ok(if live >= self.threshold_for(context_window) {
            Pressure::Compacting
        } else {
            Pressure::Relaxed
        })
    }

    /// Score every evictable episode and produce a plan.
    ///
    /// `parts` is the prompt as it would be assembled right now; the engine needs
    /// it because the budget question is about the *whole* prompt, not just the
    /// transcript.
    pub async fn plan(
        &self,
        session_id: &str,
        slot_id: &str,
        context_window: usize,
        parts: &PromptParts,
    ) -> Result<EvictionPlan> {
        let threshold = self.threshold_for(context_window);
        let target = self.target_for(context_window);

        let anchor_tokens = self.anchor_tokens(session_id).await?;
        let fixed_tokens = parts.fixed_tokens(&self.counter);
        let timeline_tokens = parts.timeline_tokens(&self.counter);

        let episodes = self.fabric.evictable_episodes(session_id).await?;
        let live_tokens = timeline_tokens;

        let total = live_tokens + anchor_tokens + fixed_tokens;
        if total < threshold {
            return Ok(EvictionPlan {
                session_id: session_id.into(),
                slot_id: slot_id.into(),
                pressure: Pressure::Relaxed,
                budget: context_window,
                threshold,
                target,
                live_tokens,
                anchor_tokens,
                fixed_tokens,
                updates: Vec::new(),
                planned_savings: 0,
                retained_prefix_tokens: live_tokens,
                coherence: None,
                notes: vec![format!(
                    "context is at {total} tokens, below the {threshold}-token trigger; nothing to do"
                )],
            });
        }

        if anchor_tokens + fixed_tokens >= context_window {
            return Err(Error::BudgetOverflow(format!(
                "the Anchor Set plus fixed prompt parts need {} tokens of a {}-token window; \
                 there is no room for any history. Sakur4 will not silently drop a pinned \
                 constraint (FR-4).",
                anchor_tokens + fixed_tokens, context_window
            )));
        }

        // --- candidate scoring ---------------------------------------------
        let mut candidates = self.score_candidates(session_id, &episodes).await?;

        // --- prefix/recent preservation -------------------------------------
        // Walk from the oldest end while we are still inside the prefix budget;
        // those episodes are never candidates. Keeping a prefix is what puts the
        // eviction boundary in the *middle*, which is the only place a cache
        // checkpoint can be snapped to — an eviction that starts at token 0
        // changes the prompt's first token and guarantees a full re-prefill.
        //
        // The budget is the larger of the informational prefix and the
        // cache-reuse reserve, and it is lifted to meet whichever checkpoint the
        // slot already holds *that is small enough to be worth preserving*.
        //
        // # Why the clamp matters
        //
        // --- where the boundary can fall ------------------------------------
        //
        // Asked before choosing evictions, because the answer determines which
        // episodes are even candidates. See `boundary_and_prefix`: doing this the
        // other way round is how a cache-coherent eviction engine ends up never
        // reusing a cache.
        let mut notes: Vec<String> = Vec::new();
        let (coherence, prefix_budget) = if self.policy.cache_align {
            let (plan, prefix) = self
                .boundary_and_prefix(session_id, slot_id, live_tokens)
                .await;
            if let Some(p) = &plan {
                notes.push(p.summary());
            }
            (plan, prefix)
        } else {
            (
                None,
                self.policy
                    .keep_prefix_tokens
                    .max(self.policy.cache_prefix_reserve_tokens)
                    .min((live_tokens as f64 * self.policy.max_prefix_ratio) as usize),            )
        };

        let prefix_end = Self::prefix_end_index(&episodes, prefix_budget);
        let recent_start = Self::recent_start_index(&episodes, self.policy.keep_recent_tokens);

        // Keep only what sits strictly between the preserved prefix and the
        // preserved recent window: that gap is the evictable middle.
        let prefix_boundary_seq: Option<i64> = prefix_end
            .checked_sub(1)
            .and_then(|i| episodes.get(i))
            .map(|e| e.seq);
        let recent_boundary_seq: Option<i64> = episodes.get(recent_start).map(|e| e.seq);

        let mut kept: Vec<Candidate> = Vec::with_capacity(candidates.len());
        for c in candidates.drain(..) {
            let after_prefix = prefix_boundary_seq.map(|s| c.seq > s).unwrap_or(true);
            let before_recent = recent_boundary_seq.map(|s| c.seq < s).unwrap_or(false);
            if after_prefix && before_recent {
                if c.in_fold {
                    notes.push(format!(
                        "episode {} is inside an open fold and is managed by unfold(), not by the \
                         automatic engine",
                        c.episode_id
                    ));
                } else {
                    kept.push(c);
                }
            }
        }
        candidates = kept;

        // Prefer the lowest-value episodes; tie-break on older seq so behaviour is
        // deterministic across runs.
        candidates.sort_by(|a, b| {
            a.value
                .partial_cmp(&b.value)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.seq.cmp(&b.seq))
        });

        // --- choose escalations --------------------------------------------
        let needed = total.saturating_sub(target);
        let mut updates: Vec<TierUpdate> = Vec::new();
        let mut savings = 0usize;
        let mut selected: Vec<&Candidate> = Vec::new();

        for c in &candidates {
            if savings >= needed {
                break;
            }
            let Some(update) = self.propose_escalation(c, needed - savings).await? else {
                continue;
            };
            savings += update.tokens_before.saturating_sub(update.tokens_after);
            updates.push(update);
            selected.push(c);
        }

        if updates.is_empty() && needed > 0 {
            notes.push(format!(
                "no episode could be escalated even though {needed} tokens of pressure exist; \
                 every candidate is either at the tier floor or protected by an unresolved dependency"
            ));
        }

        // --- cache-coherent boundary ----------------------------------------
        // The retained prefix is measured from the head until the first episode the
        // plan touched — using the *same* walk that decided which episodes were
        // evictable, then reported back into the plan so the boundary the cache
        // layer is told about describes the prompt that will actually be sent.
        //
        // Proposal and outcome can differ, and the outcome is what counts: episodes
        // are the unit of eviction, so the preserved prefix ends where an episode
        // ends, which is rarely exactly where a checkpoint sits. Recording the
        // proposal would put a number in the receipt that no prompt ever had.
        let retained_prefix_tokens = self.retained_prefix_after(&episodes, &updates);

        let mut coherence = coherence;
        if let Some(plan) = coherence.as_mut() {
            // A preserved prefix only counts as reuse where the server can actually
            // match it. When no boundary was alignable — no backend, no checkpoints,
            // or partial-state-only checkpoints — the prefix is still preserved and
            // still a prefix of the next prompt, but reporting `partial-reuse` would
            // put a number on G1's headline metric that nothing supports.
            if retained_prefix_tokens > 0 && plan.pairs_with_cache {
                if !plan.is_reuse() {
                    // The boundary landed between the proposal and the oldest
                    // checkpoint — normal, because a ring only spans
                    // `interval × depth` tokens. The prefix up to the episode
                    // boundary is still preserved verbatim and still a prefix of
                    // what the slot holds, so partial reuse is the accurate verdict.
                    plan.aligned_cut = Some(retained_prefix_tokens as i64);
                    plan.status = CacheStatus::PartialReuse;
                    plan.snap = None;
                    plan.reason = format!(
                        "the first {retained_prefix_tokens} tokens are preserved verbatim, which is \
                         the earliest boundary the episode granularity allows; the next prompt \
                         shares that head, so only the suffix is prefilled"
                    );
                } else if plan.aligned_cut.unwrap_or(0) > retained_prefix_tokens as i64 {
                    // A checkpoint hit only counts if the preserved prefix actually
                    // reaches it. When the episode boundary falls short, the honest
                    // cut is the shorter one.
                    let checkpoint_at = plan.aligned_cut.unwrap_or(0);
                    plan.aligned_cut = Some(retained_prefix_tokens as i64);
                    plan.reason = format!(
                        "a checkpoint sits at {checkpoint_at} but the preserved prefix ends at \
                         {retained_prefix_tokens} tokens; the boundary is reported at the shorter \
                         position, which is what the next prompt will actually share"
                    );
                }
            }
            notes.push(plan.summary());
            if plan.is_reuse() && !updates.is_empty() {
                notes.push(format!(
                    "the boundary is preserved verbatim at {retained_prefix_tokens} tokens, so the \
                     next prompt shares its head with the cached one"
                ));
            }
        }

        Ok(EvictionPlan {
            session_id: session_id.into(),
            slot_id: slot_id.into(),
            pressure: Pressure::Compacting,
            budget: context_window,
            threshold,
            target,
            live_tokens,
            anchor_tokens,
            fixed_tokens,
            updates,
            planned_savings: savings,
            retained_prefix_tokens,
            coherence,
            notes,
        })
    }

    /// Execute a plan: snapshot if the cache layer asks for one, then apply tiers.
    pub async fn apply(&self, plan: &EvictionPlan, parts: &PromptParts) -> Result<EvictionOutcome> {
        let mut snapshot_taken = false;
        let cache_status = plan
            .coherence
            .as_ref()
            .map(|c| c.status)
            .unwrap_or(CacheStatus::Unknown);

        if let Some(coherence) = &plan.coherence
            && coherence.wants_snapshot() {
                let cp = self
                    .coherence
                    .pre_rewrite_snapshot(&plan.session_id, &plan.slot_id, &coherence.reason)
                    .await?;
                snapshot_taken = cp.is_some();
            }

        let updates: Vec<(String, EpisodeTier)> = plan
            .updates
            .iter()
            .map(|u| (u.episode_id.clone(), u.to))
            .collect();
        let applied = self.fabric.set_tiers(updates).await?;

        // Tell the cache layer what the surviving prefix is, so the next turn's
        // reuse estimate is computed from the prompt that will actually be sent.
        if let Some(coherence) = &plan.coherence {
            let retained = parts.retained_prefix_text(plan.retained_prefix_tokens, &self.counter);
            self.coherence
                .record_plan(&plan.session_id, &plan.slot_id, coherence, &retained, &self.counter)
                .await?;
        }

        // And tell the backend, so a simulating backend's cache state matches the
        // prompt that will actually be sent next.
        let _ = self
            .coherence
            .backend()
            .note_compaction(&plan.slot_id, plan.retained_prefix_tokens as i64)
            .await;

        Ok(EvictionOutcome {
            applied,
            tokens_reclaimed: plan.planned_savings,
            snapshot_taken,
            cache_status,
            detail: plan.summary(),
        })
    }

    /// Plan and apply in one call, when pressure warrants it.
    pub async fn compact_if_needed(
        &self,
        session_id: &str,
        slot_id: &str,
        context_window: usize,
        parts: &PromptParts,
    ) -> Result<Option<(EvictionPlan, EvictionOutcome)>> {
        let plan = self
            .plan(session_id, slot_id, context_window, parts)
            .await?;
        if plan.pressure == Pressure::Relaxed || plan.is_empty() {
            return Ok(None);
        }
        let outcome = self.apply(&plan, parts).await?;
        Ok(Some((plan, outcome)))
    }

    // -----------------------------------------------------------------------
    // fold / unfold (FR-6)
    // -----------------------------------------------------------------------

    /// Open a fold: an isolated sub-context for a token-intensive subtask.
    ///
    /// The fold records where in the token stream it opened, which becomes a
    /// checkpoint boundary the cache layer can roll back to on `unfold` (FR-6's
    /// "new llama.cpp save point"). When the backend cannot take a save point, the
    /// fold still works — the rollback simply reports that it could not be done,
    /// which is more useful than refusing the operation.
    pub async fn fold(
        &self,
        session_id: &str,
        slot_id: &str,
        description: &str,
        goal: &str,
    ) -> Result<FoldOutcome> {
        if description.trim().is_empty() {
            return Err(Error::Invalid("fold() requires a description".into()));
        }
        let fold_id = new_id("fold");
        let tokens_at_open = self
            .fabric
            .session_live_tokens(session_id, &self.counter)
            .await
            .unwrap_or(0);

        // Prefer a durable save point at open: it is what makes the rollback
        // possible even if the ring wraps during a long folded subtask.
        let checkpoint = self
            .coherence
            .snapshot(session_id, slot_id)
            .await
            .ok()
            .and_then(|o| o.file_path);

        let token_position = self
            .coherence
            .slot_state(slot_id)
            .await
            .map(|s| s.n_past)
            .unwrap_or(tokens_at_open as i64);

        let cache_note = match &checkpoint {
            Some(path) => format!(
                "save point {path} recorded at token {token_position}; unfold() will roll the slot \
                 back to it"
            ),
            None => format!(
                "backend took no save point; unfold() will rely on the in-memory ring at token \
                 {token_position}, or report that no rollback was possible"
            ),
        };

        self.coherence
            .mark_fold_open(session_id, slot_id, &fold_id, token_position)
            .await?;

        self.fabric
            .db()
            .write({
                let fold = fold_id.clone();
                let session = session_id.to_string();
                let slot = slot_id.to_string();
                let description = description.to_string();
                let goal = goal.to_string();
                let checkpoint = checkpoint.clone();
                let now = crate::ids::now_rfc3339();
                move |tx| {
                    tx.execute(
                        "INSERT INTO folds
                            (fold_id, session_id, slot_id, description, goal, status,
                             opened_checkpoint_id, opened_token_position, tokens_at_open, created_at)
                         VALUES (?1, ?2, ?3, ?4, ?5, 'open', ?6, ?7, ?8, ?9)",
                        rusqlite::params![
                            fold,
                            session,
                            slot,
                            description,
                            goal,
                            checkpoint,
                            token_position,
                            tokens_at_open as i64,
                            now
                        ],
                    )?;
                    Ok(())
                }
            })
            .await?;

        // Record the fold as a memory node so episodes can hang off it.
        self.fabric
            .add_edge(crate::memory::dependency::EdgeRow::new(
                &NodeRef::new(crate::memory::dependency::NodeKind::Fold, &fold_id),
                &NodeRef::new(crate::memory::dependency::NodeKind::Project, session_id),
                EdgeKind::DependsOn,
            ))
            .await?;

        Ok(FoldOutcome {
            fold_id,
            checkpoint,
            token_position,
            tokens_at_open,
            cache_note,
        })
    }

    /// Attach an episode to a fold.
    pub async fn tag_episode_with_fold(&self, episode_id: &str, fold_id: &str) -> Result<()> {
        self.fabric
            .db()
            .write({
                let ep = episode_id.to_string();
                let fold = fold_id.to_string();
                move |tx| {
                    let n = tx.execute(
                        "UPDATE episodic_stream SET fold_id = ?2 WHERE episode_id = ?1",
                        rusqlite::params![ep, fold],
                    )?;
                    if n == 0 {
                        return Err(Error::NotFound(format!("episode {ep}")));
                    }
                    Ok(())
                }
            })
            .await?;
        self.fabric
            .add_edge(crate::memory::dependency::EdgeRow::new(
                &NodeRef::episode(episode_id),
                &NodeRef::new(crate::memory::dependency::NodeKind::Fold, fold_id),
                EdgeKind::FoldedFrom,
            ))
            .await?;
        Ok(())
    }

    /// Collapse a fold, retaining only the summary in the live window.
    pub async fn unfold(
        &self,
        session_id: &str,
        slot_id: &str,
        fold_id: &str,
        result_summary: &str,
    ) -> Result<UnfoldOutcome> {
        let summary = result_summary.trim();
        if summary.is_empty() {
            return Err(Error::Invalid(
                "unfold() requires a result_summary: the whole point is that the folded trace \
                 collapses to a result, and an empty result would lose all of it"
                    .into(),
            ));
        }

        let (status, token_position, description, goal): (String, i64, String, String) = self
            .fabric
            .db()
            .with({
                let fold = fold_id.to_string();
                move |c| {
                    Ok(c.query_row(
                        "SELECT status, COALESCE(opened_token_position, 0), description, goal
                         FROM folds WHERE fold_id = ?1",
                        [fold],
                        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                    )?)
                }
            })
            .await
            .map_err(|_| Error::NotFound(format!("fold {fold_id}")))?;

        if status == "closed" {
            return Err(Error::Invalid(format!("fold {fold_id} is already closed")));
        }

        let episodes = self.fabric.episodes_in_fold(fold_id).await?;
        let tokens_before: usize = episodes.iter().map(|e| e.token_count as usize).sum();

        // Collapse: every episode inside the fold becomes `Referenced`. The text
        // stays in the store (round-trip integrity), the live window drops it.
        let updates: Vec<(String, EpisodeTier)> = episodes
            .iter()
            .map(|e| (e.episode_id.clone(), EpisodeTier::Referenced))
            .collect();
        self.fabric.set_tiers(updates).await?;

        // Put the result summary into the stream as one episode, so the collapsed
        // result is itself part of the transcript and recallable like any other.
        let summary_episode = self
            .fabric
            .commit_episode(
                crate::memory::episodic::NewEpisode {
                    session_id: session_id.to_string(),
                    slot_id: Some(slot_id.to_string()),
                    role: crate::memory::episodic::Role::Internal,
                    content: format!("[fold {fold_id} · {description}] result: {summary}"),
                    tool_name: None,
                    fold_id: None,
                    droppable: false,
                    meta: Some(serde_json::json!({
                        "fold_id": fold_id,
                        "goal": goal,
                        "folded_episodes": episodes.len(),
                        "folded_tokens": tokens_before,
                    })),
                },
                &self.counter,
                false,
                false,
            )
            .await?;

        // Anchor a Semantic Atlas entry to the fold's own marker so the summary is
        // tracked for staleness like any other derived interpretation.
        let summary_tokens = self.counter.count(summary).get();
        let tokens_reclaimed = tokens_before.saturating_sub(summary_tokens);

        let rollback = self
            .coherence
            .roll_back_to(session_id, slot_id, token_position)
            .await
            .unwrap_or(crate::cache::RollBackOutcome {
                performed: false,
                method: crate::cache::RollBackMethod::None,
                detail: "rollback could not be attempted".into(),
            });

        self.fabric
            .db()
            .write({
                let fold = fold_id.to_string();
                let summary = summary.to_string();
                let now = crate::ids::now_rfc3339();
                let reclaimed = tokens_reclaimed as i64;
                let ep = summary_episode.episode_id.clone();
                move |tx| {
                    tx.execute(
                        "UPDATE folds SET status='closed', result_summary=?2,
                                          closed_checkpoint_id=?3, tokens_reclaimed=?4, closed_at=?5
                         WHERE fold_id = ?1",
                        rusqlite::params![fold, summary, ep, reclaimed, now],
                    )?;
                    Ok(())
                }
            })
            .await?;

        Ok(UnfoldOutcome {
            fold_id: fold_id.to_string(),
            tokens_reclaimed,
            episodes_folded: episodes.len(),
            rollback_performed: rollback.performed,
            rollback_detail: rollback.detail,
            trace_retrievable: true,
        })
    }

    /// The full trace of a folded subtask (FR-6's `recall_fold`).
    pub async fn recall_fold(&self, fold_id: &str) -> Result<FoldTrace> {
        let meta: (String, String, String, Option<String>, i64, i64, String, Option<String>) = self
            .fabric
            .db()
            .with({
                let fold = fold_id.to_string();
                move |c| {
                    Ok(c.query_row(
                        "SELECT description, goal, status, result_summary, tokens_at_open,
                                tokens_reclaimed, created_at, closed_at
                         FROM folds WHERE fold_id = ?1",
                        [fold],
                        |r| {
                            Ok((
                                r.get(0)?,
                                r.get(1)?,
                                r.get(2)?,
                                r.get(3)?,
                                r.get::<_, Option<i64>>(4)?.unwrap_or(0),
                                r.get::<_, Option<i64>>(5)?.unwrap_or(0),
                                r.get(6)?,
                                r.get(7)?,
                            ))
                        },
                    )?)
                }
            })
            .await
            .map_err(|_| Error::NotFound(format!("fold {fold_id}")))?;

        let episodes = self.fabric.episodes_in_fold(fold_id).await?;
        Ok(FoldTrace {
            fold_id: fold_id.to_string(),
            description: meta.0,
            goal: meta.1,
            status: meta.2,
            result_summary: meta.3,
            tokens_at_open: meta.4 as usize,
            tokens_reclaimed: meta.5 as usize,
            created_at: meta.6,
            closed_at: meta.7,
            episodes: episodes
                .into_iter()
                .map(|e| FoldTraceEntry {
                    episode_id: e.episode_id,
                    seq: e.seq,
                    role: e.role,
                    tool_name: e.tool_name,
                    token_count: e.token_count as usize,
                    content: e.content,
                })
                .collect(),
        })
    }

    /// Open folds for a session.
    pub async fn open_folds(&self, session_id: &str) -> Result<Vec<(String, String, String)>> {
        self.fabric
            .db()
            .with({
                let session = session_id.to_string();
                move |c| {
                    let mut stmt = c.prepare(
                        "SELECT fold_id, description, goal FROM folds
                         WHERE session_id = ?1 AND status='open' ORDER BY created_at",
                    )?;
                    let rows = stmt.query_map([session], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
                    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
                }
            })
            .await
    }

    // -----------------------------------------------------------------------
    // internals
    // -----------------------------------------------------------------------

    async fn anchor_tokens(&self, session_id: &str) -> Result<usize> {
        let anchors = self.fabric.anchors(Some(session_id)).await?;
        Ok(anchors.iter().map(|a| a.token_cost(&self.counter)).sum::<usize>()
            + if anchors.is_empty() { 0 } else { 24 })
    }

    /// Build the scored candidate list.
    async fn score_candidates(
        &self,
        _session_id: &str,
        episodes: &[EpisodeRow],
    ) -> Result<Vec<Candidate>> {
        // One graph load for the whole session, keyed by episode.
        let mut dependents: HashMap<String, usize> = HashMap::new();
        for ep in episodes {
            let node = NodeRef::episode(&ep.episode_id);
            let graph = self.fabric.graph_around(&node, 512).await?;
            dependents.insert(ep.episode_id.clone(), graph.dependents_on(&node, 4, None).len());
        }

        let newest = episodes.iter().map(|e| e.seq).max().unwrap_or(0);
        let oldest = episodes.iter().map(|e| e.seq).min().unwrap_or(0);
        let span = (newest - oldest).max(1) as f64;

        Ok(episodes
            .iter()
            .map(|ep| {
                let dependents = *dependents.get(&ep.episode_id).unwrap_or(&0);
                let mut notes = Vec::new();

                // Recency: normalised position in the session, weighted heavily
                // because it is the only relevance proxy available without a model.
                let recency = (ep.seq - oldest) as f64 / span;
                let mut value = recency * 6.0;

                // Role: a user turn states requirements; a tool result is
                // reproducible by re-running the tool.
                match ep.role.as_str() {
                    "user" => {
                        value += 2.0;
                        notes.push("user turn: statements of intent are expensive to lose".into());
                    }
                    "assistant" => value += 0.5,
                    "tool" => {
                        value -= 0.5;
                        notes.push("tool result: reproducible by re-running the tool".into());
                    }
                    "system" => {
                        value += 1.5;
                        notes.push("system turn".into());
                    }
                    _ => {}
                }

                // Structural dependencies are the strongest retention signal: if
                // something was derived from this episode, evicting it loses the
                // ground truth behind a live interpretation.
                value += (dependents as f64).min(6.0) * 1.5;
                if dependents > 0 {
                    notes.push(format!("{dependents} node(s) depend on this episode"));
                }

                // Size: evicting one huge tool result beats evicting ten small
                // turns for the same reclaimed tokens, and costs less information.
                let tokens = ep.token_count as usize;
                value -= ((tokens as f64) / 2048.0).min(2.0);

                if ep.superseded_by.is_some() {
                    value -= 3.0;
                    notes.push("superseded by a later episode".into());
                }
                if ep.droppable {
                    value -= 1.0;
                    notes.push("producer marked it droppable".into());
                }
                if ep.fold_id.is_some() {
                    value += 3.0;
                    notes.push("attached to a fold".into());
                }

                Candidate {
                    episode_id: ep.episode_id.clone(),
                    seq: ep.seq,
                    role: ep.role.clone(),
                    current_tier: ep.eviction_tier,
                    tokens,
                    value,
                    dependents,
                    droppable: ep.droppable,
                    in_fold: ep.fold_id.is_some(),
                    notes,
                }
            })
            .collect())
    }

    /// The next tier for a candidate, or `None` when it may not move.
    async fn propose_escalation(&self, c: &Candidate, still_needed: usize) -> Result<Option<TierUpdate>> {
        let Some(next) = c.current_tier.escalate() else {
            return Ok(None);
        };

        // FR-5: `Drop` only for explicitly droppable entries with no unresolved
        // dependents.
        if next == EpisodeTier::Dropped {
            if !self.policy.allow_drop {
                return Ok(None);
            }
            if !c.droppable {
                return Ok(None);
            }
            if c.dependents > 0 {
                return Ok(None);
            }
            let node = NodeRef::episode(&c.episode_id);
            let graph = self.fabric.graph_around(&node, 256).await?;
            if graph.has_dependents(&node) {
                return Ok(None);
            }
        }

        // A step may not overshoot the remaining need by a huge margin: evicting a
        // 50k-token tool result to reclaim 200 tokens is a bad trade even when the
        // tier is technically correct.
        if c.tokens > 0 && still_needed > 0 && c.tokens > still_needed.saturating_mul(8).max(8192)
            && next != EpisodeTier::Masked {
                return Ok(None);
            }

        let episode = self.fabric.episode(&c.episode_id).await?;
        let tokens_before = episode.live_tokens(&self.counter);
        let mut preview = episode.clone();
        preview.eviction_tier = next;
        let tokens_after = preview.live_tokens(&self.counter);

        // Never accept an escalation that does not actually reclaim tokens.
        if tokens_after >= tokens_before {
            return Ok(None);
        }

        Ok(Some(TierUpdate {
            episode_id: c.episode_id.clone(),
            from: c.current_tier,
            to: next,
            tokens_before,
            tokens_after,
            reason: format!(
                "tier {} → {} (value {:.2}, {} token(s) reclaimed){}",
                c.current_tier.as_str(),
                next.as_str(),
                c.value,
                tokens_before - tokens_after,
                if c.notes.is_empty() {
                    String::new()
                } else {
                    format!(": {}", c.notes.join("; "))
                }
            ),
        }))
    }

    /// Where the next prefill should start, and how to evict around it.
    ///
    /// # The shape of cache-cheap compaction
    ///
    /// A compaction can only save prefill work if the *head* of the new prompt is
    /// byte-identical to the head of the old one. So the question is not "which
    /// episodes are least valuable" — it is "where can the head be cut so that the
    /// server already holds everything before it". Sakur4 asks that question
    /// first:
    ///
    /// 1. Propose a boundary a short way into the session
    ///    ([`EvictionPolicy::cache_prefix_reserve_tokens`]).
    /// 2. Ask the Cache-Coherence Layer to snap it onto a checkpoint it can
    ///    actually rewind to, or to tell us that no such boundary exists.
    /// 3. Keep every episode before that boundary verbatim, and absorb the
    ///    pressure from what follows.
    ///
    /// An earlier design did this the other way round — pick evictions greedily
    /// from the oldest turn, then ask the cache whether the resulting boundary
    /// happened to line up. It never did: the boundary landed at token ~0, no
    /// checkpoint exists there, and every compaction was reported as a full
    /// re-prefill. The order is the whole design.
    ///
    /// When no boundary can be aligned (no checkpoint source, or a ring that has
    /// moved past the proposed cut) this returns the base reserve and the caller
    /// proceeds with a rewrite — the honest fallback, reported as such.
    async fn boundary_and_prefix(
        &self,
        session_id: &str,
        slot_id: &str,
        live_tokens: usize,
    ) -> (Option<BoundaryPlan>, usize) {
        let base_reserve = self
            .policy
            .keep_prefix_tokens
            .max(self.policy.cache_prefix_reserve_tokens);
        // Two independent bounds on the prefix: no more than a fraction of the live
        // window (or there is no middle left to evict), and no more than the
        // configured absolute ceiling (so a pathological ring cannot eat the whole
        // session).
        let prefix_max = ((live_tokens as f64) * self.policy.max_prefix_ratio)
            .max(1.0)
            .min(self.policy.cache_prefix_max_tokens.max(1) as f64)
            as usize;
        let proposal = base_reserve.min(prefix_max) as i64;
        let tolerance = self.coherence.snap_tolerance_tokens();

        let checkpoints = match self.coherence.usable_checkpoints(session_id, slot_id).await {
            Ok(c) => c,
            Err(e) => {
                tracing::debug!(error = %e, "checkpoint discovery failed");
                return (None, base_reserve.min(prefix_max));
            }
        };
        if checkpoints.is_empty() {
            return (
                Some(BoundaryPlan::full_rewrite(
                    proposal,
                    "no checkpoints are available on this backend/session; full re-prefill",
                )),
                base_reserve.min(prefix_max),
            );
        }

        // Prefer the nearest checkpoint at or below the proposal, inside tolerance.
        if let Some((cut, reason)) = snap_to_checkpoint(proposal, &checkpoints, tolerance) {
            return (
                Some(BoundaryPlan::aligned(
                    proposal,
                    cut,
                    reason,
                    checkpoints.len(),
                    false,
                )),
                (cut as usize).clamp(1, prefix_max),
            );
        }

        // Nothing usable below the proposal. If the ring has already dropped that
        // far back — which is normal, because a ring only spans
        // `interval × depth` tokens — the oldest checkpoint *is* the earliest
        // boundary the server can rewind to. Align forward onto it, provided it is
        // still early enough to leave a middle worth evicting.
        //
        // Surrendering to a full re-prefill here would be the wrong call: the
        // tokens before the oldest checkpoint are genuinely uncached, but
        // everything from that checkpoint onward is reusable, and that is most of
        // the prompt.
        if let Some(oldest) = checkpoints.first() {
            let cut = oldest.token_position;
            if cut > 0 && (cut as usize) <= prefix_max {
                return (
                    Some(BoundaryPlan::aligned(
                        proposal,
                        cut,
                        snap_reason_for(oldest, cut - proposal),
                        checkpoints.len(),
                        false,
                    )),
                    cut as usize,
                );
            }
            // The only alignable boundary is past the point where eviction could
            // still help. Say exactly that rather than reporting a generic rewrite.
            return (
                Some(BoundaryPlan::unaligned_reason(
                    proposal,
                    Some(cut),
                    tolerance,
                    &format!(
                        "the earliest checkpoint the server can rewind to is at {cut} tokens, past \
                         the {prefix_max}-token limit on a preserved prefix; preserving it would \
                         leave nothing to evict, so this compaction cannot reuse the cache. \
                         Configure a deeper checkpoint ring (-ctxcp) to make alignment possible"
                    ),
                )),
                base_reserve.min(prefix_max),
            );
        }

        (
            Some(BoundaryPlan::full_rewrite(
                proposal,
                "no usable checkpoint positions were reported",
            )),
            base_reserve.min(prefix_max),
        )
    }

    /// Index of the first episode that may be evicted, given a prefix token budget.
    ///
    /// Returns `episodes.len()` when the whole session fits inside the prefix.
    fn prefix_end_index(episodes: &[EpisodeRow], prefix_budget: usize) -> usize {
        let mut accumulated = 0usize;
        for (idx, ep) in episodes.iter().enumerate() {
            let cost = ep.token_count as usize;
            // The oldest episode is always kept, even when it alone exceeds the
            // budget: a zero-length prefix is not a prefix.
            if idx > 0 && accumulated + cost > prefix_budget {
                return idx;
            }
            accumulated += cost;
        }
        episodes.len()
    }

    /// Index just past the last episode of the preserved recent window.
    fn recent_start_index(episodes: &[EpisodeRow], recent_budget: usize) -> usize {
        let mut accumulated = 0usize;
        for (idx, ep) in episodes.iter().enumerate().rev() {
            accumulated += ep.token_count as usize;
            if accumulated >= recent_budget {
                return idx + 1;
            }
        }
        0
    }

    /// Tokens of the rendered timeline that survive the plan from the head.
    ///
    /// Everything before the first escalated episode is retained verbatim; the
    /// boundary is where that run ends.
    fn retained_prefix_after(&self, episodes: &[EpisodeRow], updates: &[TierUpdate]) -> usize {
        let changed: std::collections::HashSet<&str> =
            updates.iter().map(|u| u.episode_id.as_str()).collect();
        let mut total = 0usize;
        for ep in episodes {
            if changed.contains(ep.episode_id.as_str()) {
                break;
            }
            total += ep.token_count as usize;
        }
        total
    }
}

/// The snap reason implied by which kind of checkpoint a boundary landed on.
fn snap_reason_for(cp: &crate::llama::CheckpointRef, delta: i64) -> crate::llama::SnapReason {
    use crate::llama::SnapReason;
    match cp.kind {
        crate::llama::CheckpointKind::Internal => SnapReason::InternalCheckpoint { delta },
        crate::llama::CheckpointKind::SlotSaveFile => SnapReason::SlotSaveFile { delta },
        crate::llama::CheckpointKind::FoldMarker => SnapReason::FoldMarker { delta },
        crate::llama::CheckpointKind::PreRewrite => SnapReason::PreRewrite { delta },
    }
}

/// A full fold trace.
#[derive(Debug, Clone, serde::Serialize)]
pub struct FoldTrace {
    pub fold_id: String,
    pub description: String,
    pub goal: String,
    pub status: String,
    pub result_summary: Option<String>,
    pub tokens_at_open: usize,
    pub tokens_reclaimed: usize,
    pub created_at: String,
    pub closed_at: Option<String>,
    pub episodes: Vec<FoldTraceEntry>,
}

/// One episode inside a fold trace.
#[derive(Debug, Clone, serde::Serialize)]
pub struct FoldTraceEntry {
    pub episode_id: String,
    pub seq: i64,
    pub role: String,
    pub tool_name: Option<String>,
    pub token_count: usize,
    pub content: String,
}

/// Number of `RenderedPart`s a plan is expected to know about; used by tests to
/// assert that the engine stays in step with the prompt assembler.
pub const EVICTION_KNOWN_PARTS: usize = 8;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::CoherenceConfig;
    use crate::llama::embedded::EmbeddedBackend;
    use crate::memory::episodic::NewEpisode;
    use crate::prompt::PromptParts;
    use crate::store::Db;
    use crate::tokens::CharTokenizer;
    use std::sync::Arc;

    /// Every test uses a character tokenizer so budget arithmetic is exact.
    fn counter() -> TokenCounter {
        TokenCounter::new(CharTokenizer { chars_per_token: 4 })
    }

    async fn rig() -> (MemoryFabric, EvictionEngine, Arc<EmbeddedBackend>) {
        let db = Db::open_in_memory().await.unwrap();
        let backend = Arc::new(EmbeddedBackend::new().with_ring(256, 8));
        let counter = counter();
        let fabric = MemoryFabric::new(db.clone());
        let coherence = Coherence::new(db.clone(), backend.clone(), CoherenceConfig::default());
        let engine = EvictionEngine::new(
            fabric.clone(),
            coherence,
            EvictionPolicy::default(),
            counter,
        );
        (fabric, engine, backend)
    }

    /// Commit `n` episodes of `tokens` nominal size each.
    async fn seed(fabric: &MemoryFabric, n: usize, tokens: usize) {
        let counter = counter();
        for i in 0..n {
            let body = "x".repeat(tokens * 4);
            fabric
                .commit_episode(
                    NewEpisode::user("s1", body).with_slot("0"),
                    &counter,
                    false,
                    false,
                )
                .await
                .unwrap_or_else(|e| panic!("seed {i} failed: {e}"));
        }
    }

    fn big_prompt(timeline_tokens: usize) -> PromptParts {
        PromptParts {
            system: "system".into(),
            anchors: String::new(),
            timeline: "t".repeat(timeline_tokens * 4),
            repo_map: String::new(),
            recall: String::new(),
            tool_schemas: String::new(),
            folds: String::new(),
            extra: Vec::new(),
        }
    }

    #[tokio::test]
    async fn relaxed_pressure_produces_no_plan() {
        let (fabric, engine, _b) = rig().await;
        seed(&fabric, 5, 100).await;
        let plan = engine.plan("s1", "0", 32_768, &big_prompt(500)).await.unwrap();
        assert_eq!(plan.pressure, Pressure::Relaxed);
        assert!(plan.is_empty());
        assert!(plan.notes[0].contains("below the"));
    }

    #[tokio::test]
    async fn pressure_triggers_and_the_plan_reclaims_toward_target() {
        let (fabric, engine, _b) = rig().await;
        seed(&fabric, 40, 1000).await; // 40k tokens of history
        let parts = big_prompt(40_000);
        let plan = engine.plan("s1", "0", 32_768, &parts).await.unwrap();
        assert_eq!(plan.pressure, Pressure::Compacting);
        assert!(!plan.is_empty(), "a 40k-token session must produce evictions");
        assert!(plan.planned_savings > 0);
        assert!(
            plan.token_after_plan() <= plan.threshold,
            "plan must at least return under the trigger"
        );
    }

    #[tokio::test]
    async fn evictions_escalate_one_tier_at_a_time() {
        let (fabric, engine, _b) = rig().await;
        seed(&fabric, 40, 1000).await;
        let plan = engine
            .plan("s1", "0", 32_768, &big_prompt(40_000))
            .await
            .unwrap();
        for u in &plan.updates {
            assert_eq!(
                u.to.severity(),
                u.from.severity() + 1,
                "FR-5 forbids skipping tiers: {} → {}",
                u.from.as_str(),
                u.to.as_str()
            );
        }
    }

    #[tokio::test]
    async fn drop_is_never_selected_without_explicit_opt_in() {
        let (fabric, engine, _b) = rig().await;
        seed(&fabric, 60, 1000).await;
        let plan = engine
            .plan("s1", "0", 32_768, &big_prompt(60_000))
            .await
            .unwrap();
        assert!(
            plan.updates.iter().all(|u| u.to != EpisodeTier::Dropped),
            "allow_drop defaults to false; nothing may be dropped"
        );
    }

    #[tokio::test]
    async fn round_trip_integrity_survives_the_harshest_plan() {
        let (fabric, engine, _b) = rig().await;
        seed(&fabric, 40, 1000).await;
        let before: Vec<String> = fabric
            .session_episodes("s1")
            .await
            .unwrap()
            .into_iter()
            .map(|e| e.content)
            .collect();

        let parts = big_prompt(40_000);
        let plan = engine.plan("s1", "0", 32_768, &parts).await.unwrap();
        engine.apply(&plan, &parts).await.unwrap();

        let after: Vec<String> = fabric
            .session_episodes("s1")
            .await
            .unwrap()
            .into_iter()
            .map(|e| e.content)
            .collect();
        assert_eq!(before, after, "FR-5: evicted content must be bit-identical");

        // And the tiers really did change.
        let tiers: Vec<EpisodeTier> = fabric
            .session_episodes("s1")
            .await
            .unwrap()
            .into_iter()
            .map(|e| e.eviction_tier)
            .collect();
        assert!(tiers.iter().any(|t| *t != EpisodeTier::Live));
    }

    #[tokio::test]
    async fn recent_context_is_never_evicted() {
        let (fabric, engine, _b) = rig().await;
        seed(&fabric, 40, 1000).await;
        let plan = engine
            .plan("s1", "0", 32_768, &big_prompt(40_000))
            .await
            .unwrap();

        let episodes = fabric.session_episodes("s1").await.unwrap();
        let seq_of = |id: &str| episodes.iter().find(|e| e.episode_id == id).map(|e| e.seq);
        let touched: Vec<i64> = plan.updates.iter().filter_map(|u| seq_of(&u.episode_id)).collect();
        let newest_seq = episodes.iter().map(|e| e.seq).max().unwrap();
        let newest_touched = touched.iter().copied().max().unwrap_or(0);

        assert!(
            newest_touched < newest_seq,
            "the newest turn must never be evicted (newest={newest_seq}, newest evicted={newest_touched})"
        );
        // The plan's retained prefix is what the cache layer will try to keep.
        assert!(plan.retained_prefix_tokens > 0);
        assert!(engine.policy().keep_recent_tokens > 0);
    }

    #[tokio::test]
    async fn anchors_are_never_candidates_for_eviction() {
        let (fabric, engine, _b) = rig().await;
        seed(&fabric, 40, 1000).await;
        // Pin something and assert the engine's candidate space cannot contain it:
        // the engine selects from `evictable_episodes`, and anchors live in a
        // different table entirely. The structural claim is what matters.
        fabric
            .pin(crate::memory::anchor::PinRequest::new(
                crate::memory::anchor::AnchorKind::SafetyConstraint,
                "never force-push to main",
            ))
            .await
            .unwrap();
        let parts = big_prompt(40_000);
        let plan = engine.plan("s1", "0", 32_768, &parts).await.unwrap();
        assert!(plan.anchor_tokens > 0, "the pinned anchor must cost tokens");
        assert!(
            plan.updates.iter().all(|u| !u.episode_id.starts_with("anc_")),
            "no eviction update may reference an anchor"
        );
    }

    #[tokio::test]
    async fn anchor_overflow_is_a_visible_error_not_a_silent_drop() {
        let (fabric, engine, _b) = rig().await;
        seed(&fabric, 5, 100).await;
        // An anchor set that alone blows the window.
        for i in 0..4 {
            fabric
                .pin(crate::memory::anchor::PinRequest::new(
                    crate::memory::anchor::AnchorKind::TaskContract,
                    format!("constraint {i}: {}", "y".repeat(2000)),
                ))
                .await
                .unwrap();
        }
        let err = engine
            .plan("s1", "0", 1024, &big_prompt(100))
            .await
            .unwrap_err();
        assert!(matches!(err, Error::BudgetOverflow(_)));
        assert!(err.to_string().contains("will not silently drop"));
    }

    #[tokio::test]
    async fn dependency_holds_a_large_tool_result_above_a_later_turn() {
        let (fabric, engine, _b) = rig().await;
        let counter = counter();
        // One huge tool result, then a small episode derived from it.
        let tool = fabric
            .commit_episode(
                NewEpisode::user("s1", "z".repeat(20_000)).with_slot("0"),
                &counter,
                false,
                false,
            )
            .await
            .unwrap();
        let derived = fabric
            .commit_episode(
                NewEpisode::user("s1", "w".repeat(400)),
                &counter,
                false,
                false,
            )
            .await
            .unwrap();
        fabric
            .add_edge(crate::memory::dependency::EdgeRow::new(
                &NodeRef::episode(&derived.episode_id),
                &NodeRef::episode(&tool.episode_id),
                crate::memory::dependency::EdgeKind::DerivedFrom,
            ))
            .await
            .unwrap();
        seed(&fabric, 20, 1000).await;

        let plan = engine
            .plan("s1", "0", 32_768, &big_prompt(41_000))
            .await
            .unwrap();
        let tool_update = plan
            .updates
            .iter()
            .find(|u| u.episode_id == tool.episode_id)
            .map(|u| u.to);
        // It may be masked (cheap, reversible) but never archived or dropped,
        // because something still depends on it.
        if let Some(tier) = tool_update {
            assert!(
                tier.severity() <= EpisodeTier::Masked.severity(),
                "an episode with a live dependent must not leave the window: got {}",
                tier.as_str()
            );
        }
    }

    #[tokio::test]
    async fn fold_and_unfold_collapse_the_window_and_keep_the_trace() {
        let (fabric, engine, backend) = rig().await;
        backend.advance_to(2000);
        let fold = engine
            .fold("s1", "0", "refactor auth module", "extract the token validator")
            .await
            .unwrap();
        assert!(fold.fold_id.starts_with("fold_"));
        assert!(fold.checkpoint.is_some(), "a save point should have been taken");

        let counter = counter();
        for i in 0..6 {
            let ep = fabric
                .commit_episode(
                    NewEpisode::user("s1", format!("folded step {i}: {}", "q".repeat(400)))
                        .with_slot("0"),
                    &counter,
                    false,
                    false,
                )
                .await
                .unwrap();
            engine
                .tag_episode_with_fold(&ep.episode_id, &fold.fold_id)
                .await
                .unwrap();
        }
        backend.advance_to(6000);

        let out = engine
            .unfold("s1", "0", &fold.fold_id, "extracted TokenValidator into its own module")
            .await
            .unwrap();
        assert_eq!(out.episodes_folded, 6);
        assert!(out.tokens_reclaimed > 0);
        assert!(out.trace_retrievable);

        // Live window no longer carries the folded steps.
        let timeline = fabric
            .timeline("s1", 100_000, &counter, false)
            .await
            .unwrap();
        assert!(
            timeline.rendered.contains("result: extracted TokenValidator"),
            "the summary must be in the live window"
        );
        assert!(
            !timeline.rendered.contains("folded step 0"),
            "folded steps must have left the live window"
        );

        // But the full trace is still retrievable.
        let trace = engine.recall_fold(&fold.fold_id).await.unwrap();
        assert_eq!(trace.episodes.len(), 6);
        assert!(trace.episodes[1].content.contains("folded step 1"));
        assert_eq!(trace.status, "closed");
    }

    #[tokio::test]
    async fn unfold_refuses_an_empty_summary() {
        let (fabric, engine, _b) = rig().await;
        let fold = engine.fold("s1", "0", "d", "g").await.unwrap();
        let _ = fabric;
        let err = engine.unfold("s1", "0", &fold.fold_id, "   ").await.unwrap_err();
        assert!(matches!(err, Error::Invalid(_)));
    }

    #[tokio::test]
    async fn unfold_twice_is_rejected() {
        let (fabric, engine, _b) = rig().await;
        let fold = engine.fold("s1", "0", "d", "g").await.unwrap();
        engine.unfold("s1", "0", &fold.fold_id, "done").await.unwrap();
        let err = engine
            .unfold("s1", "0", &fold.fold_id, "done again")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("already closed"));
        let _ = fabric;
    }

    #[tokio::test]
    async fn a_plan_carries_a_cache_verdict() {
        let (fabric, engine, backend) = rig().await;
        seed(&fabric, 40, 1000).await;
        backend.advance_to(41_000);
        let plan = engine
            .plan("s1", "0", 32_768, &big_prompt(40_000))
            .await
            .unwrap();
        let coherence = plan.coherence.expect("cache alignment must be evaluated");
        // The retained prefix ends where the first escalation begins, and the
        // embedded ring is 256-deep, so this boundary is usually an exact or
        // near-exact ring hit.
        assert!(coherence.requested_cut > 0);
        assert!(coherence.summary().len() > 10);
    }

    #[tokio::test]
    async fn applying_a_plan_records_state_for_the_next_turn() {
        let (fabric, engine, backend) = rig().await;
        seed(&fabric, 40, 1000).await;
        backend.advance_to(41_000);
        let parts = big_prompt(40_000);
        let plan = engine.plan("s1", "0", 32_768, &parts).await.unwrap();
        let outcome = engine.apply(&plan, &parts).await.unwrap();
        assert_eq!(outcome.applied, plan.updates.len());
        assert!(outcome.tokens_reclaimed > 0);

        let obs = engine
            .coherence()
            .observe_prompt("s1", "0", &parts.timeline, &counter())
            .await
            .unwrap();
        assert!(obs.prompt_tokens > 0);
        assert!(obs.detail.len() > 5);
        let _ = fabric;
    }

    #[tokio::test]
    async fn compact_if_needed_is_a_no_op_when_relaxed() {
        let (fabric, engine, _b) = rig().await;
        seed(&fabric, 3, 100).await;
        let out = engine
            .compact_if_needed("s1", "0", 32_768, &big_prompt(300))
            .await
            .unwrap();
        assert!(out.is_none());
        let _ = fabric;
    }

    #[tokio::test]
    async fn suppressing_cache_alignment_leaves_the_verdict_absent() {
        let db = Db::open_in_memory().await.unwrap();
        let backend = Arc::new(EmbeddedBackend::new().with_ring(256, 8));
        let fabric = MemoryFabric::new(db.clone());
        let coherence = Coherence::new(db, backend, CoherenceConfig::default());
        let policy = EvictionPolicy {
            cache_align: false,
            ..Default::default()
        };
        let engine = EvictionEngine::new(fabric.clone(), coherence, policy, counter());
        seed(&fabric, 40, 1000).await;
        let plan = engine
            .plan("s1", "0", 32_768, &big_prompt(40_000))
            .await
            .unwrap();
        assert!(plan.coherence.is_none());
        assert!(!plan.is_cache_cheap());
    }
}
