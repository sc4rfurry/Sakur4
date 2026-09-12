//! The Anchor Set: pinned constraints that survive every compaction (FR-4).
//!
//! Anchors are the countermeasure to "governance decay" — the failure mode where
//! a summarisation-based compaction quietly drops the user correction or safety
//! constraint that was stated forty turns ago, and nothing fails visibly until
//! the agent violates it.
//!
//! Two guarantees, and one honest limitation:
//!
//! * Anchors are never candidates for any eviction tier. The eviction engine
//!   cannot even express the operation: it selects from [`crate::memory::episodic`]
//!   rows, and anchors are not episodes.
//! * Anchors are always rendered into every assembled prompt, verbatim, ahead of
//!   anything evictable.
//! * If the Anchor Set alone would exceed the context budget, Sakur4 raises a
//!   visible [`crate::error::Error::BudgetOverflow`] instead of silently dropping
//!   one (FR-4's second acceptance criterion). The PRD is explicit that a silent
//!   drop is the wrong behaviour here, so this is the one place the system
//!   refuses to make progress rather than degrade.

use crate::error::{Error, Result};
use crate::ids::{new_id, now_rfc3339};
use crate::memory::episodic::Role;
use crate::tokens::TokenCounter;

/// Why an anchor was pinned. The kind is not cosmetic: it drives the
/// constraint-detection pass that proposes auto-pins (PRD risk mitigation for
/// governance decay) and the ordering in which anchors are rendered.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum AnchorKind {
    /// The user told the agent it was wrong about something.
    UserCorrection,
    /// A safety or policy constraint.
    SafetyConstraint,
    /// An explicit task requirement or acceptance criterion.
    TaskContract,
}

impl AnchorKind {
    pub fn as_str(self) -> &'static str {
        match self {
            AnchorKind::UserCorrection => "user_correction",
            AnchorKind::SafetyConstraint => "safety_constraint",
            AnchorKind::TaskContract => "task_contract",
        }
    }

    pub fn parse(s: &str) -> Result<Self> {
        Ok(match s {
            "user_correction" => AnchorKind::UserCorrection,
            "safety_constraint" => AnchorKind::SafetyConstraint,
            "task_contract" => AnchorKind::TaskContract,
            other => return Err(Error::Invalid(format!("unknown anchor kind: {other}"))),
        })
    }

    /// Render order: safety first, then corrections, then contracts. A prompt
    /// that has to be truncated at the very end should lose contracts before
    /// safety rules — though in practice anchors are never truncated.
    pub fn priority(self) -> u8 {
        match self {
            AnchorKind::SafetyConstraint => 0,
            AnchorKind::UserCorrection => 1,
            AnchorKind::TaskContract => 2,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            AnchorKind::SafetyConstraint => "SAFETY CONSTRAINT",
            AnchorKind::UserCorrection => "USER CORRECTION",
            AnchorKind::TaskContract => "TASK CONTRACT",
        }
    }
}

/// A pinned entry.
#[derive(Debug, Clone, serde::Serialize)]
pub struct AnchorRow {
    pub anchor_id: String,
    pub content: String,
    pub kind: AnchorKind,
    pub session_id: Option<String>,
    pub project_id: Option<String>,
    pub pinned_by: String,
    pub created_at: String,
}

impl AnchorRow {
    /// The verbatim block this anchor contributes to every prompt.
    pub fn render(&self) -> String {
        format!("[{}] {}", self.kind.label(), self.content)
    }

    pub fn token_cost(&self, counter: &TokenCounter) -> usize {
        counter.count(&self.render()).get()
    }
}

/// A request to pin.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct PinRequest {
    pub content: String,
    pub kind: AnchorKind,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub project_id: Option<String>,
    /// `user`, `agent`, or `auto:<rule>` for a proposed automatic pin.
    #[serde(default = "default_pinned_by")]
    pub pinned_by: String,
}

fn default_pinned_by() -> String {
    "user".into()
}

impl PinRequest {
    pub fn new(kind: AnchorKind, content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            kind,
            session_id: None,
            project_id: None,
            pinned_by: default_pinned_by(),
        }
    }

    pub fn in_session(mut self, session: impl Into<String>) -> Self {
        self.session_id = Some(session.into());
        self
    }

    pub fn by(mut self, who: impl Into<String>) -> Self {
        self.pinned_by = who.into();
        self
    }

    /// Validate into a row. Rejects empty content, which is the only way a
    /// "pinned" anchor can fail to protect anything.
    pub fn into_row(self) -> Result<AnchorRow> {
        let content = self.content.trim().to_string();
        if content.is_empty() {
            return Err(Error::Invalid("an anchor must have content".into()));
        }
        Ok(AnchorRow {
            anchor_id: new_id("anc"),
            content,
            kind: self.kind,
            session_id: self.session_id,
            project_id: self.project_id,
            pinned_by: self.pinned_by,
            created_at: now_rfc3339(),
        })
    }
}

/// A candidate automatic pin found by the deterministic constraint detector.
#[derive(Debug, Clone, serde::Serialize)]
pub struct AnchorProposal {
    /// The episode that contained the constraint.
    pub episode_id: String,
    pub seq: i64,
    pub role: Role,
    pub kind: AnchorKind,
    pub excerpt: String,
    /// Which rule fired, so a human can audit why the system proposed a pin.
    pub rule: &'static str,
    /// Detector confidence in `[0,1]`. Deterministic rules only; no classifier.
    pub confidence: f32,
}

/// Deterministic constraint detection over an episode.
///
/// The PRD's mitigation for governance decay is "a lightweight, deterministic
/// (regex/keyword plus optional local classifier) constraint-detection pass ...
/// that proposes auto-pinning candidate safety/constraint statements, subject to
/// user/agent confirmation". This is the deterministic half: explicit,
/// auditable rules, no learned component, no inference client — which keeps it
/// usable on the write path without adding latency or a model dependency.
pub struct ConstraintDetector;

/// `(rule name, kind, patterns)` — patterns are matched case-insensitively
/// against the whole episode text.
const RULES: &[(&str, AnchorKind, &[&str])] = &[
    (
        "explicit_never",
        AnchorKind::SafetyConstraint,
        &["never ", "do not ever", "don't ever", "under no circumstances", "at no point"],
    ),
    (
        "prohibition",
        AnchorKind::SafetyConstraint,
        &[
            "must not ",
            "may not ",
            "should not ",
            "cannot ",
            "forbidden",
            "prohibited",
            "not allowed to",
        ],
    ),
    (
        "secrets_policy",
        AnchorKind::SafetyConstraint,
        &[
            "do not commit",
            "don't commit",
            "no secrets",
            "never push",
            "do not push",
            "don't push",
            "not to production",
            "without asking",
        ],
    ),
    (
        "correction",
        AnchorKind::UserCorrection,
        &[
            "that's wrong",
            "thats wrong",
            "that is wrong",
            "you are wrong",
            "incorrect:",
            "actually it",
            "no, ",
            "i said ",
            "not what i asked",
            "stop doing",
        ],
    ),
    (
        "task_contract",
        AnchorKind::TaskContract,
        &[
            "always ",
            "make sure to",
            "be sure to",
            "requirement:",
            "must always",
            "the goal is",
            "acceptance criteria",
            "constraint:",
        ],
    ),
];

impl ConstraintDetector {
    /// Scan one episode and return at most one proposal.
    ///
    /// One per episode, deliberately: flooding the Anchor Set with marginal
    /// proposals would erode the signal that anchors are supposed to carry, and
    /// every anchor permanently consumes context budget.
    pub fn detect(episode_id: &str, seq: i64, role: Role, content: &str) -> Option<AnchorProposal> {
        // Only user and system turns state constraints. An assistant turn saying
        // "I must not skip tests" is the agent paraphrasing, not a new rule.
        if !matches!(role, Role::User | Role::System) {
            return None;
        }
        if content.len() < 8 || content.len() > 8_000 {
            return None;
        }
        let lower = content.to_ascii_lowercase();

        let mut best: Option<AnchorProposal> = None;
        for (rule, kind, needles) in RULES {
            for needle in *needles {
                if let Some(idx) = lower.find(needle) {
                    // A rule firing at the very end of a long message is more
                    // likely a trailing aside than a constraint; still propose
                    // it, but with lower confidence.
                    let confidence = if idx < content.len() / 2 { 0.7 } else { 0.45 };
                    let candidate = AnchorProposal {
                        episode_id: episode_id.to_string(),
                        seq,
                        role,
                        kind: *kind,
                        excerpt: excerpt_around(content, idx, needle.len()),
                        rule,
                        confidence,
                    };
                    let better =
                        best.as_ref().map(|b| candidate.confidence > b.confidence).unwrap_or(true);
                    if better {
                        best = Some(candidate);
                    }
                }
            }
        }
        // Safety beats corrections at equal confidence.
        best
    }

    /// Scan a batch, keeping only proposals at or above `min_confidence`.
    pub fn detect_all<'a, I>(episodes: I, min_confidence: f32) -> Vec<AnchorProposal>
    where
        I: IntoIterator<Item = (&'a str, i64, Role, &'a str)>,
    {
        episodes
            .into_iter()
            .filter_map(|(id, seq, role, content)| Self::detect(id, seq, role, content))
            .filter(|p| p.confidence >= min_confidence)
            .collect()
    }
}

fn excerpt_around(content: &str, idx: usize, needle_len: usize) -> String {
    let chars: Vec<char> = content.chars().collect();
    // `idx` came from a byte offset in a lowercased copy; for the ASCII rules
    // above that is a valid character index in practice, but clamp defensively.
    let start = idx.saturating_sub(48).min(chars.len());
    let end = (idx + needle_len + 96).min(chars.len());
    let mut out: String = chars[start..end].iter().collect();
    out = out.trim().to_string();
    if start > 0 {
        out.insert_str(0, "… ");
    }
    if end < chars.len() {
        out.push_str(" …");
    }
    out
}

/// Render the Anchor Set into the prompt block that precedes everything else.
///
/// Returns [`Error::BudgetOverflow`] when the anchors alone cannot fit — the
/// explicit "visible warning rather than silent drop" requirement of FR-4.
pub fn render_anchor_block(
    anchors: &[AnchorRow],
    counter: &TokenCounter,
    budget_tokens: usize,
) -> Result<(String, usize)> {
    if anchors.is_empty() {
        return Ok((String::new(), 0));
    }
    let mut ordered: Vec<&AnchorRow> = anchors.iter().collect();
    ordered.sort_by_key(|a| (a.kind.priority(), a.created_at.clone(), a.anchor_id.clone()));

    let mut body = String::from(
        "=== PINNED ANCHORS (verbatim, exempt from all compaction) ===\n\
         These were pinned by the user or the agent and must be honoured for the \
         entire session.\n",
    );
    for a in &ordered {
        body.push_str(&a.render());
        body.push('\n');
    }

    let used = counter.count(&body).get();
    if used > budget_tokens {
        return Err(Error::BudgetOverflow(format!(
            "the Anchor Set alone needs {used} tokens but the configured budget for \
             anchors is {budget_tokens}. Sakur4 will not silently drop a pinned \
             constraint; raise the budget, unpin an anchor, or shorten the longest one."
        )));
    }
    Ok((body, used))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tokens::CharTokenizer;

    fn anchor(kind: AnchorKind, content: &str) -> AnchorRow {
        PinRequest::new(kind, content).into_row().unwrap()
    }

    #[test]
    fn empty_anchor_is_rejected() {
        assert!(PinRequest::new(AnchorKind::TaskContract, "   ").into_row().is_err());
    }

    #[test]
    fn priority_orders_safety_first() {
        let list = vec![
            anchor(AnchorKind::TaskContract, "keep the API stable"),
            anchor(AnchorKind::SafetyConstraint, "never force-push"),
            anchor(AnchorKind::UserCorrection, "it is validateUser, not checkUser"),
        ];
        let counter = TokenCounter::new(CharTokenizer { chars_per_token: 4 });
        let (block, used) = render_anchor_block(&list, &counter, 10_000).unwrap();
        assert!(used > 0);
        let safety = block.find("SAFETY CONSTRAINT").unwrap();
        let correction = block.find("USER CORRECTION").unwrap();
        let contract = block.find("TASK CONTRACT").unwrap();
        assert!(safety < correction && correction < contract);
    }

    #[test]
    fn oversized_anchor_set_overflows_visibly_instead_of_dropping() {
        let list = vec![
            anchor(AnchorKind::SafetyConstraint, &"x".repeat(400)),
            anchor(AnchorKind::TaskContract, &"y".repeat(400)),
        ];
        let counter = TokenCounter::new(CharTokenizer { chars_per_token: 4 });
        let err = render_anchor_block(&list, &counter, 50).unwrap_err();
        assert!(matches!(err, Error::BudgetOverflow(_)));
        assert!(err.to_string().contains("will not silently drop"));
    }

    #[test]
    fn empty_set_renders_nothing_and_costs_nothing() {
        let counter = TokenCounter::new(CharTokenizer::default());
        let (block, used) = render_anchor_block(&[], &counter, 100).unwrap();
        assert!(block.is_empty());
        assert_eq!(used, 0);
    }

    #[test]
    fn detector_finds_safety_constraints_in_user_turns() {
        let p = ConstraintDetector::detect(
            "ep1",
            7,
            Role::User,
            "One rule for this repo: never force-push to main.",
        )
        .unwrap();
        assert_eq!(p.kind, AnchorKind::SafetyConstraint);
        assert_eq!(p.rule, "explicit_never");
        assert!(p.confidence >= 0.45);
        assert!(p.excerpt.contains("force-push"));
    }

    #[test]
    fn detector_finds_user_corrections() {
        let p = ConstraintDetector::detect(
            "ep2",
            9,
            Role::User,
            "That's wrong — the function is validateUser(id), not checkUser(email).",
        )
        .unwrap();
        assert_eq!(p.kind, AnchorKind::UserCorrection);
    }

    #[test]
    fn detector_ignores_assistant_and_tool_turns() {
        let text = "I must not forget to run the tests.";
        assert!(ConstraintDetector::detect("e", 1, Role::Assistant, text).is_none());
        assert!(ConstraintDetector::detect("e", 1, Role::Tool, text).is_none());
    }

    #[test]
    fn detector_ignores_ordinary_chatter() {
        assert!(
            ConstraintDetector::detect("e", 1, Role::User, "please summarise this file").is_none()
        );
        assert!(ConstraintDetector::detect("e", 1, Role::User, "ok").is_none());
    }

    #[test]
    fn detector_respects_min_confidence() {
        // A late-appearing rule scores 0.45; requiring 0.6 must filter it out.
        let text = format!("{} never do this", "padding ".repeat(30));
        let all = ConstraintDetector::detect_all([("e", 1, Role::User, text.as_str())], 0.0);
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].confidence, 0.45);
        let strict = ConstraintDetector::detect_all([("e", 1, Role::User, text.as_str())], 0.6);
        assert!(strict.is_empty());
    }
}
