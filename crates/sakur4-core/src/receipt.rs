//! The Context Ledger Receipt (PRD component C8).
//!
//! G8 asks that token and context spend be *legible*: "every turn, the agent and
//! user can see where the context budget went and why". INN-5 goes further — the
//! receipt is what turns a silent re-prefill and a silent memory loss into a
//! visible, debuggable signal.
//!
//! Two design rules make it trustworthy rather than decorative:
//!
//! 1. **The breakdown is measured, never estimated.** Every number comes from the
//!    same [`PromptParts`] that produced the prompt, counted with the same
//!    [`TokenCounter`]. FR-15's acceptance criterion — that the category sum
//!    matches the real prompt token count — is therefore a property of the code
//!    path, not a test that might drift.
//! 2. **The cache verdict carries its evidence.** "partial-reuse" is never stated
//!    alone; it comes with the boundary, the checkpoint it snapped to, and how far
//!    the cut moved. A status without its arithmetic is a status nobody can act on.
//!
//! The receipt renders as plain text that is readable with no client-side
//! formatting at all — FR-15's second criterion — because the primary consumer is
//! a human staring at a slow turn.

use crate::error::Result;
use crate::ids::{new_id, now_rfc3339};
use crate::prompt::{PromptParts, RenderedPart};
use crate::store::Db;
use crate::tokens::{TokenCounter, TokenizerKind};

/// Token counts by category.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Breakdown {
    pub system_prompt: usize,
    pub pinned_anchors: usize,
    pub retrieved_memory: usize,
    pub repo_map: usize,
    pub raw_recent_history: usize,
    pub tool_schemas: usize,
    pub fold_summaries: usize,
    pub other: usize,
}

impl Breakdown {
    /// Build from the assembled prompt.
    pub fn from_parts(parts: &PromptParts, counter: &TokenCounter) -> Self {
        let mut b = Breakdown::default();
        for (part, tokens) in parts.breakdown(counter) {
            match part {
                RenderedPart::System => b.system_prompt += tokens,
                RenderedPart::Anchors => b.pinned_anchors += tokens,
                RenderedPart::Recall => b.retrieved_memory += tokens,
                RenderedPart::RepoMap => b.repo_map += tokens,
                RenderedPart::Timeline => b.raw_recent_history += tokens,
                RenderedPart::ToolSchemas => b.tool_schemas += tokens,
                RenderedPart::Folds => b.fold_summaries += tokens,
                RenderedPart::Extra => b.other += tokens,
            }
        }
        b
    }

    pub fn total(&self) -> usize {
        self.system_prompt
            + self.pinned_anchors
            + self.retrieved_memory
            + self.repo_map
            + self.raw_recent_history
            + self.tool_schemas
            + self.fold_summaries
            + self.other
    }

    /// Categories in descending size — where the budget actually went.
    pub fn by_size(&self) -> Vec<(&'static str, usize)> {
        let mut v = vec![
            ("system prompt", self.system_prompt),
            ("pinned anchors", self.pinned_anchors),
            ("retrieved memory", self.retrieved_memory),
            ("repo map", self.repo_map),
            ("raw recent history", self.raw_recent_history),
            ("tool schemas", self.tool_schemas),
            ("fold summaries", self.fold_summaries),
            ("other", self.other),
        ];
        v.retain(|(_, n)| *n > 0);
        v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
        v
    }

    /// A proportional bar, for the plain-text render.
    fn bar(&self, value: usize, width: usize) -> String {
        let total = self.total().max(1);
        let filled = ((value as f64 / total as f64) * width as f64).round() as usize;
        format!("{}{}", "█".repeat(filled.min(width)), "·".repeat(width.saturating_sub(filled)))
    }
}

/// One turn's receipt.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Receipt {
    pub receipt_id: String,
    pub session_id: String,
    pub slot_id: Option<String>,
    pub turn: i64,
    pub breakdown: Breakdown,
    /// Measured tokens of the assembled prompt.
    pub total_tokens: usize,
    /// The window this prompt was planned against.
    pub context_window: usize,
    pub cache_status: String,
    pub cache_detail: String,
    /// Measured prompt-eval time for this turn, when the backend reported it.
    pub prompt_eval_ms: Option<i64>,
    pub prompt_tokens_reused: Option<usize>,
    pub prompt_tokens_prefilled: Option<usize>,
    /// What the eviction engine did this turn, if anything.
    pub eviction: Option<EvictionSummary>,
    pub tokenizer: String,
    pub backend: String,
    pub created_at: String,
}

/// What an eviction plan did.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct EvictionSummary {
    pub applied: usize,
    pub tokens_reclaimed: usize,
    pub from_tier_live_to_masked: usize,
    pub from_tier_masked_to_referenced: usize,
    pub from_tier_referenced_to_archived: usize,
    pub from_tier_archived_to_dropped: usize,
    pub snapshot_taken: bool,
    pub boundary: Option<String>,
}

impl EvictionSummary {
    /// Condense a plan's updates into per-transition counts.
    pub fn from_plan(plan: &crate::evict::EvictionPlan, snapshot_taken: bool) -> Self {
        use crate::memory::episodic::EpisodeTier as T;
        let mut s = EvictionSummary {
            applied: plan.updates.len(),
            tokens_reclaimed: plan.planned_savings,
            from_tier_live_to_masked: 0,
            from_tier_masked_to_referenced: 0,
            from_tier_referenced_to_archived: 0,
            from_tier_archived_to_dropped: 0,
            snapshot_taken,
            boundary: plan.coherence.as_ref().map(|c| c.summary()),
        };
        for u in &plan.updates {
            match (u.from, u.to) {
                (T::Live, T::Masked) => s.from_tier_live_to_masked += 1,
                (T::Masked, T::Referenced) => s.from_tier_masked_to_referenced += 1,
                (T::Referenced, T::Archived) => s.from_tier_referenced_to_archived += 1,
                (T::Archived, T::Dropped) => s.from_tier_archived_to_dropped += 1,
                _ => {}
            }
        }
        s
    }
}

impl Receipt {
    /// Build a receipt from the prompt that is about to be sent.
    pub fn build(
        session_id: &str,
        slot_id: Option<&str>,
        turn: i64,
        parts: &PromptParts,
        counter: &TokenCounter,
        context_window: usize,
    ) -> Self {
        let breakdown = Breakdown::from_parts(parts, counter);
        // Measure the whole prompt independently of the per-category sum: if the
        // two ever disagree by more than rounding, the receipt should show the
        // measured number, not the flattering one.
        let measured = counter.count(&parts.render()).get();
        Self {
            receipt_id: new_id("rcpt"),
            session_id: session_id.to_string(),
            slot_id: slot_id.map(String::from),
            turn,
            breakdown,
            total_tokens: measured,
            context_window,
            cache_status: "unknown".into(),
            cache_detail: String::new(),
            prompt_eval_ms: None,
            prompt_tokens_reused: None,
            prompt_tokens_prefilled: None,
            eviction: None,
            tokenizer: counter.kind().as_str().to_string(),
            backend: String::new(),
            created_at: now_rfc3339(),
        }
    }

    pub fn with_cache(mut self, status: &str, detail: impl Into<String>) -> Self {
        self.cache_status = status.to_string();
        self.cache_detail = detail.into();
        self
    }

    pub fn with_cache_numbers(mut self, reused: usize, prefilled: usize) -> Self {
        self.prompt_tokens_reused = Some(reused);
        self.prompt_tokens_prefilled = Some(prefilled);
        self
    }

    pub fn with_prompt_eval_ms(mut self, ms: i64) -> Self {
        self.prompt_eval_ms = Some(ms);
        self
    }

    pub fn with_eviction(mut self, summary: EvictionSummary) -> Self {
        self.eviction = Some(summary);
        self
    }

    pub fn with_backend(mut self, name: impl Into<String>) -> Self {
        self.backend = name.into();
        self
    }

    /// Fill ratio of the window.
    pub fn fill_ratio(&self) -> f64 {
        if self.context_window == 0 {
            0.0
        } else {
            (self.total_tokens as f64 / self.context_window as f64).clamp(0.0, 1.0)
        }
    }

    /// Whether the category sum agrees with the measured total, within one
    /// percent plus two tokens. This is FR-15's acceptance criterion, evaluated
    /// on every receipt rather than only in tests.
    pub fn breakdown_is_consistent(&self) -> bool {
        let sum = self.breakdown.total();
        let tolerance = (self.total_tokens / 100).max(2);
        sum.abs_diff(self.total_tokens) <= tolerance
    }

    /// Render as human-readable text, with no client-side formatting required.
    pub fn render(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "Context Ledger Receipt · turn {} · session {}\n",
            self.turn, self.session_id
        ));
        out.push_str(&format!(
            "  window {}/{} tokens ({:.0}% full) · tokenizer {} · backend {}\n",
            self.total_tokens,
            self.context_window,
            self.fill_ratio() * 100.0,
            self.tokenizer,
            if self.backend.is_empty() { "n/a" } else { &self.backend }
        ));

        out.push_str("  where the budget went:\n");
        let width = 18;
        for (name, tokens) in self.breakdown.by_size() {
            let pct = if self.total_tokens == 0 {
                0.0
            } else {
                tokens as f64 / self.total_tokens as f64 * 100.0
            };
            out.push_str(&format!(
                "    {:<20} {:>7}  {:>5.1}%  {}\n",
                name,
                tokens,
                pct,
                self.breakdown.bar(tokens, width)
            ));
        }
        if !self.breakdown_is_consistent() {
            out.push_str(&format!(
                "    NOTE: category sum {} differs from measured total {} beyond tolerance\n",
                self.breakdown.total(),
                self.total_tokens
            ));
        }

        out.push_str(&format!("  cache: {}\n", self.cache_status));
        if !self.cache_detail.is_empty() {
            out.push_str(&format!("    {}\n", self.cache_detail));
        }
        if let (Some(reused), Some(prefilled)) =
            (self.prompt_tokens_reused, self.prompt_tokens_prefilled)
        {
            let ratio = if self.total_tokens == 0 {
                0.0
            } else {
                reused as f64 / self.total_tokens as f64 * 100.0
            };
            out.push_str(&format!(
                "    {reused} tokens reused / {prefilled} prefilled ({ratio:.0}% saved)\n"
            ));
        }
        if let Some(ms) = self.prompt_eval_ms {
            out.push_str(&format!("  prompt eval: {ms} ms\n"));
        }
        if let Some(e) = &self.eviction {
            out.push_str(&format!(
                "  eviction: {} episode(s), {} tokens reclaimed{} \
                 (live→masked {}, masked→referenced {}, referenced→archived {}, archived→dropped {})\n",
                e.applied,
                e.tokens_reclaimed,
                if e.snapshot_taken {
                    ", pre-rewrite snapshot taken"
                } else {
                    ""
                },
                e.from_tier_live_to_masked,
                e.from_tier_masked_to_referenced,
                e.from_tier_referenced_to_archived,
                e.from_tier_archived_to_dropped
            ));
            if let Some(b) = &e.boundary {
                out.push_str(&format!("    {b}\n"));
            }
        }
        out
    }
}

/// Persistence and aggregation for receipts.
#[derive(Clone)]
pub struct ReceiptLog {
    db: Db,
}

impl ReceiptLog {
    pub fn new(db: Db) -> Self {
        Self { db }
    }

    /// Persist a receipt and return its id.
    pub async fn record(&self, receipt: &Receipt) -> Result<String> {
        let row = receipt.clone();
        self.db
            .write(move |tx| {
                tx.execute(
                    "INSERT INTO receipt
                        (receipt_id, session_id, slot_id, turn, total_tokens, breakdown_json,
                         cache_status, cache_detail, prompt_eval_ms, eviction_json, created_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                    rusqlite::params![
                        row.receipt_id,
                        row.session_id,
                        row.slot_id,
                        row.turn,
                        row.total_tokens as i64,
                        serde_json::to_string(&row.breakdown)?,
                        row.cache_status,
                        row.cache_detail,
                        row.prompt_eval_ms,
                        match &row.eviction {
                            Some(e) => Some(serde_json::to_string(e)?),
                            None => None,
                        },
                        row.created_at,
                    ],
                )?;
                Ok(())
            })
            .await?;
        Ok(receipt.receipt_id.clone())
    }

    /// The most recent receipt for a session.
    pub async fn latest(&self, session_id: &str) -> Result<Option<Receipt>> {
        let session = session_id.to_string();
        let row: Option<ReceiptRow> = self
            .db
            .with(move |c| {
                Ok(c.query_row(
                    "SELECT receipt_id, session_id, slot_id, turn, total_tokens, breakdown_json,
                            cache_status, cache_detail, prompt_eval_ms, eviction_json, created_at
                     FROM receipt WHERE session_id = ?1 ORDER BY turn DESC LIMIT 1",
                    [session],
                    map_receipt_row,
                )
                .ok())
            })
            .await?;
        row.map(row_to_receipt).transpose()
    }

    /// Receipt history for a session, oldest first (FR/NFR-14).
    pub async fn history(&self, session_id: &str, limit: usize) -> Result<Vec<Receipt>> {
        let session = session_id.to_string();
        let limit = limit as i64;
        let rows = self
            .db
            .with(move |c| {
                let mut stmt = c.prepare(
                    "SELECT receipt_id, session_id, slot_id, turn, total_tokens, breakdown_json,
                            cache_status, cache_detail, prompt_eval_ms, eviction_json, created_at
                     FROM receipt WHERE session_id = ?1 ORDER BY turn ASC LIMIT ?2",
                )?;
                let rows = stmt.query_map(rusqlite::params![session, limit], map_receipt_row)?;
                Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
            })
            .await?;
        rows.into_iter().map(row_to_receipt).collect()
    }

    /// Next turn number for a session.
    pub async fn next_turn(&self, session_id: &str) -> Result<i64> {
        let session = session_id.to_string();
        self.db
            .with(move |c| {
                Ok(c.query_row(
                    "SELECT COALESCE(MAX(turn), 0) + 1 FROM receipt WHERE session_id = ?1",
                    [session],
                    |r| r.get(0),
                )?)
            })
            .await
    }

    /// Aggregate cache and latency statistics — the PRD's leading indicators.
    pub async fn stats(&self, session_id: Option<&str>) -> Result<ReceiptStats> {
        let session = session_id.map(String::from);
        self.db
            .with(move |c| {
                let mut stmt = c.prepare(
                    "SELECT cache_status, prompt_eval_ms, total_tokens
                     FROM receipt WHERE (?1 IS NULL OR session_id = ?1)",
                )?;
                let rows = stmt.query_map(rusqlite::params![session], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, Option<i64>>(1)?, r.get::<_, i64>(2)?))
                })?;

                let mut s = ReceiptStats::default();
                let mut latencies: Vec<i64> = Vec::new();
                for row in rows {
                    let (status, ms, tokens) = row?;
                    s.turns += 1;
                    s.total_tokens += tokens as usize;
                    match status.as_str() {
                        "partial-reuse" => s.partial_reuse += 1,
                        "warm-restored" => s.warm_restored += 1,
                        "full-re-prefill" => s.full_re_prefill += 1,
                        "cold" => s.cold += 1,
                        _ => s.unknown += 1,
                    }
                    if let Some(ms) = ms {
                        latencies.push(ms);
                    }
                }
                latencies.sort_unstable();
                s.prompt_eval_samples = latencies.len();
                if !latencies.is_empty() {
                    s.prompt_eval_ms_avg =
                        latencies.iter().sum::<i64>() as f64 / latencies.len() as f64;
                    let idx = ((latencies.len() as f64) * 0.95).ceil() as usize;
                    s.prompt_eval_ms_p95 =
                        latencies[idx.saturating_sub(1).min(latencies.len() - 1)];
                }
                Ok(s)
            })
            .await
    }
}

/// Aggregated receipt statistics.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct ReceiptStats {
    pub turns: usize,
    pub partial_reuse: usize,
    pub warm_restored: usize,
    pub full_re_prefill: usize,
    pub cold: usize,
    pub unknown: usize,
    pub total_tokens: usize,
    pub prompt_eval_samples: usize,
    pub prompt_eval_ms_avg: f64,
    pub prompt_eval_ms_p95: i64,
}

impl ReceiptStats {
    /// The PRD's headline indicator: share of compactions that reused a prefix.
    pub fn reuse_ratio(&self) -> f64 {
        let considered = self.partial_reuse + self.warm_restored + self.full_re_prefill;
        if considered == 0 {
            0.0
        } else {
            (self.partial_reuse + self.warm_restored) as f64 / considered as f64
        }
    }

    /// Whether G1's 80% target is currently being met, over a meaningful sample.
    pub fn meets_g1(&self) -> Option<bool> {
        let considered = self.partial_reuse + self.warm_restored + self.full_re_prefill;
        if considered < 5 { None } else { Some(self.reuse_ratio() >= 0.80) }
    }

    pub fn render(&self) -> String {
        let g1 = match self.meets_g1() {
            Some(true) => "meets G1 (≥80% reuse)",
            Some(false) => "below G1 target (≥80% reuse)",
            None => "insufficient data for G1 (needs ≥5 compaction events)",
        };
        format!(
            "turns={} reuse={}/{} ({:.0}%) partial={} warm={} full-reprefill={} cold={} · \
             prompt-eval avg={:.0}ms p95={}ms over {} sample(s) · {}",
            self.turns,
            self.partial_reuse + self.warm_restored,
            self.partial_reuse + self.warm_restored + self.full_re_prefill,
            self.reuse_ratio() * 100.0,
            self.partial_reuse,
            self.warm_restored,
            self.full_re_prefill,
            self.cold,
            self.prompt_eval_ms_avg,
            self.prompt_eval_ms_p95,
            self.prompt_eval_samples,
            g1
        )
    }
}

/// The columns every receipt read selects, in order.
///
/// Factored into a type alias and a mapper because three queries share the same
/// eleven columns, and a column-order mismatch between hand-written tuple
/// destructuring sites is exactly the bug that stays invisible until someone
/// reads a `created_at` where a `cache_status` was expected.
type ReceiptRow = (
    String,
    String,
    Option<String>,
    i64,
    i64,
    String,
    String,
    String,
    Option<i64>,
    Option<String>,
    String,
);

fn map_receipt_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<ReceiptRow> {
    Ok((
        r.get(0)?,
        r.get(1)?,
        r.get(2)?,
        r.get(3)?,
        r.get(4)?,
        r.get(5)?,
        r.get(6)?,
        r.get(7)?,
        r.get(8)?,
        r.get(9)?,
        r.get(10)?,
    ))
}

#[allow(clippy::type_complexity)]
fn row_to_receipt(row: ReceiptRow) -> Result<Receipt> {
    let breakdown: Breakdown = serde_json::from_str(&row.5)?;
    let eviction: Option<EvictionSummary> = match row.9 {
        Some(raw) => serde_json::from_str(&raw).ok(),
        None => None,
    };
    Ok(Receipt {
        receipt_id: row.0,
        session_id: row.1,
        slot_id: row.2,
        turn: row.3,
        total_tokens: row.4 as usize,
        breakdown,
        cache_status: row.6,
        cache_detail: row.7,
        prompt_eval_ms: row.8,
        eviction,
        created_at: row.10,
        context_window: 0,
        prompt_tokens_reused: None,
        prompt_tokens_prefilled: None,
        tokenizer: String::new(),
        backend: String::new(),
    })
}

/// Convenience for the MCP surface: build, enrich and persist in one call.
pub struct ReceiptBuilder {
    receipt: Receipt,
}

impl ReceiptBuilder {
    pub fn new(
        session_id: &str,
        slot_id: Option<&str>,
        turn: i64,
        parts: &PromptParts,
        counter: &TokenCounter,
        context_window: usize,
    ) -> Self {
        Self { receipt: Receipt::build(session_id, slot_id, turn, parts, counter, context_window) }
    }

    pub fn cache(
        mut self,
        status: &str,
        detail: impl Into<String>,
        reused: usize,
        prefilled: usize,
    ) -> Self {
        self.receipt.cache_status = status.to_string();
        self.receipt.cache_detail = detail.into();
        self.receipt.prompt_tokens_reused = Some(reused);
        self.receipt.prompt_tokens_prefilled = Some(prefilled);
        self
    }

    pub fn eviction(mut self, summary: EvictionSummary) -> Self {
        self.receipt.eviction = Some(summary);
        self
    }

    pub fn prompt_eval_ms(mut self, ms: i64) -> Self {
        self.receipt.prompt_eval_ms = Some(ms);
        self
    }

    pub fn backend(mut self, name: impl Into<String>) -> Self {
        self.receipt.backend = name.into();
        self
    }

    pub fn tokenizer_kind(mut self, kind: TokenizerKind) -> Self {
        self.receipt.tokenizer = kind.as_str().to_string();
        self
    }

    pub fn build(self) -> Receipt {
        self.receipt
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tokens::CharTokenizer;

    fn counter() -> TokenCounter {
        TokenCounter::new(CharTokenizer { chars_per_token: 4 })
    }

    fn parts() -> PromptParts {
        PromptParts::new()
            .with_system("you are a careful coding agent")
            .with_anchors("[SAFETY CONSTRAINT] never force-push to main")
            .with_recall("recall: the validator lives in src/auth.rs")
            .with_repo_map("src/auth.rs\n  fn validate")
            .with_timeline("<user> refactor auth\n<assistant> on it\n")
            .with_tool_schemas("{\"name\":\"read_file\"}")
    }

    #[test]
    fn breakdown_matches_the_measured_prompt_within_tolerance() {
        let c = counter();
        let r = Receipt::build("s1", Some("0"), 1, &parts(), &c, 32_768);
        assert!(
            r.breakdown_is_consistent(),
            "sum {} vs measured {}",
            r.breakdown.total(),
            r.total_tokens
        );
    }

    #[test]
    fn every_category_is_populated_by_the_sample_prompt() {
        let c = counter();
        let b = Breakdown::from_parts(&parts(), &c);
        assert!(b.system_prompt > 0);
        assert!(b.pinned_anchors > 0);
        assert!(b.retrieved_memory > 0);
        assert!(b.repo_map > 0);
        assert!(b.raw_recent_history > 0);
        assert!(b.tool_schemas > 0);
        assert_eq!(b.fold_summaries, 0);
    }

    #[test]
    fn by_size_is_descending() {
        let b = Breakdown::from_parts(&parts(), &counter());
        let sizes = b.by_size();
        for pair in sizes.windows(2) {
            assert!(pair[0].1 >= pair[1].1);
        }
    }

    #[test]
    fn render_is_readable_without_any_client_formatting() {
        let c = counter();
        let r = Receipt::build("s1", Some("0"), 3, &parts(), &c, 32_768)
            .with_cache("partial-reuse", "snapped 128 tokens back onto an in-memory checkpoint")
            .with_cache_numbers(9_000, 1_200)
            .with_prompt_eval_ms(412)
            .with_backend("embedded");
        let text = r.render();
        assert!(text.contains("Context Ledger Receipt"));
        assert!(text.contains("where the budget went"));
        assert!(text.contains("pinned anchors"));
        assert!(text.contains("partial-reuse"));
        assert!(text.contains("9000 tokens reused"));
        assert!(text.contains("412 ms"));
        // No ANSI escapes, no markdown tables, no JSON.
        assert!(!text.contains('\u{1b}'));
        assert!(!text.contains("```"));
    }

    #[test]
    fn fill_ratio_is_bounded_and_defined_for_a_zero_window() {
        let c = counter();
        let mut r = Receipt::build("s1", None, 1, &parts(), &c, 32_768);
        assert!(r.fill_ratio() >= 0.0 && r.fill_ratio() <= 1.0);
        r.context_window = 0;
        assert_eq!(r.fill_ratio(), 0.0);
    }

    #[test]
    fn eviction_summary_counts_each_transition() {
        use crate::cache::BoundaryPlan;
        use crate::evict::{EvictionPlan, Pressure, TierUpdate};
        use crate::memory::episodic::EpisodeTier as T;

        let plan = EvictionPlan {
            session_id: "s1".into(),
            slot_id: "0".into(),
            pressure: Pressure::Compacting,
            budget: 32_768,
            threshold: 24_576,
            target: 18_000,
            live_tokens: 30_000,
            anchor_tokens: 100,
            fixed_tokens: 500,
            updates: vec![
                TierUpdate {
                    episode_id: "a".into(),
                    from: T::Live,
                    to: T::Masked,
                    tokens_before: 900,
                    tokens_after: 40,
                    reason: "test".into(),
                },
                TierUpdate {
                    episode_id: "b".into(),
                    from: T::Masked,
                    to: T::Referenced,
                    tokens_before: 40,
                    tokens_after: 10,
                    reason: "test".into(),
                },
            ],
            planned_savings: 890,
            retained_prefix_tokens: 5_000,
            coherence: Some(BoundaryPlan::full_rewrite(5_000, "test")),
            notes: vec![],
        };
        let s = EvictionSummary::from_plan(&plan, true);
        assert_eq!(s.applied, 2);
        assert_eq!(s.tokens_reclaimed, 890);
        assert_eq!(s.from_tier_live_to_masked, 1);
        assert_eq!(s.from_tier_masked_to_referenced, 1);
        assert!(s.snapshot_taken);
        assert!(s.boundary.is_some());
    }

    #[tokio::test]
    async fn receipts_persist_and_aggregate() {
        let db = Db::open_in_memory().await.unwrap();
        let log = ReceiptLog::new(db);
        let c = counter();
        for (turn, status) in [
            (1, "partial-reuse"),
            (2, "partial-reuse"),
            (3, "full-re-prefill"),
            (4, "partial-reuse"),
            (5, "warm-restored"),
            (6, "partial-reuse"),
        ] {
            let r = Receipt::build("s1", Some("0"), turn, &parts(), &c, 32_768)
                .with_cache(status, "test")
                .with_prompt_eval_ms(100 * turn);
            log.record(&r).await.unwrap();
        }

        let latest = log.latest("s1").await.unwrap().unwrap();
        assert_eq!(latest.turn, 6);
        assert_eq!(log.next_turn("s1").await.unwrap(), 7);

        let history = log.history("s1", 10).await.unwrap();
        assert_eq!(history.len(), 6);
        assert_eq!(history[0].turn, 1);

        let stats = log.stats(Some("s1")).await.unwrap();
        assert_eq!(stats.turns, 6);
        assert_eq!(stats.partial_reuse, 4);
        assert_eq!(stats.warm_restored, 1);
        assert_eq!(stats.full_re_prefill, 1);
        assert!((stats.reuse_ratio() - 5.0 / 6.0).abs() < 1e-9);
        assert_eq!(stats.meets_g1(), Some(true));
        assert!(stats.render().contains("meets G1"));
        assert_eq!(stats.prompt_eval_ms_p95, 600);
    }

    #[tokio::test]
    async fn g1_verdict_is_withheld_without_enough_samples() {
        let db = Db::open_in_memory().await.unwrap();
        let log = ReceiptLog::new(db);
        let c = counter();
        for turn in 1..=2 {
            log.record(
                &Receipt::build("s1", None, turn, &parts(), &c, 32_768)
                    .with_cache("full-re-prefill", "test"),
            )
            .await
            .unwrap();
        }
        let stats = log.stats(None).await.unwrap();
        assert_eq!(stats.meets_g1(), None);
        assert!(stats.render().contains("insufficient data"));
        assert_eq!(stats.reuse_ratio(), 0.0);
    }

    #[tokio::test]
    async fn latest_is_none_for_an_unknown_session() {
        let db = Db::open_in_memory().await.unwrap();
        let log = ReceiptLog::new(db);
        assert!(log.latest("nope").await.unwrap().is_none());
    }
}
