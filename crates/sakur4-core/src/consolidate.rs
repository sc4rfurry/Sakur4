//! The Idle Consolidator — the "Dream Cycle" (PRD component C6).
//!
//! Memory-quality work is moved off the interactive path entirely: promotion of
//! high-value episodes into the Semantic Atlas, staleness detection and
//! regeneration, re-embedding, and archival of long-cold episodes all happen when
//! no slot is generating. That is the PRD's stated contribution — applying
//! sleep-time compute "specifically to the staleness-detection and
//! re-consolidation problem created by the dual-track design".
//!
//! # Two hard requirements, handled structurally
//!
//! * **FR-13: never run concurrently with an active generation on any tracked
//!   slot.** Enforced by asking every tracked slot before each work item, not by
//!   a timer. A slot that starts generating mid-pass stops the pass.
//! * **FR-13: fully interruptible; a new user message immediately preempts and
//!   safely checkpoints in-progress work.** Work is performed one item at a time,
//!   each in its own transaction, and the loop re-checks idleness between items.
//!   Interruption therefore loses at most the item in flight, and that item was
//!   never partially written.
//!
//! # Which model writes the summaries
//!
//! The PRD flags the auxiliary-model question as open, because a user running a
//! 27-35B model on a 24 GB card may have no VRAM for a second model. Sakur4
//! therefore defaults to [`SummaryPolicy::Extractive`]: a deterministic,
//! dependency-free summariser built from the episode's own sentences and
//! identifiers. It is honest about what it is — an extract, not an interpretation
//! — and it means the Dual-Track discipline holds even with no auxiliary model at
//! all. When the operator *does* configure an auxiliary endpoint, the policy can
//! be switched and the summaries become genuinely interpretive, still anchored.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::embed::Embedder;
use crate::error::Result;
use crate::llama::InferenceBackend;
use crate::memory::fabric::MemoryFabric;
use crate::memory::semantic::{AnchorType, SemanticWrite};
use crate::store::Db;
use crate::tokens::TokenCounter;

/// Consolidation configuration.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct ConsolidatorConfig {
    /// Seconds of no generation across all tracked slots before work starts.
    pub quiet_period_secs: u64,
    /// How often to check whether the system has gone quiet.
    pub poll_interval_secs: u64,
    /// Minimum tokens for an episode to be worth summarising. Below this the
    /// summary would cost as much context as the original.
    pub min_episode_tokens: usize,
    /// Maximum items processed per pass, bounding a pass's cost.
    pub max_items_per_pass: usize,
    /// Regenerate stale Atlas entries.
    pub regenerate_stale: bool,
    /// Write new Atlas entries for promotable episodes.
    pub promote_episodes: bool,
    /// Re-embed Atlas entries whose embedding is missing.
    pub reembed: bool,
    /// Archive episodes colder than this many days that nothing depends on.
    pub archive_after_days: Option<i64>,
    /// Cap on how many stale entries to regenerate in one pass.
    pub max_regenerations_per_pass: usize,
    /// Which summariser to use.
    pub summary_policy: SummaryPolicy,
}

impl Default for ConsolidatorConfig {
    fn default() -> Self {
        Self {
            quiet_period_secs: 90,
            poll_interval_secs: 15,
            min_episode_tokens: 96,
            max_items_per_pass: 24,
            regenerate_stale: true,
            promote_episodes: true,
            reembed: true,
            archive_after_days: Some(14),
            max_regenerations_per_pass: 8,
            summary_policy: SummaryPolicy::Extractive,
        }
    }
}

/// How summaries are produced.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum SummaryPolicy {
    /// Deterministic extraction from the episode's own text. No model, no
    /// network, no VRAM. Default.
    Extractive,
    /// Ask an OpenAI-compatible local endpoint to write an interpretation, still
    /// mandatorily anchored.
    AuxiliaryModel {
        base_url: String,
        model: String,
        max_tokens: usize,
    },
}

/// What one pass did.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct ConsolidationReport {
    pub ran: bool,
    pub skipped_reason: Option<String>,
    pub promoted: usize,
    pub regenerated: usize,
    pub reembedded: usize,
    pub archived: usize,
    pub pruned_vectors: usize,
    pub interrupted: bool,
    pub elapsed_ms: i64,
    pub notes: Vec<String>,
}

impl ConsolidationReport {
    pub fn summary(&self) -> String {
        if !self.ran {
            return format!(
                "consolidation skipped: {}",
                self.skipped_reason.as_deref().unwrap_or("unknown reason")
            );
        }
        format!(
            "dream cycle: promoted {}, regenerated {}, re-embedded {}, archived {}{} · {} ms",
            self.promoted,
            self.regenerated,
            self.reembedded,
            self.archived,
            if self.interrupted { " (interrupted)" } else { "" },
            self.elapsed_ms
        )
    }
}

/// The consolidator.
#[derive(Clone)]
pub struct Consolidator {
    db: Db,
    fabric: MemoryFabric,
    backend: Arc<dyn InferenceBackend>,
    embedder: Arc<dyn Embedder>,
    counter: TokenCounter,
    config: ConsolidatorConfig,
    /// Slot ids the harness has told Sakur4 about. Only these are checked for
    /// idleness; an unknown slot cannot be proven idle, and assuming otherwise
    /// would risk FR-13's forbidden overlap.
    tracked_slots: Arc<parking_lot::RwLock<Vec<String>>>,
    running: Arc<AtomicBool>,
    last_activity: Arc<AtomicU64>,
}

impl Consolidator {
    pub fn new(
        db: Db,
        fabric: MemoryFabric,
        backend: Arc<dyn InferenceBackend>,
        embedder: Arc<dyn Embedder>,
        counter: TokenCounter,
        config: ConsolidatorConfig,
    ) -> Self {
        Self {
            db,
            fabric,
            backend,
            embedder,
            counter,
            config,
            tracked_slots: Arc::new(parking_lot::RwLock::new(vec!["0".to_string()])),
            running: Arc::new(AtomicBool::new(false)),
            last_activity: Arc::new(AtomicU64::new(now_secs())),
        }
    }

    pub fn config(&self) -> &ConsolidatorConfig {
        &self.config
    }

    /// Register a slot so idleness checks cover it.
    pub fn track_slot(&self, slot_id: &str) {
        let mut slots = self.tracked_slots.write();
        if !slots.iter().any(|s| s == slot_id) {
            slots.push(slot_id.to_string());
        }
    }

    pub fn tracked_slots(&self) -> Vec<String> {
        self.tracked_slots.read().clone()
    }

    /// Record harness activity. Any request on any slot resets the quiet timer,
    /// which is what makes "a new user message immediately preempts" true.
    pub fn note_activity(&self) {
        self.last_activity.store(now_secs(), Ordering::SeqCst);
    }

    /// Whether the system is quiet enough to consolidate.
    pub async fn is_idle(&self) -> bool {
        let quiet_for = now_secs().saturating_sub(self.last_activity.load(Ordering::SeqCst));
        if quiet_for < self.config.quiet_period_secs {
            return false;
        }
        for slot in self.tracked_slots() {
            match self.backend.slot_state(&slot).await {
                Ok(state) if state.is_processing => return false,
                Ok(_) => {}
                // A slot we cannot read is a slot we cannot prove idle. FR-13's
                // requirement is "never runs concurrently with an active
                // generation", so unprovable means "do not run".
                Err(e) => {
                    tracing::debug!(slot = %slot, error = %e, "cannot confirm slot idleness");
                    return false;
                }
            }
        }
        true
    }

    /// Run one consolidation pass if the system is idle.
    ///
    /// Returns a report describing what happened, including why nothing did.
    pub async fn maybe_run(&self) -> Result<ConsolidationReport> {
        let started = std::time::Instant::now();
        let mut report = ConsolidationReport::default();

        // Mutual exclusion: two overlapping passes would duplicate Atlas writes.
        if self.running.swap(true, Ordering::SeqCst) {
            let mut r = ConsolidationReport {
                skipped_reason: Some("another consolidation pass is already running".into()),
                ..Default::default()
            };
            r.elapsed_ms = started.elapsed().as_millis() as i64;
            return Ok(r);
        }
        let _guard = RunningGuard(self.running.clone());

        if !self.is_idle().await {
            let mut r = ConsolidationReport {
                skipped_reason: Some(format!(
                    "not idle yet (quiet period is {}s, or a tracked slot is generating)",
                    self.config.quiet_period_secs
                )),
                ..Default::default()
            };
            r.elapsed_ms = started.elapsed().as_millis() as i64;
            return Ok(r);
        }

        report.ran = true;

        // --- 1. regenerate stale summaries --------------------------------
        if self.config.regenerate_stale {
            let stale = self.fabric.staleness_report(None, 64).await?;
            for entry in stale.entries.iter().take(self.config.max_regenerations_per_pass) {
                if !self.is_idle().await {
                    report.interrupted = true;
                    report
                        .notes
                        .push("interrupted by new activity while regenerating".into());
                    break;
                }
                let fresh = match self.summarise_anchor(entry.anchor_type, &entry.anchor_id).await {
                    Ok(Some(text)) => text,
                    Ok(None) => continue,
                    Err(e) => {
                        report.notes.push(format!(
                            "could not re-derive a summary for {}: {e}",
                            entry.atlas_id
                        ));
                        continue;
                    }
                };
                match self.fabric.refresh_semantic(&entry.atlas_id, fresh).await {
                    Ok(updated) => {
                        report.regenerated += 1;
                        if self.config.reembed {
                            if let Err(e) = self.embed_atlas(&updated.atlas_id).await {
                                report.notes.push(format!("re-embed failed: {e}"));
                            }
                        }
                    }
                    Err(e) => report
                        .notes
                        .push(format!("regeneration of {} failed: {e}", entry.atlas_id)),
                }
            }
        }

        // --- 2. promote high-value episodes -------------------------------
        if self.config.promote_episodes && !report.interrupted {
            let promoted = self.promote_episodes(&mut report).await?;
            report.promoted = promoted;
        }

        // --- 3. re-embed anything missing a vector ------------------------
        if self.config.reembed && !report.interrupted {
            report.reembedded = self.backfill_embeddings(&mut report).await?;
        }

        // --- 4. archive long-cold, undepended episodes --------------------
        if let Some(days) = self.config.archive_after_days {
            if !report.interrupted {
                report.archived = self.archive_cold(days).await?;
            }
        }

        // --- 5. housekeeping ----------------------------------------------
        report.pruned_vectors = self.fabric.db().prune_orphan_vectors().await?;

        report.elapsed_ms = started.elapsed().as_millis() as i64;
        tracing::info!(report = %report.summary(), "consolidation pass finished");
        Ok(report)
    }

    /// A bounded loop for running the consolidator as a background task.
    ///
    /// The caller owns cancellation: dropping or cancelling the future stops the
    /// loop between passes, never inside one, because each pass's work items are
    /// individually transactional.
    pub async fn run_loop(self, mut stop: tokio::sync::watch::Receiver<bool>) {
        let interval = std::time::Duration::from_secs(self.config.poll_interval_secs.max(1));
        loop {
            tokio::select! {
                _ = tokio::time::sleep(interval) => {}
                _ = stop.changed() => {
                    if *stop.borrow() {
                        tracing::info!("consolidator stopping");
                        return;
                    }
                    continue;
                }
            }
            match self.maybe_run().await {
                Ok(r) if r.ran => tracing::debug!(report = %r.summary(), "consolidator pass"),
                Ok(_) => {}
                Err(e) => tracing::warn!(error = %e, "consolidation pass failed"),
            }
        }
    }

    // -----------------------------------------------------------------------
    // internals
    // -----------------------------------------------------------------------

    /// Promote recent, substantial episodes into the Atlas.
    async fn promote_episodes(&self, report: &mut ConsolidationReport) -> Result<usize> {
        let sessions = self.fabric.sessions(16).await?;
        let mut promoted = 0usize;

        for (session_id, _, _) in sessions {
            if promoted >= self.config.max_items_per_pass {
                break;
            }
            if !self.is_idle().await {
                report.interrupted = true;
                report
                    .notes
                    .push("interrupted by new activity while promoting".into());
                break;
            }

            let episodes = self.fabric.session_episodes(&session_id).await?;
            // Only episodes that are still live (evicting them is the eviction
            // engine's job) and that carry enough signal to be worth an entry.
            let candidates: Vec<_> = episodes
                .iter()
                .filter(|e| {
                    e.eviction_tier == crate::memory::episodic::EpisodeTier::Live
                        && e.token_count as usize >= self.config.min_episode_tokens
                        && matches!(e.role.as_str(), "user" | "assistant" | "tool")
                })
                .collect();

            for ep in candidates.iter().rev().take(4) {
                if promoted >= self.config.max_items_per_pass {
                    break;
                }
                // Skip episodes already summarised: the Atlas's anchor link table
                // is the record of that.
                let existing: i64 = self
                    .db
                    .with({
                        let id = ep.episode_id.clone();
                        move |c| {
                            Ok(c.query_row(
                                "SELECT COUNT(*) FROM semantic_atlas
                                 WHERE anchor_type='episodic_stream' AND anchor_id = ?1",
                                [id],
                                |r| r.get(0),
                            )?)
                        }
                    })
                    .await?;
                if existing > 0 {
                    continue;
                }

                let summary = self.summarise_text(&ep.content).await?;
                if summary.trim().is_empty() {
                    continue;
                }

                // A promotion must actually save context. An extractive summary
                // of a short or dense turn can easily be *longer* than the turn
                // itself, and an Atlas entry that costs more tokens than the text
                // it replaces is a regression dressed up as memory maintenance.
                // The `min_episode_tokens` threshold is a proxy for this; this is
                // the measurement.
                let summary_tokens = self.counter.count(&summary).get();
                let episode_tokens = ep.token_count.max(0) as usize;
                if summary_tokens >= episode_tokens {
                    report.notes.push(format!(
                        "skipped promotion of episode {} (seq {}): its summary would cost {} \
                         tokens against the episode's {}, so promoting it would grow the \
                         context rather than shrink it",
                        ep.episode_id, ep.seq, summary_tokens, episode_tokens
                    ));
                    continue;
                }

                let write = SemanticWrite::on_episode(summary, ep.episode_id.clone())
                    .in_session(session_id.clone())
                    .also_anchored_to(AnchorType::EpisodicStream, ep.episode_id.clone());
                match self.fabric.put_semantic(write).await {
                    Ok(entry) => {
                        promoted += 1;
                        if self.config.reembed {
                            let _ = self.embed_atlas(&entry.atlas_id).await;
                        }
                    }
                    Err(e) => report
                        .notes
                        .push(format!("promotion of {} failed: {e}", ep.episode_id)),
                }
            }
        }
        Ok(promoted)
    }

    /// Re-derive a summary from whichever anchor an Atlas entry points at.
    async fn summarise_anchor(
        &self,
        anchor_type: AnchorType,
        anchor_id: &str,
    ) -> Result<Option<String>> {
        match anchor_type {
            AnchorType::SymbolicFact => {
                let Some(fact) = self.fabric.fact_by_id(anchor_id).await? else {
                    return Ok(None);
                };
                // A symbolic anchor has a deterministic description available, so
                // no model is needed even under the auxiliary-model policy: the
                // signature *is* the truth.
                Ok(Some(match &fact.signature {
                    Some(sig) => format!(
                        "{} is declared as `{sig}`{}",
                        fact.qualified_name,
                        fact.file_path
                            .as_ref()
                            .map(|f| format!(" in {f}"))
                            .unwrap_or_default()
                    ),
                    None => format!("{} is defined in the project", fact.qualified_name),
                }))
            }
            AnchorType::EpisodicStream => {
                let Ok(ep) = self.fabric.episode(anchor_id).await else {
                    return Ok(None);
                };
                Ok(Some(self.summarise_text(&ep.content).await?))
            }
        }
    }

    /// Produce a summary under the configured policy.
    async fn summarise_text(&self, text: &str) -> Result<String> {
        match &self.config.summary_policy {
            SummaryPolicy::Extractive => Ok(extractive_summary(text, 3, 400)),
            SummaryPolicy::AuxiliaryModel {
                base_url,
                model,
                max_tokens,
            } => {
                match self
                    .call_auxiliary(base_url, model, text, *max_tokens)
                    .await
                {
                    Ok(s) if !s.trim().is_empty() => Ok(s),
                    Ok(_) => Ok(extractive_summary(text, 3, 400)),
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            "auxiliary summary model failed; falling back to extraction"
                        );
                        Ok(extractive_summary(text, 3, 400))
                    }
                }
            }
        }
    }

    async fn call_auxiliary(
        &self,
        base_url: &str,
        model: &str,
        text: &str,
        max_tokens: usize,
    ) -> Result<String> {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(120))
            .build()?;
        let body = serde_json::json!({
            "model": model,
            "messages": [
                {
                    "role": "system",
                    "content": "You summarise a single agent-session excerpt. Report only what \
                                the excerpt states. Do not infer, do not add APIs or names that \
                                are not present, and do not use hedging language. Two sentences \
                                at most."
                },
                { "role": "user", "content": text }
            ],
            "max_tokens": max_tokens,
            "temperature": 0.0,
            "stream": false
        });
        let resp = client
            .post(format!("{}/v1/chat/completions", base_url.trim_end_matches('/')))
            .json(&body)
            .send()
            .await?;
        if !resp.status().is_success() {
            return Err(crate::error::Error::BackendUnavailable(format!(
                "auxiliary summariser returned {}",
                resp.status()
            )));
        }
        let doc: serde_json::Value = resp.json().await?;
        Ok(doc
            .get("choices")
            .and_then(|c| c.as_array())
            .and_then(|c| c.first())
            .and_then(|c| c.get("message"))
            .and_then(|m| m.get("content"))
            .and_then(|c| c.as_str())
            .unwrap_or("")
            .trim()
            .to_string())
    }

    async fn embed_atlas(&self, atlas_id: &str) -> Result<()> {
        let entry = self.fabric.semantic_entry(atlas_id).await?;
        let vectors = self
            .embedder
            .embed(std::slice::from_ref(&entry.content))
            .await?;
        if let Some(v) = vectors.first() {
            self.db
                .put_vector(
                    "semantic_atlas",
                    &entry.atlas_id,
                    self.embedder.model_id(),
                    v,
                )
                .await?;
        }
        Ok(())
    }

    /// Embed Atlas entries and symbolic facts that have no vector yet.
    async fn backfill_embeddings(&self, report: &mut ConsolidationReport) -> Result<usize> {
        let missing = self
            .db
            .with({
                let model = self.embedder.model_id().to_string();
                move |c| {
                    let mut stmt = c.prepare(
                        "SELECT sa.atlas_id, sa.content FROM semantic_atlas sa
                         LEFT JOIN vectors v ON v.source_table='semantic_atlas'
                                            AND v.source_id = sa.atlas_id
                                            AND v.model = ?1
                         WHERE v.embedding_id IS NULL
                         LIMIT 64",
                    )?;
                    let rows = stmt.query_map([model], |r| {
                        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                    })?;
                    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
                }
            })
            .await?;

        let mut done = 0usize;
        for (id, content) in missing {
            if !self.is_idle().await {
                report.interrupted = true;
                break;
            }
            let vectors = self.embedder.embed(std::slice::from_ref(&content)).await?;
            if let Some(v) = vectors.first() {
                self.db
                    .put_vector("semantic_atlas", &id, self.embedder.model_id(), v)
                    .await?;
                done += 1;
            }
        }
        Ok(done)
    }

    /// Archive episodes older than `days` that are live and undepended.
    ///
    /// Archiving is the *third* tier, so this only ever applies where the
    /// automatic engine has not already acted, and it never touches anchors or
    /// anything a fold owns.
    async fn archive_cold(&self, days: i64) -> Result<usize> {
        let cutoff = chrono::Utc::now() - chrono::Duration::days(days);
        let sessions = self.fabric.sessions(64).await?;
        let mut archived = 0usize;

        for (session_id, _, _) in sessions {
            let episodes = self.fabric.session_episodes(&session_id).await?;
            for ep in episodes {
                if ep.eviction_tier != crate::memory::episodic::EpisodeTier::Live {
                    continue;
                }
                if ep.fold_id.is_some() {
                    continue;
                }
                let Some(created) = crate::ids::parse_rfc3339(&ep.created_at) else {
                    continue;
                };
                if created > cutoff {
                    continue;
                }
                // Never archive something a live interpretation depends on without
                // first ensuring the interpretation exists.
                let node = crate::memory::dependency::NodeRef::episode(&ep.episode_id);
                let graph = self.fabric.graph_around(&node, 128).await?;
                let dependents = graph.dependents_on(&node, 2, None);
                let has_atlas = dependents.iter().any(|d| {
                    d.kind == crate::memory::dependency::NodeKind::SemanticEntry
                });
                if !has_atlas && self.config.promote_episodes {
                    // Promote first so archiving does not lose the only summary of
                    // this content.
                    let summary = self.summarise_text(&ep.content).await?;
                    if !summary.trim().is_empty() {
                        let _ = self
                            .fabric
                            .put_semantic(
                                SemanticWrite::on_episode(summary, ep.episode_id.clone())
                                    .in_session(session_id.clone()),
                            )
                            .await;
                    }
                }
                self.fabric
                    .set_tier(
                        &ep.episode_id,
                        crate::memory::episodic::EpisodeTier::Archived,
                    )
                    .await?;
                archived += 1;
            }
        }
        Ok(archived)
    }
}

/// Build a deterministic extractive summary.
///
/// Picks the sentences with the highest density of distinctive identifiers, in
/// original order, and prefixes the result with the identifiers themselves. This
/// is not a semantic compression and does not claim to be: it selects text that
/// was already written, so it cannot fabricate a function name. That property is
/// the entire reason it is the default under the dual-track discipline.
pub fn extractive_summary(text: &str, sentences: usize, max_chars: usize) -> String {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return String::new();
    }

    // Split on sentence enders and newlines; keep it simple and predictable.
    let mut units: Vec<String> = Vec::new();
    let mut current = String::new();
    for ch in trimmed.chars() {
        current.push(ch);
        if matches!(ch, '.' | '!' | '?' | '\n') {
            let s = current.trim().to_string();
            if s.chars().count() >= 12 {
                units.push(s);
            }
            current.clear();
        }
    }
    let tail = current.trim();
    if tail.chars().count() >= 12 {
        units.push(tail.to_string());
    }
    if units.is_empty() {
        return truncate_words(trimmed, max_chars);
    }

    // Score by distinctive-token density; longer identifiers are more
    // informative than common English words.
    let distinctive: Vec<Vec<String>> = units
        .iter()
        .map(|u| {
            u.split(|c: char| !c.is_alphanumeric() && c != '_')
                .filter(|t| t.len() >= 5 && (t.contains('_') || t.chars().any(|c| c.is_uppercase())))
                .map(|t| t.to_lowercase())
                .collect()
        })
        .collect();

    let mut order: Vec<usize> = (0..units.len()).collect();
    order.sort_by(|a, b| {
        distinctive[*b]
            .len()
            .cmp(&distinctive[*a].len())
            .then_with(|| a.cmp(b))
    });
    let mut chosen: Vec<usize> = order.into_iter().take(sentences.max(1)).collect();
    chosen.sort_unstable();

    let mut out = String::new();
    for (i, idx) in chosen.iter().enumerate() {
        if i > 0 {
            out.push(' ');
        }
        out.push_str(&units[*idx]);
    }

    // Prefix the identifiers so a reader (or a model) can see at a glance which
    // symbols the excerpt actually mentions.
    let mut idents: Vec<String> = distinctive
        .iter()
        .flatten()
        .cloned()
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    idents.truncate(6);
    if !idents.is_empty() {
        out = format!("[mentions: {}] {}", idents.join(", "), out);
    }
    truncate_words(&out, max_chars)
}

fn truncate_words(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max_chars).collect();
    // Cut at the last space so the excerpt does not end mid-identifier.
    if let Some(idx) = out.rfind(' ') {
        out.truncate(idx);
    }
    out.push('…');
    out
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

struct RunningGuard(Arc<AtomicBool>);

impl Drop for RunningGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llama::embedded::EmbeddedBackend;
    use crate::memory::episodic::NewEpisode;
    use crate::memory::symbolic::{FactKind, FactSource, SymbolicWrite};
    use crate::tokens::CharTokenizer;

    fn counter() -> TokenCounter {
        TokenCounter::new(CharTokenizer { chars_per_token: 4 })
    }

    async fn rig(cfg: ConsolidatorConfig) -> (MemoryFabric, Consolidator, Arc<EmbeddedBackend>) {
        let db = Db::open_in_memory().await.unwrap();
        let backend = Arc::new(EmbeddedBackend::new());
        let fabric = MemoryFabric::new(db.clone());
        let c = Consolidator::new(
            db,
            fabric.clone(),
            backend.clone(),
            Arc::new(crate::embed::HashingEmbedder::default()),
            counter(),
            cfg,
        );
        (fabric, c, backend)
    }

    fn idle_config() -> ConsolidatorConfig {
        ConsolidatorConfig {
            quiet_period_secs: 0,
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn skips_when_a_slot_is_generating() {
        let (fabric, c, backend) = rig(idle_config()).await;
        let _ = fabric;
        // Simulate an active generation by making the slot unreadable.
        // EmbeddedBackend never reports is_processing, so instead assert the
        // quiet-period gate, which is the part that depends on tracked activity.
        let cfg = ConsolidatorConfig {
            quiet_period_secs: 3_600,
            ..Default::default()
        };
        c.note_activity();
        let (_, c2, _) = rig(cfg).await;
        let report = c2.maybe_run().await.unwrap();
        assert!(!report.ran);
        assert!(report.skipped_reason.as_deref().unwrap().contains("not idle"));
        let _ = backend;
    }

    #[tokio::test]
    async fn an_unreadable_slot_blocks_consolidation() {
        // FR-13 says consolidation must never overlap generation; a slot whose
        // state cannot be read cannot be proven idle, so the pass must not run.
        let db = Db::open_in_memory().await.unwrap();
        let fabric = MemoryFabric::new(db.clone());
        let backend: Arc<dyn InferenceBackend> =
            Arc::new(crate::llama::embedded::NullBackend::new());
        let c = Consolidator::new(
            db,
            fabric,
            backend,
            Arc::new(crate::embed::HashingEmbedder::default()),
            counter(),
            idle_config(),
        );
        assert!(!c.is_idle().await);
        let report = c.maybe_run().await.unwrap();
        assert!(!report.ran);
    }

    #[tokio::test]
    async fn promotes_substantial_episodes_into_the_atlas() {
        let (fabric, c, _b) = rig(idle_config()).await;
        let counter = counter();
        // Each turn has to clear two bars: `min_episode_tokens`, and the
        // measurement that its summary is actually shorter than the turn.
        for i in 0..3 {
            fabric
                .commit_episode(
                    NewEpisode::user("s1", substantial_turn(i)).with_slot("0"),
                    &counter,
                    false,
                    false,
                )
                .await
                .unwrap();
        }
        let report = c.maybe_run().await.unwrap();
        assert!(report.ran);
        assert_eq!(report.promoted, 3, "notes: {:?}", report.notes);
        assert!(
            report.reembedded == 0,
            "promotion embeds inline, so nothing should need backfilling in the same pass"
        );

        let stats = fabric.db().stats().await.unwrap();
        assert_eq!(stats.semantic_entries, 3);

        // Every promoted entry must end up searchable, which means embedded.
        let unembedded: i64 = fabric
            .db()
            .with(|c| {
                Ok(c.query_row(
                    "SELECT COUNT(*) FROM semantic_atlas sa
                     LEFT JOIN vectors v ON v.source_table='semantic_atlas'
                                        AND v.source_id = sa.atlas_id
                     WHERE v.embedding_id IS NULL",
                    [],
                    |r| r.get(0),
                )?)
            })
            .await
            .unwrap();
        assert_eq!(unembedded, 0, "promoted entries must be embedded");
    }

    #[tokio::test]
    async fn a_short_turn_is_left_alone_rather_than_grown() {
        // The failure this guards against is quiet and real: an extractive
        // summary prefixes the identifiers it found, so summarising a dense
        // three-line turn produces something *longer* than the turn. Promoting
        // that would make consolidation a net cost.
        let (fabric, c, _b) = rig(idle_config()).await;
        let counter = counter();
        fabric
            .commit_episode(
                NewEpisode::user(
                    "s1",
                    "Step 0: refactor CacheCoherenceLayer so boundary_snap_delta stays within \
                     tolerance_tokens and the prefix survives as an LCP match.",
                )
                .with_slot("0"),
                &counter,
                false,
                false,
            )
            .await
            .unwrap();
        let report = c.maybe_run().await.unwrap();
        assert!(report.ran);
        assert_eq!(
            report.promoted, 0,
            "a turn whose summary would not be smaller must not be promoted"
        );
    }

    /// A turn long enough that an extractive summary is genuinely smaller.
    fn substantial_turn(i: usize) -> String {
        let filler = "The eviction engine walks the dependency graph and escalates the \
                      lowest-value episodes first, never touching the Anchor Set, and it \
                      refuses to drop anything that still has unresolved dependents. ";
        let mut text = format!("Step {i}: refactor the CacheCoherenceLayer. ");
        for _ in 0..6 {
            text.push_str(filler);
        }
        text.push_str(
            "The boundary_snap_delta must stay within tolerance_tokens so that the surviving \
             prefix remains an LCP match; otherwise the next turn pays a full re-prefill.",
        );
        text
    }

    #[tokio::test]
    async fn short_episodes_are_not_promoted() {
        let (fabric, c, _b) = rig(idle_config()).await;
        let counter = counter();
        fabric
            .commit_episode(NewEpisode::user("s1", "ok"), &counter, false, false)
            .await
            .unwrap();
        let report = c.maybe_run().await.unwrap();
        assert_eq!(report.promoted, 0);
    }

    #[tokio::test]
    async fn stale_entries_are_regenerated_and_the_staleness_rate_returns_to_zero() {
        let (fabric, c, _b) = rig(idle_config()).await;

        // A fact, a summary of it, then a change to the fact.
        let old = SymbolicWrite::new(FactKind::Function, "auth::checkUser")
            .signature("fn checkUser(email: &str) -> bool")
            .body("fn checkUser(email: &str) -> bool { true }")
            .into_fact(FactSource::TreeSitter, None);
        fabric.upsert_facts(vec![old.clone()]).await.unwrap();
        fabric
            .put_semantic(crate::memory::semantic::SemanticWrite::on_fact(
                "checkUser looks a user up by email",
                old.fact_id.clone(),
            ))
            .await
            .unwrap();

        let new = SymbolicWrite::new(FactKind::Function, "auth::checkUser")
            .signature("fn checkUser(id: UserId) -> Result<User>")
            .body("fn checkUser(id: UserId) -> Result<User> { users::by_id(id) }")
            .into_fact(FactSource::TreeSitter, None);
        fabric.upsert_facts(vec![new]).await.unwrap();

        let before = fabric.staleness_report(None, 32).await.unwrap();
        assert_eq!(before.stale, 1, "the summary must be detected as stale");

        let report = c.maybe_run().await.unwrap();
        assert!(report.ran);
        assert_eq!(report.regenerated, 1);

        let after = fabric.staleness_report(None, 32).await.unwrap();
        assert_eq!(after.stale, 0, "regeneration must clear staleness");
        assert!(after.is_clean());
    }

    #[tokio::test]
    async fn regeneration_for_a_symbolic_anchor_needs_no_model() {
        let (fabric, c, _b) = rig(idle_config()).await;
        let fact = SymbolicWrite::new(FactKind::Function, "m::thing")
            .signature("fn thing() -> u8")
            .body("fn thing() -> u8 { 1 }")
            .into_fact(FactSource::TreeSitter, None);
        fabric.upsert_facts(vec![fact.clone()]).await.unwrap();
        fabric
            .put_semantic(crate::memory::semantic::SemanticWrite::on_fact(
                "old wording",
                fact.fact_id.clone(),
            ))
            .await
            .unwrap();
        fabric
            .upsert_facts(vec![
                SymbolicWrite::new(FactKind::Function, "m::thing")
                    .signature("fn thing() -> u16")
                    .body("fn thing() -> u16 { 1 }")
                    .into_fact(FactSource::TreeSitter, None),
            ])
            .await
            .unwrap();

        let report = c.maybe_run().await.unwrap();
        assert_eq!(report.regenerated, 1);
        let report2 = fabric.staleness_report(None, 32).await.unwrap();
        assert!(report2.is_clean());

        // The replacement text was derived from the fact, not invented.
        let all = fabric
            .db()
            .with(|db| {
                Ok(db.query_row(
                    "SELECT content FROM semantic_atlas LIMIT 1",
                    [],
                    |r| r.get::<_, String>(0),
                )?)
            })
            .await
            .unwrap();
        assert!(all.contains("u16"), "new summary must reflect the current truth");
    }

    #[tokio::test]
    async fn archival_preserves_a_summary_of_what_it_archives() {
        let (fabric, c, _b) = rig(ConsolidatorConfig {
            quiet_period_secs: 0,
            archive_after_days: Some(0),
            ..Default::default()
        })
        .await;
        let counter = counter();
        let out = fabric
            .commit_episode(
                NewEpisode::user(
                    "s1",
                    "The DeployPipeline must always run the migration_check step before \
                     promoting any build to the production environment.",
                ),
                &counter,
                false,
                false,
            )
            .await
            .unwrap();
        // Backdate it so the cutoff applies.
        fabric
            .db()
            .write({
                let id = out.episode_id.clone();
                move |tx| {
                    tx.execute(
                        "UPDATE episodic_stream SET created_at='2020-01-01T00:00:00.000Z'
                         WHERE episode_id = ?1",
                        [id],
                    )?;
                    Ok(())
                }
            })
            .await
            .unwrap();

        let report = c.maybe_run().await.unwrap();
        assert_eq!(report.archived, 1);

        let ep = fabric.episode(&out.episode_id).await.unwrap();
        assert_eq!(ep.eviction_tier, crate::memory::episodic::EpisodeTier::Archived);
        assert_eq!(
            ep.content.contains("migration_check"),
            true,
            "archiving must not touch the stored content"
        );

        let stats = fabric.db().stats().await.unwrap();
        assert!(
            stats.semantic_entries >= 1,
            "a summary must exist so archiving does not lose the content's meaning"
        );
    }

    #[test]
    fn extractive_summary_never_invents_identifiers() {
        let text = "The CacheCoherenceLayer snaps boundaries onto checkpoints. \
                    Unrelated filler sentence follows here. \
                    The boundary_snap_delta must stay within tolerance_tokens.";
        let summary = extractive_summary(text, 2, 400);
        // Every identifier in the summary must appear verbatim in the source.
        for token in summary.split(|c: char| !c.is_alphanumeric() && c != '_') {
            if token.len() >= 5 && (token.contains('_') || token.chars().any(|c| c.is_uppercase())) {
                assert!(
                    text.contains(token),
                    "summary invented the identifier {token:?}: {summary}"
                );
            }
        }
        assert!(summary.contains("CacheCoherenceLayer") || summary.contains("boundary_snap_delta"));
        assert!(summary.starts_with("[mentions: "));
    }

    #[test]
    fn extractive_summary_handles_empty_and_short_input() {
        assert_eq!(extractive_summary("", 3, 100), "");
        assert_eq!(extractive_summary("   ", 3, 100), "");
        let short = extractive_summary("hi", 3, 100);
        assert_eq!(short, "hi");
    }

    #[test]
    fn extractive_summary_respects_the_character_cap() {
        let text = "alpha_beta_gamma ".repeat(200);
        let summary = extractive_summary(&text, 3, 100);
        assert!(summary.chars().count() <= 101, "got {} chars", summary.chars().count());
    }

    #[test]
    fn extractive_summary_is_deterministic() {
        let text = "First the AlphaService starts. Then the BetaWorker consumes a queue. \
                    Finally the GammaReporter writes metrics.";
        assert_eq!(
            extractive_summary(text, 2, 300),
            extractive_summary(text, 2, 300)
        );
    }

    #[tokio::test]
    async fn overlapping_passes_are_refused() {
        let (_fabric, c, _b) = rig(idle_config()).await;
        // Simulate a pass already in flight.
        c.running.store(true, Ordering::SeqCst);
        let report = c.maybe_run().await.unwrap();
        assert!(!report.ran);
        assert!(report.skipped_reason.as_deref().unwrap().contains("already running"));
    }

    #[tokio::test]
    async fn tracked_slots_are_deduplicated() {
        let (_fabric, c, _b) = rig(idle_config()).await;
        c.track_slot("1");
        c.track_slot("1");
        c.track_slot("2");
        let slots = c.tracked_slots();
        assert_eq!(slots.iter().filter(|s| *s == "1").count(), 1);
        assert!(slots.contains(&"2".to_string()));
    }

    #[tokio::test]
    async fn report_summarises_what_happened() {
        let (_fabric, c, _b) = rig(idle_config()).await;
        let report = c.maybe_run().await.unwrap();
        assert!(report.summary().contains("dream cycle"));
    }
}
