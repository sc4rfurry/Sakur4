//! Boundary plans: the Cache-Coherence Layer's decision record.
//!
//! A [`BoundaryPlan`] is deliberately a *data* structure rather than an action.
//! The eviction engine proposes a cut, the coherence layer answers with a plan,
//! and the engine decides whether to accept it. That split is what keeps the two
//! components independently testable — and it is what makes the receipt able to
//! print exactly why a turn was slow.

use crate::llama::SnapReason;

/// The verdict on a compaction's cache behaviour.
///
/// These five values are the vocabulary of the Context Ledger Receipt's cache
/// field (FR-15) and the numerator/denominator of the PRD's headline leading
/// indicator ("percentage of compaction events resolved via partial-prefix
/// reuse vs. full re-prefill").
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CacheStatus {
    /// Nothing is known about this slot yet.
    Unknown,
    /// The slot holds no usable state; the whole prompt will be evaluated.
    Cold,
    /// No checkpoint could be aligned to; the prompt is a materially different
    /// token sequence and llama.cpp will re-prefill it in full.
    FullRePrefill,
    /// The cut was aligned to an existing checkpoint, so a prefix survives and
    /// the next turn's prefill is only the suffix.
    PartialReuse,
    /// A saved slot was restored from disk before this turn.
    WarmRestored,
}

impl CacheStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            CacheStatus::Unknown => "unknown",
            CacheStatus::Cold => "cold",
            CacheStatus::FullRePrefill => "full-re-prefill",
            CacheStatus::PartialReuse => "partial-reuse",
            CacheStatus::WarmRestored => "warm-restored",
        }
    }

    /// Whether this outcome counts as a win for G1's 80% target.
    pub fn is_reuse(self) -> bool {
        matches!(self, CacheStatus::PartialReuse | CacheStatus::WarmRestored)
    }

    /// A short human phrase for the receipt.
    pub fn headline(self) -> &'static str {
        match self {
            CacheStatus::Unknown => "cache state unknown",
            CacheStatus::Cold => "cold slot — full prefill expected",
            CacheStatus::FullRePrefill => "FULL RE-PREFILL — compaction broke the prefix",
            CacheStatus::PartialReuse => "partial reuse — prefix survived compaction",
            CacheStatus::WarmRestored => "warm restored from a saved slot",
        }
    }
}

/// What the layer recommends the caller do next.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case", tag = "action")]
pub enum CoherenceAction {
    /// Apply the eviction at the aligned position.
    EvictAtAlignedBoundary,
    /// Apply the eviction, but first persist the pre-rewrite KV state.
    SnapshotThenEvict,
    /// Apply the eviction at the requested position and accept the re-prefill.
    EvictAndAcceptReprefill,
    /// Do not evict: nothing would be gained.
    Skip,
}

/// The decision.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct BoundaryPlan {
    /// Where the eviction engine asked to cut, in live-window tokens.
    pub requested_cut: i64,
    /// Where the cut will actually fall.
    pub aligned_cut: Option<i64>,
    /// Closest checkpoint at or before the requested cut, whether or not it was
    /// usable — reported so a human can see how far off the ring was.
    pub nearest_cut: Option<i64>,
    pub status: CacheStatus,
    pub action: CoherenceAction,
    /// Why the cut moved (or did not).
    pub reason: String,
    /// Structured version of `reason`, when the cut was snapped.
    pub snap: Option<SnapReason>,
    /// How many checkpoints were considered.
    pub candidates_considered: usize,
    /// Whether the model's checkpoints carry only partial state.
    pub partial_state_only: bool,
    /// Tolerance in force when the decision was made.
    pub tolerance_tokens: i64,
    /// Whether preserving a prefix would actually let the server reuse anything.
    ///
    /// False only when there is no boundary the server could rewind to at all — an
    /// unreachable backend, no checkpoints, or only partial-state checkpoints. A
    /// preserved prefix is still preserved and still a prefix of the next prompt in
    /// that case, but the server cannot match it, so reporting reuse would be
    /// false. The distinction is what keeps G1's headline metric honest.
    pub pairs_with_cache: bool,
}

impl BoundaryPlan {
    /// A plan that aligns the cut onto a checkpoint.
    pub fn aligned(
        requested_cut: i64,
        aligned_cut: i64,
        snap: SnapReason,
        candidates_considered: usize,
        partial_state_only: bool,
    ) -> Self {
        Self {
            requested_cut,
            aligned_cut: Some(aligned_cut),
            nearest_cut: Some(aligned_cut),
            status: CacheStatus::PartialReuse,
            // An aligned boundary is always applied at the aligned position; the
            // delta is recorded in the reason rather than changing the action, so
            // callers have one thing to branch on when they commit the plan.
            action: CoherenceAction::EvictAtAlignedBoundary,
            reason: format!(
                "{} — the surviving prefix stays LCP-matchable, so the next turn prefills \
                 only the suffix",
                snap.describe()
            ),
            snap: Some(snap),
            candidates_considered,
            partial_state_only,
            tolerance_tokens: 0,
            // A checkpoint was found and snapped to, so the server can rewind here.
            pairs_with_cache: true,
        }
    }

    /// A plan that must rewrite, with the nearest checkpoint recorded for audit.
    pub fn unaligned(requested_cut: i64, nearest: Option<i64>, tolerance: i64) -> Self {
        Self::unaligned_reason(
            requested_cut,
            nearest,
            tolerance,
            "no checkpoint is close enough to the requested boundary",
        )
    }

    /// As [`BoundaryPlan::unaligned`], with a caller-supplied explanation.
    pub fn unaligned_reason(
        requested_cut: i64,
        nearest: Option<i64>,
        tolerance: i64,
        why: &str,
    ) -> Self {
        let gap = nearest.map(|n| requested_cut - n);
        Self {
            requested_cut,
            aligned_cut: None,
            nearest_cut: nearest,
            status: CacheStatus::FullRePrefill,
            action: CoherenceAction::SnapshotThenEvict,
            reason: match (nearest, gap) {
                (Some(n), Some(g)) if g > 0 => format!(
                    "nearest checkpoint is {g} tokens before the requested cut (at {n}), outside \
                     the {tolerance}-token tolerance; the pre-rewrite state will be saved so the \
                     prefill is never paid twice"
                ),
                (Some(n), _) => {
                    format!("the checkpoint ring is ahead of the requested cut (at {n}); {why}")
                }
                _ => format!("{why}; the pre-rewrite state will be saved before the rewrite"),
            },
            snap: None,
            candidates_considered: 0,
            partial_state_only: false,
            tolerance_tokens: tolerance,
            // No boundary could be aligned, so a preserved prefix buys nothing.
            pairs_with_cache: false,
        }
    }

    /// A plan for a cut the retained ring has already moved past.
    ///
    /// llama.cpp matches the *longest common prefix*, so a slot holding more
    /// tokens than the cut still contributes its matching prefix — but the tokens
    /// beyond the cut are wasted, and no prefill is saved. Reporting this as
    /// `FullRePrefill` rather than `PartialReuse` is the difference between a
    /// metric that means something and one that flatters the system.
    pub fn past_the_ring(
        requested_cut: i64,
        next_checkpoint: i64,
        candidates_considered: usize,
        partial_state_only: bool,
    ) -> Self {
        Self {
            requested_cut,
            aligned_cut: None,
            nearest_cut: Some(next_checkpoint),
            status: CacheStatus::FullRePrefill,
            action: CoherenceAction::SnapshotThenEvict,
            reason: format!(
                "the retained checkpoint ring starts at {next_checkpoint}, past the requested \
                 boundary of {requested_cut}; nothing cached is reusable at this boundary. \
                 Aligning the boundary forward onto {next_checkpoint} would reuse it, at the \
                 cost of keeping more context live."
            ),
            snap: None,
            candidates_considered,
            partial_state_only,
            tolerance_tokens: 0,
            // The ring is ahead of the cut, so the tokens below it are gone. The
            // *caller* may still choose to align forward onto `next_checkpoint`;
            // that decision is recorded by the resulting plan, not this one.
            pairs_with_cache: false,
        }
    }

    /// The distance from the retained cache's start to this boundary, when the
    /// cache is ahead of the cut.
    pub fn ring_lead(&self) -> Option<i64> {
        match (self.aligned_cut, self.nearest_cut) {
            (None, Some(n)) if n > self.requested_cut => Some(n - self.requested_cut),
            _ => None,
        }
    }

    /// A plan for a backend that cannot participate in coherence at all.
    pub fn full_rewrite(requested_cut: i64, reason: impl Into<String>) -> Self {
        Self {
            requested_cut,
            aligned_cut: None,
            nearest_cut: None,
            status: CacheStatus::FullRePrefill,
            action: CoherenceAction::EvictAndAcceptReprefill,
            reason: reason.into(),
            snap: None,
            candidates_considered: 0,
            partial_state_only: false,
            tolerance_tokens: 0,
            // Nothing to align to. A prefix may still be preserved, but the server
            // cannot match it, so no reuse can be claimed.
            pairs_with_cache: false,
        }
    }

    /// How far the cut moved.
    pub fn delta(&self) -> i64 {
        match (self.requested_cut, self.aligned_cut) {
            (req, Some(aligned)) => req - aligned,
            _ => 0,
        }
    }

    /// Whether the plan resolved as a win for G1.
    pub fn is_reuse(&self) -> bool {
        self.status.is_reuse()
    }

    /// Whether a snapshot should be taken before applying the plan.
    pub fn wants_snapshot(&self) -> bool {
        matches!(self.action, CoherenceAction::SnapshotThenEvict)
    }

    /// One-line rendering for the receipt and for `doctor`.
    pub fn summary(&self) -> String {
        match self.aligned_cut {
            Some(aligned) => format!(
                "{} (cut {} → {}, Δ{}): {}",
                self.status.headline(),
                self.requested_cut,
                aligned,
                self.delta(),
                self.reason
            ),
            None => {
                format!("{} (cut {}): {}", self.status.headline(), self.requested_cut, self.reason)
            }
        }
    }
}

/// Per-slot verdict used by the eviction engine to decide whether to attempt
/// alignment at all.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SlotVerdict {
    pub slot_id: String,
    pub n_past: i64,
    pub n_ctx: i64,
    pub fill_ratio: f64,
    pub capabilities: crate::llama::CapabilitySet,
    pub checkpoints: usize,
    /// True when there is enough cache machinery for FR-7 to have a chance.
    pub coherence_possible: bool,
    pub note: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aligned_plan_reports_reuse_and_a_zero_or_positive_delta() {
        let p = BoundaryPlan::aligned(
            5000,
            4608,
            SnapReason::InternalCheckpoint { delta: 392 },
            8,
            false,
        );
        assert_eq!(p.status, CacheStatus::PartialReuse);
        assert!(p.is_reuse());
        assert_eq!(p.delta(), 392);
        assert!(p.summary().contains("4608"));
        assert!(!p.wants_snapshot());
    }

    #[test]
    fn exact_hit_has_zero_delta() {
        let p = BoundaryPlan::aligned(
            4608,
            4608,
            SnapReason::InternalCheckpoint { delta: 0 },
            3,
            false,
        );
        assert_eq!(p.delta(), 0);
        assert_eq!(p.action, CoherenceAction::EvictAtAlignedBoundary);
    }

    #[test]
    fn unaligned_plan_requests_a_snapshot_and_names_the_gap() {
        let p = BoundaryPlan::unaligned(12_000, Some(8192), 512);
        assert_eq!(p.status, CacheStatus::FullRePrefill);
        assert!(p.wants_snapshot());
        assert!(p.reason.contains("3808 tokens before"));
        assert!(!p.is_reuse());
    }

    #[test]
    fn unaligned_plan_without_any_checkpoint_reads_cleanly() {
        let p = BoundaryPlan::unaligned(1000, None, 512);
        assert!(p.reason.contains("no checkpoint is close enough"));
        assert_eq!(p.nearest_cut, None);
        assert!(p.wants_snapshot(), "an unavoidable rewrite should be protected by a save");
    }

    #[test]
    fn a_ring_ahead_of_the_cut_is_reported_as_no_reuse_not_as_a_gap() {
        // llama.cpp matches the longest common *prefix*: a ring that starts after
        // the cut still contributes its matching head, but the tokens past the cut
        // are wasted and no prefill is saved. Reporting that as FullRePrefill is
        // what keeps G1's metric honest.
        let p = BoundaryPlan::past_the_ring(1000, 2500, 4, false);
        assert_eq!(p.status, CacheStatus::FullRePrefill);
        assert!(!p.is_reuse());
        assert_eq!(p.ring_lead(), Some(1500));
        assert!(p.reason.contains("past the requested"));
    }

    #[test]
    fn capability_fallback_plan_never_claims_reuse() {
        let p = BoundaryPlan::full_rewrite(2000, "backend unreachable");
        assert_eq!(p.action, CoherenceAction::EvictAndAcceptReprefill);
        assert!(!p.is_reuse());
        assert!(!p.wants_snapshot());
        assert!(p.summary().contains("FULL RE-PREFILL"));
    }

    #[test]
    fn status_strings_match_the_database_check_constraint() {
        // `slot_state.cache_status` has a CHECK constraint listing exactly these.
        for s in [
            CacheStatus::Unknown,
            CacheStatus::Cold,
            CacheStatus::FullRePrefill,
            CacheStatus::PartialReuse,
            CacheStatus::WarmRestored,
        ] {
            assert!(
                ["unknown", "cold", "full-re-prefill", "partial-reuse", "warm-restored"]
                    .contains(&s.as_str()),
                "{} is not in the schema's allowed set",
                s.as_str()
            );
        }
    }

    #[test]
    fn plans_round_trip_through_json() {
        // The plan is persisted in `slot_state.cache_detail`; a plan that cannot
        // be read back would silently turn every turn into "unknown".
        let p = BoundaryPlan::aligned(5000, 4608, SnapReason::SlotSaveFile { delta: 392 }, 4, true);
        let json = serde_json::to_string(&p).unwrap();
        let back: BoundaryPlan = serde_json::from_str(&json).unwrap();
        assert_eq!(p, back);
    }
}
