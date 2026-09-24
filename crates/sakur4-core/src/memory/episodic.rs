//! The Episodic Stream: raw, append-only trajectory (FR-1).
//!
//! This is the Fabric's source of truth. Two design consequences follow from
//! taking that seriously:
//!
//! 1. **Nothing is ever rewritten.** Corrections, supersessions and evictions
//!    are recorded as *new* rows or as column changes that do not touch
//!    `content`. Recalling an evicted episode therefore returns bytes identical
//!    to the original, which is FR-5's round-trip integrity criterion — it is
//!    not a feature that had to be implemented, it is what falls out of never
//!    mutating the content column.
//! 2. **The eviction tier is a rendering concern.** `eviction_tier` says how an
//!    episode should appear *in the live window*; it never says the episode may
//!    be forgotten. `drop` means "omit from the live window", not "delete".
//!
//! The database enforces (1) with `BEFORE UPDATE`/`BEFORE DELETE` triggers; see
//! [`crate::store::schema`].

use crate::error::{Error, Result};
use crate::ids::short_hash_str;
use crate::tokens::TokenCounter;

/// Who produced an episode.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
    /// A Sakur4-internal note (fold boundaries, consolidation markers).
    Internal,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
            Role::Internal => "internal",
        }
    }

    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "system" => Ok(Role::System),
            "user" => Ok(Role::User),
            "assistant" => Ok(Role::Assistant),
            "tool" => Ok(Role::Tool),
            "internal" => Ok(Role::Internal),
            other => Err(Error::Invalid(format!("unknown role: {other}"))),
        }
    }
}

/// How an episode currently appears in the live context window (FR-5).
///
/// Ordering is meaningful: [`EpisodeTier::severity`] is what the eviction engine
/// compares to decide whether a lower tier still has room to absorb pressure.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    serde::Serialize,
    serde::Deserialize,
    schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum EpisodeTier {
    /// Rendered in full.
    Live,
    /// Verbose tool output replaced by a compact reference; the full text is
    /// still one `recall` away.
    Masked,
    /// Pulled out of the window entirely; represented (if at all) by a Semantic
    /// Atlas entry derived from it.
    Referenced,
    /// Cold storage; retrievable only by explicit recall.
    Archived,
    /// Omitted from the live window. Only ever applied to episodes explicitly
    /// marked droppable and with no unresolved dependents.
    Dropped,
}

impl EpisodeTier {
    pub fn as_str(self) -> &'static str {
        match self {
            EpisodeTier::Live => "live",
            EpisodeTier::Masked => "masked",
            EpisodeTier::Referenced => "referenced",
            EpisodeTier::Archived => "archived",
            EpisodeTier::Dropped => "dropped",
        }
    }

    /// 0 = live. Higher means more aggressively evicted.
    pub fn severity(self) -> u8 {
        match self {
            EpisodeTier::Live => 0,
            EpisodeTier::Masked => 1,
            EpisodeTier::Referenced => 2,
            EpisodeTier::Archived => 3,
            EpisodeTier::Dropped => 4,
        }
    }

    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "live" => Ok(EpisodeTier::Live),
            "masked" => Ok(EpisodeTier::Masked),
            "referenced" => Ok(EpisodeTier::Referenced),
            "archived" => Ok(EpisodeTier::Archived),
            "dropped" => Ok(EpisodeTier::Dropped),
            other => Err(Error::Invalid(format!("unknown eviction tier: {other}"))),
        }
    }

    /// The tier one step more severe, or `None` at the floor.
    pub fn escalate(self) -> Option<EpisodeTier> {
        match self {
            EpisodeTier::Live => Some(EpisodeTier::Masked),
            EpisodeTier::Masked => Some(EpisodeTier::Referenced),
            EpisodeTier::Referenced => Some(EpisodeTier::Archived),
            EpisodeTier::Archived => Some(EpisodeTier::Dropped),
            EpisodeTier::Dropped => None,
        }
    }

    /// True when the episode contributes no tokens to the live window.
    pub fn is_out_of_window(self) -> bool {
        matches!(self, EpisodeTier::Referenced | EpisodeTier::Archived | EpisodeTier::Dropped)
    }
}

/// A request to append to the stream.
#[derive(Debug, Clone)]
pub struct NewEpisode {
    pub session_id: String,
    pub slot_id: Option<String>,
    pub role: Role,
    pub content: String,
    pub tool_name: Option<String>,
    pub fold_id: Option<String>,
    /// Marked droppable only by the producer, e.g. a duplicate file read.
    pub droppable: bool,
    pub meta: Option<serde_json::Value>,
    /// Which project this turn belongs to, so one store can hold more than one.
    ///
    /// Optional because the fabric is also driven without a project — tests, the `--ephemeral`
    /// demo — and because episodes written before migration 3 have no recoverable value. A row with
    /// `None` is *not attributable* rather than "belongs to whoever is asking": a project-scoped
    /// query excludes it instead of showing one project another's transcript.
    pub project_id: Option<String>,
}

impl NewEpisode {
    pub fn user(session_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            session_id: session_id.into(),
            slot_id: None,
            role: Role::User,
            content: content.into(),
            tool_name: None,
            fold_id: None,
            droppable: false,
            meta: None,
            project_id: None,
        }
    }

    pub fn tool_result(
        session_id: impl Into<String>,
        tool_name: impl Into<String>,
        content: impl Into<String>,
    ) -> Self {
        let tool_name = tool_name.into();
        Self {
            session_id: session_id.into(),
            slot_id: None,
            role: Role::Tool,
            content: content.into(),
            tool_name: Some(tool_name),
            fold_id: None,
            droppable: false,
            meta: None,
            project_id: None,
        }
    }

    pub fn assistant(session_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            session_id: session_id.into(),
            slot_id: None,
            role: Role::Assistant,
            content: content.into(),
            tool_name: None,
            fold_id: None,
            droppable: false,
            meta: None,
            project_id: None,
        }
    }

    pub fn with_slot(mut self, slot: impl Into<String>) -> Self {
        self.slot_id = Some(slot.into());
        self
    }

    /// Attribute this turn to a project.
    ///
    /// Set where the engine knows its project — the MCP tools and the CLI — so a store holding more
    /// than one project can keep their transcripts apart. Left unset, the row is stored as not
    /// attributable and a project-scoped query will not return it.
    pub fn with_project(mut self, project: impl Into<String>) -> Self {
        self.project_id = Some(project.into());
        self
    }

    pub fn in_fold(mut self, fold_id: impl Into<String>) -> Self {
        self.fold_id = Some(fold_id.into());
        self
    }

    pub fn droppable(mut self) -> Self {
        self.droppable = true;
        self
    }

    pub fn with_meta(mut self, meta: serde_json::Value) -> Self {
        self.meta = Some(meta);
        self
    }
}

/// A row of the stream.
#[derive(Debug, Clone, serde::Serialize)]
pub struct EpisodeRow {
    pub episode_id: String,
    pub seq: i64,
    pub session_id: String,
    pub slot_id: Option<String>,
    pub role: String,
    pub content: String,
    pub tool_name: Option<String>,
    pub token_count: i64,
    pub created_at: String,
    pub fold_id: Option<String>,
    pub eviction_tier: EpisodeTier,
    pub droppable: bool,
    pub superseded_by: Option<String>,
    /// The project this turn belongs to, absent for rows written before migration 3.
    pub project_id: Option<String>,
}

impl EpisodeRow {
    /// The text a rendered prompt would use for this episode at its current tier.
    ///
    /// Masking is applied at render time rather than by rewriting the row, which
    /// is what keeps round-trip integrity exact: `content` is never touched.
    pub fn render(&self) -> String {
        match self.eviction_tier {
            EpisodeTier::Live => self.content.clone(),
            EpisodeTier::Masked => {
                let preview: String = self.content.chars().take(160).collect();
                let suffix = if self.content.chars().count() > 160 { " …" } else { "" };
                format!(
                    "[masked {} result: {} tokens, recall with memory.recall episode={}]\n{preview}{suffix}",
                    self.tool_name.as_deref().unwrap_or("tool"),
                    self.token_count,
                    self.episode_id
                )
            }
            EpisodeTier::Referenced => format!(
                "[evicted to Semantic Atlas: episode={} — use memory.recall to restore verbatim]",
                self.episode_id
            ),
            EpisodeTier::Archived => format!(
                "[archived: episode={} — use memory.recall with include_archived=true]",
                self.episode_id
            ),
            EpisodeTier::Dropped => String::new(),
        }
    }

    /// Deterministic hash of the episode's content, used as the anchor hash for
    /// Semantic Atlas entries derived from it (FR-3).
    pub fn content_hash(&self) -> String {
        short_hash_str(&self.content)
    }

    /// Tokens this episode costs in the live window right now.
    pub fn live_tokens(&self, counter: &TokenCounter) -> usize {
        match self.eviction_tier {
            EpisodeTier::Dropped => 0,
            EpisodeTier::Live => self.token_count as usize,
            // Masked/archived/referenced render to a short stub; measuring the
            // stub rather than trusting a stored estimate keeps FR-15 honest.
            _ => counter.count(&self.render()).get(),
        }
    }
}
