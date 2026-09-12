//! The Memory Fabric (PRD component C1).
//!
//! Four stores and one graph, with a hard discipline about who may write what:
//!
//! | Store | Written by | May an LLM write it? |
//! |---|---|---|
//! | Episodic Stream | the harness, once per turn/tool call | no — and rows are immutable once written |
//! | Symbolic Ledger | tree-sitter and structured tool-output parsers only | **categorically no** (FR-2) |
//! | Semantic Atlas | the Idle Consolidator or an explicit tool call | yes — every entry must carry an anchor |
//! | Anchor Set | user or agent, explicitly | yes, but exempt from all eviction |
//!
//! The module is organised so that the FR-2 guarantee is a property of the
//! *module graph*, not a review checklist: [`symbolic`] contains no reference to
//! [`crate::embed`], [`crate::llama`] or [`crate::consolidate`], so no code path
//! in it can reach an inference client even by accident.

pub mod anchor;
pub mod dependency;
pub mod episodic;
pub mod fabric;
pub mod semantic;
pub mod symbolic;

pub use anchor::{AnchorKind, AnchorProposal, AnchorRow, ConstraintDetector, PinRequest};
pub use dependency::{DependencyGraph, EdgeKind, EdgeRow, NodeKind, NodeRef};
pub use episodic::{EpisodeRow, EpisodeTier, NewEpisode, Role};
pub use fabric::{CommitOutcome, MemoryFabric, SessionTimeline, TimelineItem};
pub use semantic::{AnchorType, SemanticEntry, SemanticWrite, StalenessReport};
pub use symbolic::{
    FactKind, FactSource, SymbolicFact, SymbolicWrite, ToolOutputFacts, ToolOutputParser,
};

/// Content hashes and token counts are both attached to several DTOs; keeping the
/// canonical descriptions here avoids four copy-pasted doc comments.
///
/// * A **content hash** is BLAKE3 truncated to 16 hex characters — see
///   [`crate::ids::short_hash_str`]. It is the unit of staleness detection
///   throughout the fabric: two facts with equal hashes are interchangeable for
///   every purpose Sakur4 uses them for.
/// * A **token count** is produced by the session's [`crate::tokens::TokenCounter`]
///   so that budget arithmetic in the eviction engine and the numbers printed in
///   the Context Ledger Receipt are always measured the same way.
pub mod conventions {}
