//! The Semantic Atlas: LLM-derived interpretation, mandatorily anchored (FR-3).
//!
//! This is the *only* place in the Fabric where model-generated text is stored,
//! and every row must carry:
//!
//! * an `anchor_type` + `anchor_id` pointing at the symbolic fact or episodic
//!   event(s) it was derived from, and
//! * the anchor's hash **as of write time**.
//!
//! Staleness is then a comparison between that recorded hash and the anchor's
//! current hash. Two consequences worth stating plainly:
//!
//! 1. A summary can never outrun its source silently. If the function changed,
//!    the entry is stale the moment the new fact is written — no background job
//!    has to have run.
//! 2. A stale entry is *never* returned as fact. [`crate::recall`] either
//!    attaches the current anchor content alongside the stale summary, or
//!    refuses to surface the summary at all. This is INN-4: catching drift at
//!    read time as a second line of defence.

use crate::error::{Error, Result};
use crate::ids::{new_id, now_rfc3339, short_hash_str};

/// Which store an entry is anchored to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AnchorType {
    SymbolicFact,
    EpisodicStream,
}

impl AnchorType {
    pub fn as_str(self) -> &'static str {
        match self {
            AnchorType::SymbolicFact => "symbolic_fact",
            AnchorType::EpisodicStream => "episodic_stream",
        }
    }

    pub fn parse(s: &str) -> Result<Self> {
        Ok(match s {
            "symbolic_fact" => AnchorType::SymbolicFact,
            "episodic_stream" => AnchorType::EpisodicStream,
            other => return Err(Error::Invalid(format!("unknown anchor type: {other}"))),
        })
    }

    /// The natural key prefix Sakur4 uses in ids and logs.
    pub fn id_prefix(self) -> &'static str {
        match self {
            AnchorType::SymbolicFact => "sym",
            AnchorType::EpisodicStream => "ep",
        }
    }
}

/// A request to write an Atlas entry.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct SemanticWrite {
    pub content: String,
    pub anchor_type: AnchorType,
    pub anchor_id: String,
    /// The anchor's hash at write time. Callers normally omit this and let the
    /// store resolve it, which is safer: it makes it impossible to record a hash
    /// that never matched the anchor in the first place.
    #[serde(default)]
    pub anchor_hash_at_write: Option<String>,
    /// Additional anchors this entry also depends on. All of them participate in
    /// staleness detection.
    #[serde(default)]
    pub extra_anchors: Vec<(AnchorType, String)>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub project_id: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
}

impl SemanticWrite {
    pub fn new(content: impl Into<String>, anchor_type: AnchorType, anchor_id: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            anchor_type,
            anchor_id: anchor_id.into(),
            anchor_hash_at_write: None,
            extra_anchors: Vec::new(),
            model: None,
            project_id: None,
            session_id: None,
        }
    }

    /// Anchor to a symbolic fact.
    pub fn on_fact(content: impl Into<String>, fact_id: impl Into<String>) -> Self {
        Self::new(content, AnchorType::SymbolicFact, fact_id)
    }

    /// Anchor to an episode.
    pub fn on_episode(content: impl Into<String>, episode_id: impl Into<String>) -> Self {
        Self::new(content, AnchorType::EpisodicStream, episode_id)
    }

    pub fn by_model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    pub fn in_project(mut self, project_id: impl Into<String>) -> Self {
        self.project_id = Some(project_id.into());
        self
    }

    pub fn in_session(mut self, session_id: impl Into<String>) -> Self {
        self.session_id = Some(session_id.into());
        self
    }

    pub fn also_anchored_to(mut self, anchor_type: AnchorType, anchor_id: impl Into<String>) -> Self {
        self.extra_anchors.push((anchor_type, anchor_id.into()));
        self
    }

    /// Structural validation before touching the database.
    ///
    /// FR-3's first acceptance criterion is "insert is rejected if anchor_ref is
    /// null or points to a non-existent row". This covers the null/empty half;
    /// the foreign-key half is enforced by the store, which resolves the hash by
    /// selecting the anchor row and failing when it is absent.
    pub fn validate(&self) -> Result<()> {
        if self.content.trim().is_empty() {
            return Err(Error::Invalid("a Semantic Atlas entry must have content".into()));
        }
        if self.anchor_id.trim().is_empty() {
            return Err(Error::Integrity(
                "a Semantic Atlas entry must be anchored: anchor_id is empty (FR-3)".into(),
            ));
        }
        if self.content.len() > 32_000 {
            return Err(Error::Invalid(format!(
                "Semantic Atlas entries are summaries; {} characters is beyond the supported size",
                self.content.len()
            )));
        }
        Ok(())
    }
}

/// A row of the atlas, with staleness resolved at read time.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SemanticEntry {
    pub atlas_id: String,
    pub content: String,
    pub anchor_type: AnchorType,
    pub anchor_id: String,
    pub anchor_hash_at_write: String,
    /// Current hash of the anchor. Equal to `anchor_hash_at_write` unless the
    /// anchor changed.
    pub current_anchor_hash: Option<String>,
    /// True when the anchor changed since this entry was written.
    pub is_stale: bool,
    pub model: Option<String>,
    pub project_id: Option<String>,
    pub session_id: Option<String>,
    pub created_at: String,
    /// Every anchor this entry depends on, primary first.
    pub anchors: Vec<(AnchorType, String)>,
}

impl SemanticEntry {
    /// Build from stored columns plus the resolved current anchor hash.
    ///
    /// Ten arguments because it mirrors the row exactly: every field is required
    /// to decide staleness, and bundling them into a struct would only move the
    /// same eight values one layer up.
    #[allow(clippy::too_many_arguments)]
    pub fn from_row(
        atlas_id: String,
        content: String,
        anchor_type: AnchorType,
        anchor_id: String,
        anchor_hash_at_write: String,
        current_anchor_hash: Option<String>,
        model: Option<String>,
        project_id: Option<String>,
        session_id: Option<String>,
        created_at: String,
    ) -> Self {
        // A *missing* anchor counts as stale. If the fact an entry was derived
        // from no longer exists, the summary is describing something that is not
        // there any more, which is exactly the failure this whole component
        // exists to catch.
        let is_stale = match &current_anchor_hash {
            Some(cur) => cur != &anchor_hash_at_write,
            None => true,
        };
        let mut anchors = vec![(anchor_type, anchor_id.clone())];
        Self {
            atlas_id,
            content,
            anchor_type,
            anchor_id,
            anchor_hash_at_write,
            current_anchor_hash,
            is_stale,
            model,
            project_id,
            session_id,
            created_at,
            anchors: std::mem::take(&mut anchors),
        }
    }

    /// How this entry may be used in a prompt, given its staleness.
    ///
    /// Fresh entries render as summaries. Stale ones render as an explicit
    /// warning that names the drift and points at the ground truth — never as a
    /// bare assertion (INN-4).
    pub fn render_for_prompt(&self) -> String {
        if self.is_stale {
            format!(
                "[STALE SUMMARY — do not trust] {} \n  ↳ the anchor it was derived from \
                 ({}({})) has changed since this summary was written; re-read the source \
                 or call code.query_symbol to get the current truth.",
                self.content,
                self.anchor_type.as_str(),
                self.anchor_id
            )
        } else {
            self.content.clone()
        }
    }

    /// Short marker used in recall result lists.
    pub fn staleness_marker(&self) -> &'static str {
        if self.is_stale {
            "[STALE]"
        } else {
            ""
        }
    }

    /// Deterministic hash of the entry's own content.
    pub fn content_hash(&self) -> String {
        short_hash_str(&self.content)
    }
}

/// An entry that is stale, with the information needed to re-derive it.
#[derive(Debug, Clone, serde::Serialize)]
pub struct StaleEntry {
    pub atlas_id: String,
    pub anchor_type: AnchorType,
    pub anchor_id: String,
    pub recorded_hash: String,
    pub current_hash: Option<String>,
    /// Why the entry is stale, in a form the receipt can print.
    pub reason: StaleReason,
}

/// Why an Atlas entry went stale.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StaleReason {
    /// The anchor's content changed.
    AnchorChanged,
    /// The anchor no longer exists.
    AnchorDeleted,
}

/// Aggregate staleness, the metric the PRD tracks as a leading indicator.
#[derive(Debug, Clone, serde::Serialize)]
pub struct StalenessReport {
    pub total: usize,
    pub stale: usize,
    pub deleted_anchors: usize,
    pub entries: Vec<StaleEntry>,
}

impl StalenessReport {
    /// Staleness rate in `[0,1]`; the leading indicator the PRD wants trending
    /// toward zero once the Idle Consolidator is running.
    pub fn rate(&self) -> f64 {
        if self.total == 0 {
            0.0
        } else {
            self.stale as f64 / self.total as f64
        }
    }

    pub fn is_clean(&self) -> bool {
        self.stale == 0
    }
}

/// Construct the id for a new entry. Exposed so callers can log it before the
/// write completes.
pub fn new_atlas_id() -> String {
    new_id("atlas")
}

/// Timestamp helper so `semantic` does not reach into `ids` at call sites.
pub fn stamped_now() -> String {
    now_rfc3339()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(recorded: &str, current: Option<&str>) -> SemanticEntry {
        SemanticEntry::from_row(
            "atlas_1".into(),
            "f() validates the user id".into(),
            AnchorType::SymbolicFact,
            "sym_1".into(),
            recorded.into(),
            current.map(String::from),
            Some("aux-model".into()),
            None,
            None,
            "2026-09-12T00:00:00Z".into(),
        )
    }

    #[test]
    fn fresh_when_hashes_match() {
        let e = entry("abc", Some("abc"));
        assert!(!e.is_stale);
        assert_eq!(e.staleness_marker(), "");
        assert_eq!(e.render_for_prompt(), "f() validates the user id");
    }

    #[test]
    fn stale_when_the_anchor_changed() {
        let e = entry("abc", Some("def"));
        assert!(e.is_stale);
        assert_eq!(e.staleness_marker(), "[STALE]");
        let rendered = e.render_for_prompt();
        assert!(rendered.contains("[STALE SUMMARY"));
        assert!(rendered.contains("do not trust"));
    }

    #[test]
    fn a_deleted_anchor_is_stale_not_fresh() {
        let e = entry("abc", None);
        assert!(
            e.is_stale,
            "a summary of something that no longer exists must never render as fact"
        );
    }

    #[test]
    fn writes_require_an_anchor() {
        let mut w = SemanticWrite::on_fact("summary", "");
        assert!(matches!(w.validate(), Err(Error::Integrity(_))));
        w.anchor_id = "sym_1".into();
        assert!(w.validate().is_ok());
    }

    #[test]
    fn writes_require_content() {
        let w = SemanticWrite::on_fact("   ", "sym_1");
        assert!(w.validate().is_err());
    }

    #[test]
    fn staleness_rate_is_defined_for_an_empty_atlas() {
        let r = StalenessReport {
            total: 0,
            stale: 0,
            deleted_anchors: 0,
            entries: Vec::new(),
        };
        assert_eq!(r.rate(), 0.0);
        assert!(r.is_clean());
    }

    #[test]
    fn extra_anchors_are_recorded_on_the_write() {
        let w = SemanticWrite::on_episode("summary", "ep_1")
            .also_anchored_to(AnchorType::SymbolicFact, "sym_9");
        assert_eq!(w.extra_anchors.len(), 1);
        assert_eq!(w.anchor_type, AnchorType::EpisodicStream);
    }
}
