//! The Memory Fabric as a whole: the store-facing API every other component uses.
//!
//! Nothing above this layer touches SQL. Keeping the query surface in one place
//! is what makes the FR-2 boundary auditable — [`crate::memory::symbolic`]
//! defines the *shape* of a deterministic fact, and this module is the only
//! place that turns one into a row.

use rusqlite::{OptionalExtension, Row};

use crate::error::{Error, Result};
use crate::ids::{new_id, now_rfc3339};
use crate::memory::anchor::{AnchorRow, ConstraintDetector, PinRequest};
use crate::memory::dependency::{DependencyGraph, EdgeKind, EdgeRow, NodeRef};
use crate::memory::episodic::{EpisodeRow, EpisodeTier, NewEpisode, Role};
use crate::memory::semantic::{
    AnchorType, SemanticEntry, SemanticWrite, StaleEntry, StaleReason, StalenessReport,
};
use crate::memory::symbolic::{FactKind, FactSource, SymbolicFact, ToolOutputFacts};
use crate::store::Db;
use crate::tokens::TokenCounter;

/// Result of appending to the Episodic Stream.
#[derive(Debug, Clone, serde::Serialize)]
pub struct CommitOutcome {
    pub episode_id: String,
    pub seq: i64,
    pub token_count: i64,
    /// Symbolic facts extracted deterministically from this episode, if it
    /// carried structured tool output. Empty for prose.
    pub facts_extracted: usize,
    /// A short description of what the symbolic extractor found, or why it found
    /// nothing. Surfaced so the operator can see the dual-track split happening.
    pub symbolic_summary: String,
    /// A constraint this episode appears to state, proposed for pinning. Never
    /// auto-pinned: the PRD requires user or agent confirmation.
    pub anchor_proposal: Option<crate::memory::anchor::AnchorProposal>,
}

/// One entry of a rendered session timeline.
#[derive(Debug, Clone, serde::Serialize)]
pub struct TimelineItem {
    pub episode_id: String,
    pub seq: i64,
    pub role: String,
    pub tier: EpisodeTier,
    pub tokens: usize,
    /// True when this episode is inside the requested budget window.
    pub included: bool,
    pub preview: String,
}

/// A budgeted, eviction-aware view of a session.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SessionTimeline {
    /// Rendered text, newest-heavy but in chronological order.
    pub rendered: String,
    pub items: Vec<TimelineItem>,
    pub included_tokens: usize,
    /// Total tokens the session's live window would cost with nothing evicted.
    pub total_tokens: usize,
    pub episodes_total: usize,
    pub episodes_included: usize,
    /// Episodes deliberately left out of the window.
    pub episodes_evicted: usize,
    /// Tokens that would be reclaimed by the currently-applied eviction plan.
    pub reclaimed_tokens: usize,
}

/// Handle to the Fabric. Cheap to clone.
#[derive(Clone)]
pub struct MemoryFabric {
    db: Db,
}

impl MemoryFabric {
    pub fn new(db: Db) -> Self {
        Self { db }
    }

    pub fn db(&self) -> &Db {
        &self.db
    }

    // =======================================================================
    // Episodic Stream
    // =======================================================================

    /// Append one turn. The only write path into the stream (FR-1).
    ///
    /// Doing the token count, the symbolic extraction and the constraint probe
    /// inside one transaction means an episode and everything derived from it
    /// become visible together — a reader can never observe an episode whose
    /// symbolic facts have not landed yet.
    pub async fn commit_episode(
        &self,
        new: NewEpisode,
        counter: &TokenCounter,
        extract_symbolic: bool,
        detect_constraints: bool,
    ) -> Result<CommitOutcome> {
        let episode_id = new_id("ep");
        let tokens = counter.count(&new.content).get() as i64;
        let created_at = now_rfc3339();
        let session_id = new.session_id.clone();
        let role = new.role;
        let content = new.content.clone();
        let tool_name = new.tool_name.clone();
        let slot_id = new.slot_id.clone();
        let fold_id = new.fold_id.clone();
        let droppable = new.droppable;
        let project_for_tx = new.project_id.clone();
        let meta_json =
            new.meta.as_ref().map(|m| serde_json::to_string(m).unwrap_or_else(|_| "null".into()));

        // Deterministic extraction happens before the transaction so the write
        // stays short; nothing here can call a model (see `symbolic`).
        let symbolic: Option<ToolOutputFacts> = if extract_symbolic && role == Role::Tool {
            Some(crate::memory::symbolic::ToolOutputParser::parse_any(
                tool_name.as_deref(),
                &content,
            ))
        } else {
            None
        };

        let episode_id_for_tx = episode_id.clone();
        let symbolic_for_tx = symbolic.clone();
        let session_id_for_tx = session_id.clone();
        let content_for_tx = content.clone();
        let tool_name_for_tx = tool_name.clone();

        let seq = self
            .db
            .write(move |tx| {
                // Ensure the session row exists so receipts and folds can join it.
                tx.execute(
                    "INSERT INTO session(session_id, slot_id, description, created_at, last_seen_at)
                     VALUES (?1, ?2, '', ?3, ?3)
                     ON CONFLICT(session_id) DO UPDATE SET last_seen_at = excluded.last_seen_at,
                                                           slot_id = COALESCE(excluded.slot_id, session.slot_id)",
                    rusqlite::params![session_id_for_tx, slot_id, created_at],
                )?;

                let seq: i64 = tx.query_row(
                    "SELECT COALESCE(MAX(seq), 0) + 1 FROM episodic_stream",
                    [],
                    |r| r.get(0),
                )?;

                tx.execute(
                    "INSERT INTO episodic_stream
                        (episode_id, seq, session_id, slot_id, role, content, tool_name,
                         token_count, created_at, fold_id, eviction_tier, droppable, meta_json,
                         project_id)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 'live', ?11, ?12, ?13)",
                    rusqlite::params![
                        episode_id_for_tx,
                        seq,
                        session_id,
                        slot_id,
                        role.as_str(),
                        content_for_tx,
                        tool_name_for_tx,
                        tokens,
                        created_at,
                        fold_id,
                        i64::from(droppable),
                        meta_json,
                        project_for_tx,
                    ],
                )?;

                // Symbolic facts extracted from structured tool output, written
                // in the same transaction, tagged with a deterministic source.
                let mut facts_extracted = 0usize;
                if let Some(facts) = &symbolic_for_tx {
                    for write in &facts.facts {
                        let fact = write
                            .clone()
                            .into_fact(FactSource::ToolOutputParser, None);
                        upsert_symbolic_fact_tx(tx, &fact)?;
                        insert_edge_tx(
                            tx,
                            &NodeRef::episode(&episode_id_for_tx),
                            &NodeRef::fact(&fact.fact_id),
                            EdgeKind::DerivedFrom,
                        )?;
                        facts_extracted += 1;
                    }
                }

                Ok((seq, facts_extracted))
            })
            .await?;

        // Constraint detection is deliberately *outside* the transaction: it is a
        // proposal, not a fact, and must never be able to fail a commit.
        let anchor_proposal = if detect_constraints {
            ConstraintDetector::detect(&episode_id, seq.0, role, &content)
        } else {
            None
        };

        Ok(CommitOutcome {
            episode_id,
            seq: seq.0,
            token_count: tokens,
            facts_extracted: seq.1,
            symbolic_summary: symbolic
                .map(|s| s.summary)
                .unwrap_or_else(|| "not a tool result".into()),
            anchor_proposal,
        })
    }

    /// Fetch one episode by id.
    pub async fn episode(&self, episode_id: &str) -> Result<EpisodeRow> {
        let id = episode_id.to_string();
        let id_for_msg = id.clone();
        self.db
            .with(move |c| {
                c.query_row(
                    &format!("SELECT {EPISODE_COLS} FROM episodic_stream WHERE episode_id = ?1"),
                    [id],
                    map_episode,
                )
                .optional()?
                .ok_or_else(|| Error::NotFound(format!("episode {id_for_msg}")))
            })
            .await
    }

    /// All live-window episodes of a session, oldest first.
    pub async fn session_episodes(&self, session_id: &str) -> Result<Vec<EpisodeRow>> {
        let sid = session_id.to_string();
        self.db
            .with(move |c| {
                let mut stmt = c.prepare(&format!(
                    "SELECT {EPISODE_COLS} FROM episodic_stream
                     WHERE session_id = ?1 ORDER BY seq ASC"
                ))?;
                let rows = stmt.query_map([sid], map_episode)?;
                Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
            })
            .await
    }

    /// The most recent `limit` episodes of a session, oldest first.
    pub async fn recent_episodes(&self, session_id: &str, limit: usize) -> Result<Vec<EpisodeRow>> {
        let sid = session_id.to_string();
        let limit = limit as i64;
        self.db
            .with(move |c| {
                let mut stmt = c.prepare(&format!(
                    "SELECT {EPISODE_COLS} FROM (
                        SELECT * FROM episodic_stream WHERE session_id = ?1
                        ORDER BY seq DESC LIMIT ?2
                     ) ORDER BY seq ASC"
                ))?;
                let rows = stmt.query_map(rusqlite::params![sid, limit], map_episode)?;
                Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
            })
            .await
    }

    /// Episodes that are live (tier = live) and evictable, oldest first.
    ///
    /// This is the eviction engine's candidate set — anchors and folded content
    /// are structurally absent from it.
    pub async fn evictable_episodes(&self, session_id: &str) -> Result<Vec<EpisodeRow>> {
        let sid = session_id.to_string();
        self.db
            .with(move |c| {
                let mut stmt = c.prepare(&format!(
                    "SELECT {EPISODE_COLS} FROM episodic_stream
                     WHERE session_id = ?1 AND fold_id IS NULL
                     ORDER BY seq ASC"
                ))?;
                let rows = stmt.query_map([sid], map_episode)?;
                Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
            })
            .await
    }

    /// Episodes in any tier, for fold trace retrieval.
    pub async fn episodes_in_fold(&self, fold_id: &str) -> Result<Vec<EpisodeRow>> {
        let fid = fold_id.to_string();
        self.db
            .with(move |c| {
                let mut stmt = c.prepare(&format!(
                    "SELECT {EPISODE_COLS} FROM episodic_stream
                     WHERE fold_id = ?1 ORDER BY seq ASC"
                ))?;
                let rows = stmt.query_map([fid], map_episode)?;
                Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
            })
            .await
    }

    /// Move one episode to a different eviction tier.
    ///
    /// Note what this does *not* do: it never touches `content`. That single
    /// omission is why FR-5's "an evicted-then-recalled episode's content is
    /// bit-identical to the original" holds by construction.
    pub async fn set_tier(&self, episode_id: &str, tier: EpisodeTier) -> Result<()> {
        let id = episode_id.to_string();
        let tier_s = tier.as_str().to_string();
        self.db
            .write(move |tx| {
                let n = tx.execute(
                    "UPDATE episodic_stream SET eviction_tier = ?2 WHERE episode_id = ?1",
                    rusqlite::params![id, tier_s],
                )?;
                if n == 0 {
                    return Err(Error::NotFound(format!("episode {id}")));
                }
                Ok(())
            })
            .await
    }

    /// Move many episodes to the same tier in one transaction.
    pub async fn set_tiers(&self, updates: Vec<(String, EpisodeTier)>) -> Result<usize> {
        if updates.is_empty() {
            return Ok(0);
        }
        self.db
            .write(move |tx| {
                let mut n = 0usize;
                for (id, tier) in &updates {
                    n += tx.execute(
                        "UPDATE episodic_stream SET eviction_tier = ?2 WHERE episode_id = ?1",
                        rusqlite::params![id, tier.as_str()],
                    )?;
                }
                Ok(n)
            })
            .await
    }

    /// Record that one episode supersedes another, so the eviction engine knows
    /// the superseded one is a safe `Drop` candidate (FR-5 tier 4).
    ///
    /// # An id that matches nothing is an error
    ///
    /// This used to run the `UPDATE`, ignore how many rows it touched, and write the edge regardless —
    /// so correcting an episode that does not exist **reported success** and left no record that
    /// anything was wrong. A caller who mistypes an id has not corrected anything, and the whole point
    /// of a correction is that the record says so.
    ///
    /// Checked before the edge is written, so a failed correction leaves no half-record: the count is
    /// the evidence, and writing `Supersedes` for a row that was not touched would assert a
    /// relationship that does not hold.
    pub async fn mark_superseded(&self, episode_id: &str, by_episode_id: &str) -> Result<()> {
        let id = episode_id.to_string();
        let by = by_episode_id.to_string();
        let for_error = episode_id.to_string();
        self.db
            .write(move |tx| {
                let touched = tx.execute(
                    "UPDATE episodic_stream SET superseded_by = ?2, droppable = 1
                     WHERE episode_id = ?1",
                    rusqlite::params![id, by],
                )?;
                if touched == 0 {
                    return Err(Error::NotFound(format!(
                        "cannot correct episode {for_error}: no such episode in this store"
                    )));
                }
                insert_edge_tx(
                    tx,
                    &NodeRef::episode(&by),
                    &NodeRef::episode(&id),
                    EdgeKind::Supersedes,
                )?;
                Ok(())
            })
            .await
    }

    /// Build a budgeted, tier-aware rendering of a session (FR-15's "raw recent
    /// history" category).
    ///
    /// Selection is newest-first because recency is the strongest relevance
    /// signal available without a model, but rendering is chronological because a
    /// transcript read out of order is worse than one that is truncated.
    pub async fn timeline(
        &self,
        session_id: &str,
        budget_tokens: usize,
        counter: &TokenCounter,
        include_folded: bool,
    ) -> Result<SessionTimeline> {
        let mut episodes = self.session_episodes(session_id).await?;
        if !include_folded {
            episodes.retain(|e| e.fold_id.is_none());
        }

        let total_tokens: usize = episodes.iter().map(|e| e.live_tokens(counter)).sum();

        // Walk newest -> oldest, taking what fits.
        let mut selected: Vec<usize> = Vec::new();
        let mut used = 0usize;
        for (idx, ep) in episodes.iter().enumerate().rev() {
            let cost = ep.live_tokens(counter);
            if used + cost > budget_tokens && !selected.is_empty() {
                break;
            }
            used += cost;
            selected.push(idx);
        }
        selected.reverse();
        let chosen: std::collections::HashSet<usize> = selected.iter().copied().collect();

        let mut rendered = String::new();
        let mut items = Vec::with_capacity(episodes.len());
        for (idx, ep) in episodes.iter().enumerate() {
            let included = chosen.contains(&idx) && !ep.eviction_tier.is_out_of_window();
            let text = ep.render();
            if included {
                let role = ep.role.as_str();
                let tool = ep.tool_name.as_deref().map(|t| format!("[{t}] ")).unwrap_or_default();
                rendered.push_str(&format!("<{role}> {tool}{text}\n"));
            }
            items.push(TimelineItem {
                episode_id: ep.episode_id.clone(),
                seq: ep.seq,
                role: ep.role.clone(),
                tier: ep.eviction_tier,
                tokens: ep.live_tokens(counter),
                included,
                preview: preview_of(&text, 120),
            });
        }

        let included_tokens = items.iter().filter(|i| i.included).map(|i| i.tokens).sum();
        let evicted = items.iter().filter(|i| !i.included).count();

        Ok(SessionTimeline {
            rendered,
            items,
            included_tokens,
            total_tokens,
            episodes_total: episodes.len(),
            episodes_included: selected.len().min(episodes.len()),
            episodes_evicted: evicted,
            reclaimed_tokens: total_tokens.saturating_sub(included_tokens),
        })
    }

    // =======================================================================
    // Symbolic Ledger
    // =======================================================================

    /// Insert or refresh a batch of deterministic facts.
    ///
    /// Refreshing is by natural key: the same symbol re-parsed replaces its row
    /// and its `updated_at`/`ast_hash`. That is what makes re-indexing a single
    /// changed file O(symbols in file).
    pub async fn upsert_facts(&self, facts: Vec<SymbolicFact>) -> Result<usize> {
        if facts.is_empty() {
            return Ok(0);
        }
        self.db
            .write(move |tx| {
                for f in &facts {
                    upsert_symbolic_fact_tx(tx, f)?;
                }
                Ok(facts.len())
            })
            .await
    }

    /// Current hash of an anchor, whatever track it lives in.
    ///
    /// `Ok(None)` means the anchor does not exist. Callers must treat that as
    /// staleness, not as "no information" — see [`SemanticEntry::from_row`].
    pub async fn anchor_hash(
        &self,
        anchor_type: AnchorType,
        anchor_id: &str,
    ) -> Result<Option<String>> {
        let id = anchor_id.to_string();
        self.db
            .with(move |c| match anchor_type {
                AnchorType::SymbolicFact => Ok(c
                    .query_row("SELECT ast_hash FROM symbolic_fact WHERE fact_id = ?1", [id], |r| {
                        r.get::<_, String>(0)
                    })
                    .optional()?),
                AnchorType::EpisodicStream => Ok(c
                    .query_row(
                        "SELECT printf('%016x', seq) FROM episodic_stream WHERE episode_id = ?1",
                        [id],
                        |r| r.get::<_, String>(0),
                    )
                    .optional()?),
            })
            .await
    }

    /// Look up a fact by its qualified name.
    pub async fn fact_by_name(&self, qualified_name: &str) -> Result<Option<SymbolicFact>> {
        let name = qualified_name.to_string();
        self.db
            .with(move |c| {
                Ok(c.query_row(
                    &format!(
                        "SELECT {FACT_COLS} FROM symbolic_fact WHERE qualified_name = ?1 LIMIT 1"
                    ),
                    [name],
                    map_fact,
                )
                .optional()?)
            })
            .await
    }

    /// Look up a fact by id.
    pub async fn fact_by_id(&self, fact_id: &str) -> Result<Option<SymbolicFact>> {
        let id = fact_id.to_string();
        self.db
            .with(move |c| {
                Ok(c.query_row(
                    &format!("SELECT {FACT_COLS} FROM symbolic_fact WHERE fact_id = ?1"),
                    [id],
                    map_fact,
                )
                .optional()?)
            })
            .await
    }

    /// All facts attached to one file, ordered by line.
    pub async fn facts_in_file(&self, rel_path: &str) -> Result<Vec<SymbolicFact>> {
        let path = rel_path.to_string();
        self.db
            .with(move |c| {
                let mut stmt = c.prepare(&format!(
                    "SELECT {FACT_COLS} FROM symbolic_fact WHERE file_path = ?1
                     ORDER BY COALESCE(line_start, 0)"
                ))?;
                let rows = stmt.query_map([path], map_fact)?;
                Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
            })
            .await
    }

    /// Remove every fact (and its edges) belonging to files that no longer exist.
    ///
    /// Called after a Repo Cortex re-index so a deleted file's symbols stop being
    /// offered to the model as current truth.
    pub async fn forget_files(&self, rel_paths: Vec<String>) -> Result<usize> {
        if rel_paths.is_empty() {
            return Ok(0);
        }
        self.db
            .write(move |tx| {
                let mut removed = 0usize;
                for path in &rel_paths {
                    let ids: Vec<String> = {
                        let mut stmt =
                            tx.prepare("SELECT fact_id FROM symbolic_fact WHERE file_path = ?1")?;
                        let rows = stmt.query_map([path], |r| r.get::<_, String>(0))?;
                        rows.collect::<rusqlite::Result<Vec<_>>>()?
                    };
                    for id in &ids {
                        tx.execute(
                            "DELETE FROM dependency_graph_edge
                             WHERE (src_type='symbolic_fact' AND src_id=?1)
                                OR (dst_type='symbolic_fact' AND dst_id=?1)",
                            [id],
                        )?;
                    }
                    removed +=
                        tx.execute("DELETE FROM symbolic_fact WHERE file_path = ?1", [path])?;
                }
                Ok(removed)
            })
            .await
    }

    /// A ready-to-inject outline of one file's symbols, for `code.query_symbol`
    /// and the fold preamble.
    pub async fn file_outline(&self, rel_path: &str) -> Result<String> {
        let facts = self.facts_in_file(rel_path).await?;
        if facts.is_empty() {
            return Ok(String::new());
        }
        let mut out = format!("{rel_path}\n");
        for f in facts {
            if f.kind == FactKind::Import {
                continue;
            }
            out.push_str(&format!("  {}\n", f.render()));
        }
        Ok(out)
    }

    // =======================================================================
    // Semantic Atlas
    // =======================================================================

    /// Write an Atlas entry, resolving and recording the anchor hash.
    ///
    /// FR-3's first acceptance criterion ("insert is rejected if anchor_ref is
    /// null or points to a non-existent row") is enforced here by *reading the
    /// anchor first*: a missing anchor is an error, not a row with a null hash.
    pub async fn put_semantic(&self, write: SemanticWrite) -> Result<SemanticEntry> {
        write.validate()?;
        let anchor_type = write.anchor_type;
        let anchor_id = write.anchor_id.clone();

        let current = self.anchor_hash(anchor_type, &anchor_id).await?.ok_or_else(|| {
            Error::Integrity(format!(
                "cannot anchor a Semantic Atlas entry to a non-existent {} ({anchor_id}) — FR-3",
                anchor_type.as_str()
            ))
        })?;

        // Extra anchors must also exist; a summary that claims to depend on
        // something imaginary is exactly the drift Sakur4 exists to prevent.
        for (kind, id) in &write.extra_anchors {
            if self.anchor_hash(*kind, id).await?.is_none() {
                return Err(Error::Integrity(format!(
                    "cannot anchor to a non-existent {} ({id}) — FR-3",
                    kind.as_str()
                )));
            }
        }

        let atlas_id = new_id("atlas");
        let recorded_hash = write.anchor_hash_at_write.clone().unwrap_or_else(|| current.clone());
        let now = now_rfc3339();

        let id_for_tx = atlas_id.clone();
        let content = write.content.clone();
        let model = write.model.clone();
        let project_id = write.project_id.clone();
        let session_id = write.session_id.clone();
        let extras = write.extra_anchors.clone();

        self.db
            .write(move |tx| {
                tx.execute(
                    "INSERT INTO semantic_atlas
                        (atlas_id, content, anchor_type, anchor_id, anchor_hash_at_write,
                         model, project_id, session_id, created_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                    rusqlite::params![
                        id_for_tx,
                        content,
                        anchor_type.as_str(),
                        anchor_id,
                        recorded_hash,
                        model,
                        project_id,
                        session_id,
                        now
                    ],
                )?;
                tx.execute(
                    "INSERT OR IGNORE INTO semantic_anchor_link(atlas_id, anchor_type, anchor_id)
                     VALUES (?1, ?2, ?3)",
                    rusqlite::params![id_for_tx, anchor_type.as_str(), anchor_id],
                )?;
                for (kind, id) in &extras {
                    tx.execute(
                        "INSERT OR IGNORE INTO semantic_anchor_link(atlas_id, anchor_type, anchor_id)
                         VALUES (?1, ?2, ?3)",
                        rusqlite::params![id_for_tx, kind.as_str(), id],
                    )?;
                }
                insert_edge_tx(
                    tx,
                    &NodeRef::atlas(&id_for_tx),
                    &match anchor_type {
                        AnchorType::SymbolicFact => NodeRef::fact(&anchor_id),
                        AnchorType::EpisodicStream => NodeRef::episode(&anchor_id),
                    },
                    EdgeKind::DerivedFrom,
                )?;
                for (kind, id) in &extras {
                    let dst = match kind {
                        AnchorType::SymbolicFact => NodeRef::fact(id),
                        AnchorType::EpisodicStream => NodeRef::episode(id),
                    };
                    insert_edge_tx(tx, &NodeRef::atlas(&id_for_tx), &dst, EdgeKind::DerivedFrom)?;
                }
                Ok(())
            })
            .await?;

        self.semantic_entry(&atlas_id).await
    }

    /// Fetch one Atlas entry with staleness resolved.
    pub async fn semantic_entry(&self, atlas_id: &str) -> Result<SemanticEntry> {
        let id = atlas_id.to_string();
        let row = self
            .db
            .with(move |c| {
                Ok(c.query_row(
                    "SELECT sa.atlas_id, sa.content, sa.anchor_type, sa.anchor_id,
                            sa.anchor_hash_at_write, s.current_anchor_hash,
                            sa.model, sa.project_id, sa.session_id, sa.created_at
                     FROM semantic_atlas sa
                     LEFT JOIN semantic_atlas_staleness s ON s.atlas_id = sa.atlas_id
                     WHERE sa.atlas_id = ?1",
                    [id],
                    map_semantic,
                )
                .optional()?)
            })
            .await?
            .ok_or_else(|| Error::NotFound(format!("semantic entry {atlas_id}")))?;

        let anchors = self.semantic_anchors(atlas_id).await?;
        let mut entry = row;
        entry.anchors = anchors;
        Ok(entry)
    }

    /// Every anchor an entry depends on.
    pub async fn semantic_anchors(&self, atlas_id: &str) -> Result<Vec<(AnchorType, String)>> {
        let id = atlas_id.to_string();
        self.db
            .with(move |c| {
                let mut stmt = c.prepare(
                    "SELECT anchor_type, anchor_id FROM semantic_anchor_link WHERE atlas_id = ?1",
                )?;
                let rows = stmt.query_map([id], |r| {
                    let t: String = r.get(0)?;
                    let i: String = r.get(1)?;
                    Ok((t, i))
                })?;
                let mut out = Vec::new();
                for row in rows {
                    let (t, i) = row?;
                    out.push((AnchorType::parse(&t)?, i));
                }
                Ok(out)
            })
            .await
    }

    /// Compute staleness across the whole Atlas (or one project).
    pub async fn staleness_report(
        &self,
        project_id: Option<&str>,
        limit: usize,
    ) -> Result<StalenessReport> {
        let project = project_id.map(String::from);
        let limit = limit as i64;
        self.db
            .with(move |c| {
                let total: i64 = match &project {
                    Some(p) => c.query_row(
                        "SELECT COUNT(*) FROM semantic_atlas WHERE project_id = ?1",
                        [p],
                        |r| r.get(0),
                    )?,
                    None => c.query_row("SELECT COUNT(*) FROM semantic_atlas", [], |r| r.get(0))?,
                };

                let mut sql = String::from(
                    "SELECT sa.atlas_id, sa.anchor_type, sa.anchor_id, sa.anchor_hash_at_write,
                            s.current_anchor_hash
                     FROM semantic_atlas sa
                     LEFT JOIN semantic_atlas_staleness s ON s.atlas_id = sa.atlas_id
                     WHERE (s.current_anchor_hash IS NULL
                            OR s.current_anchor_hash <> sa.anchor_hash_at_write)",
                );
                if project.is_some() {
                    sql.push_str(" AND sa.project_id = ?1");
                }
                sql.push_str(" ORDER BY sa.created_at DESC LIMIT ?");
                sql.push_str(if project.is_some() { "2" } else { "1" });

                let mut stmt = c.prepare(&sql)?;
                let mapper = |r: &Row<'_>| -> rusqlite::Result<(
                    String,
                    String,
                    String,
                    String,
                    Option<String>,
                )> {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
                };
                let rows: Vec<(String, String, String, String, Option<String>)> = match &project {
                    Some(p) => stmt
                        .query_map(rusqlite::params![p, limit], mapper)?
                        .collect::<rusqlite::Result<Vec<_>>>()?,
                    None => stmt
                        .query_map(rusqlite::params![limit], mapper)?
                        .collect::<rusqlite::Result<Vec<_>>>()?,
                };

                let mut entries = Vec::new();
                let mut deleted = 0usize;
                for (atlas_id, at, aid, recorded, current) in rows {
                    if current.is_none() {
                        deleted += 1;
                    }
                    entries.push(StaleEntry {
                        atlas_id,
                        anchor_type: AnchorType::parse(&at)?,
                        anchor_id: aid,
                        recorded_hash: recorded,
                        current_hash: current.clone(),
                        reason: if current.is_none() {
                            StaleReason::AnchorDeleted
                        } else {
                            StaleReason::AnchorChanged
                        },
                    });
                }
                Ok(StalenessReport {
                    total: total as usize,
                    stale: entries.len(),
                    deleted_anchors: deleted,
                    entries,
                })
            })
            .await
    }

    /// Replace an entry's content and refresh its recorded anchor hash.
    ///
    /// Used by the Idle Consolidator when it regenerates a drifted summary.
    pub async fn refresh_semantic(
        &self,
        atlas_id: &str,
        new_content: String,
    ) -> Result<SemanticEntry> {
        let existing = self.semantic_entry(atlas_id).await?;
        let current = self
            .anchor_hash(existing.anchor_type, &existing.anchor_id)
            .await?
            .ok_or_else(|| {
                Error::Integrity(format!(
                    "cannot refresh {}: its anchor {} no longer exists",
                    atlas_id, existing.anchor_id
                ))
            })?;
        let id = atlas_id.to_string();
        let now = now_rfc3339();
        self.db
            .write(move |tx| {
                tx.execute(
                    "UPDATE semantic_atlas
                     SET content = ?2, anchor_hash_at_write = ?3, regenerated_at = ?4
                     WHERE atlas_id = ?1",
                    rusqlite::params![id, new_content, current, now],
                )?;
                Ok(())
            })
            .await?;
        self.semantic_entry(atlas_id).await
    }

    // =======================================================================
    // Anchor Set
    // =======================================================================

    /// Pin an entry. Returns the created row.
    pub async fn pin(&self, req: PinRequest) -> Result<AnchorRow> {
        let row = req.into_row()?;
        let r = row.clone();
        self.db
            .write(move |tx| {
                tx.execute(
                    "INSERT INTO anchor_set
                        (anchor_id, content, kind, session_id, project_id, pinned_by, created_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    rusqlite::params![
                        r.anchor_id,
                        r.content,
                        r.kind.as_str(),
                        r.session_id,
                        r.project_id,
                        r.pinned_by,
                        r.created_at
                    ],
                )?;
                Ok(())
            })
            .await?;
        Ok(row)
    }

    /// The Anchor Set, safety-first.
    pub async fn anchors(&self, session_id: Option<&str>) -> Result<Vec<AnchorRow>> {
        let session = session_id.map(String::from);
        self.db
            .with(move |c| {
                let mut stmt = c.prepare(
                    "SELECT anchor_id, content, kind, session_id, project_id, pinned_by, created_at
                     FROM anchor_set
                     WHERE (?1 IS NULL OR session_id IS NULL OR session_id = ?1)",
                )?;
                let rows = stmt.query_map(rusqlite::params![session], |r| {
                    let kind: String = r.get(2)?;
                    Ok(AnchorRow {
                        anchor_id: r.get(0)?,
                        content: r.get(1)?,
                        // A stored kind that fails to parse is a schema-level
                        // impossibility (the column has a CHECK constraint); map
                        // it to a conversion error rather than panicking.
                        kind: crate::memory::anchor::AnchorKind::parse(&kind).map_err(|e| {
                            rusqlite::Error::FromSqlConversionFailure(
                                2,
                                rusqlite::types::Type::Text,
                                Box::new(e),
                            )
                        })?,
                        session_id: r.get(3)?,
                        project_id: r.get(4)?,
                        pinned_by: r.get(5)?,
                        created_at: r.get(6)?,
                    })
                })?;
                Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
            })
            .await
    }

    /// Unpin (delete) an anchor by id. Anchors are the one table with a delete
    /// path, because unpinning is an explicit human act — unlike forgetting an
    /// episode, which must never be silent.
    pub async fn unpin(&self, anchor_id: &str) -> Result<bool> {
        let id = anchor_id.to_string();
        self.db
            .write(move |tx| {
                let n = tx.execute("DELETE FROM anchor_set WHERE anchor_id = ?1", [id])?;
                Ok(n > 0)
            })
            .await
    }

    // =======================================================================
    // Dependency Graph
    // =======================================================================

    /// Add an edge.
    pub async fn add_edge(&self, edge: EdgeRow) -> Result<()> {
        self.db
            .write(move |tx| {
                let (a, b, c, d, e, f, g, h) = edge.params();
                tx.execute(EdgeRow::insert_sql(), rusqlite::params![a, b, c, d, e, f, g, h])?;
                Ok(())
            })
            .await
    }

    /// Load the graph neighbourhood around a node.
    ///
    /// Bounded by `limit` edges so a pathological graph cannot make an eviction
    /// decision O(whole database).
    pub async fn graph_around(&self, node: &NodeRef, limit: usize) -> Result<DependencyGraph> {
        let kind = node.kind.as_str().to_string();
        let id = node.id.clone();
        let limit = limit as i64;
        let edges = self
            .db
            .with(move |c| {
                // A two-hop load: edges touching the node, plus edges touching
                // those neighbours. Enough for eviction ordering and blast
                // radius at the depths Sakur4 exposes.
                let mut stmt = c.prepare(
                    "WITH seed AS (
                         SELECT src_type, src_id, dst_type, dst_id, edge_kind, weight, target_hash
                         FROM dependency_graph_edge
                         WHERE (src_type = ?1 AND src_id = ?2)
                            OR (dst_type = ?1 AND dst_id = ?2)
                         LIMIT ?3
                     ),
                     hop2 AS (
                         SELECT e.src_type, e.src_id, e.dst_type, e.dst_id, e.edge_kind, e.weight,
                                e.target_hash
                         FROM dependency_graph_edge e
                         JOIN seed s ON (e.src_type = s.dst_type AND e.src_id = s.dst_id)
                                     OR (e.dst_type = s.src_type AND e.dst_id = s.src_id)
                         LIMIT ?3
                     )
                     SELECT DISTINCT * FROM (SELECT * FROM seed UNION SELECT * FROM hop2)",
                )?;
                let rows = stmt.query_map(rusqlite::params![kind, id, limit], |r| {
                    let k: String = r.get(4)?;
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                        k,
                        r.get::<_, f64>(5)?,
                        r.get::<_, Option<String>>(6)?,
                    ))
                })?;
                let mut out = Vec::new();
                for row in rows {
                    let (st, si, dt, di, k, w, th) = row?;
                    let kind = EdgeKind::parse(&k)?;
                    out.push(EdgeRow {
                        src_type: st,
                        src_id: si,
                        dst_type: dt,
                        dst_id: di,
                        edge_kind: kind,
                        weight: w,
                        target_hash: th,
                    });
                }
                Ok(out)
            })
            .await?;
        Ok(DependencyGraph::from_edges(edges))
    }

    // =======================================================================
    // Sessions
    // =======================================================================

    /// Ensure a session row exists and stamp it as seen.
    pub async fn touch_session(
        &self,
        session_id: &str,
        slot_id: Option<&str>,
        description: &str,
    ) -> Result<()> {
        let sid = session_id.to_string();
        let slot = slot_id.map(String::from);
        let desc = description.to_string();
        let now = now_rfc3339();
        self.db
            .write(move |tx| {
                tx.execute(
                    "INSERT INTO session(session_id, slot_id, description, created_at, last_seen_at)
                     VALUES (?1, ?2, ?3, ?4, ?4)
                     ON CONFLICT(session_id) DO UPDATE SET
                        last_seen_at = excluded.last_seen_at,
                        slot_id = COALESCE(excluded.slot_id, session.slot_id),
                        description = CASE WHEN excluded.description = '' THEN session.description
                                           ELSE excluded.description END",
                    rusqlite::params![sid, slot, desc, now],
                )?;
                Ok(())
            })
            .await
    }

    /// Tokens the session's live window currently costs.
    ///
    /// Measured by rendering the same timeline the eviction engine and the
    /// receipt see, rather than by summing stored per-episode counts. The two
    /// differ by the role tags and separators the renderer adds, and a budget
    /// decision taken on one number while a receipt prints the other is exactly
    /// the kind of quiet disagreement this project exists to eliminate.
    pub async fn session_live_tokens(
        &self,
        session_id: &str,
        counter: &TokenCounter,
    ) -> Result<usize> {
        Ok(self.timeline(session_id, usize::MAX, counter, false).await?.included_tokens)
    }

    /// Distinct sessions known to the store, newest first.
    pub async fn sessions(&self, limit: usize) -> Result<Vec<(String, Option<String>, String)>> {
        let limit = limit as i64;
        self.db
            .with(move |c| {
                let mut stmt = c.prepare(
                    "SELECT session_id, slot_id, last_seen_at FROM session
                     ORDER BY last_seen_at DESC LIMIT ?1",
                )?;
                let rows = stmt.query_map([limit], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
                Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
            })
            .await
    }

    /// Total number of episodes in the store (for stats and receipts).
    ///
    /// Nothing calls this: `Db::stats()` supplies the same number to `sakur4.status`, and the
    /// receipt's breakdown counts tokens rather than episodes. Left in place rather than deleted,
    /// because it is a one-line query with a clear purpose and no cost — unlike a comment claiming
    /// a caller it does not have.
    pub async fn episode_count(&self) -> Result<i64> {
        self.db
            .with(|c| Ok(c.query_row("SELECT COUNT(*) FROM episodic_stream", [], |r| r.get(0))?))
            .await
    }

    /// Bulk-load the symbolic facts of a whole project, for the repo map's
    /// centrality pass.
    pub async fn project_facts(&self, project_id: &str) -> Result<Vec<SymbolicFact>> {
        let pid = project_id.to_string();
        self.db
            .with(move |c| {
                let mut stmt = c.prepare(&format!(
                    "SELECT {FACT_COLS} FROM symbolic_fact WHERE project_id = ?1"
                ))?;
                let rows = stmt.query_map([pid], map_fact)?;
                Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
            })
            .await
    }

    /// Facts whose qualified names match any of `names`.
    ///
    /// # Nothing calls this, and unlike its neighbours that is not a defect — the path it names is redundant
    ///
    /// Its doc claimed *"used by staleness re-resolution in the recall engine"*, and a previous round corrected
    /// that to say nothing calls it, leaving the method because a re-resolution path *"does not exist yet"*.
    /// **Reading that path settles it: it never needs to exist.** Staleness is resolved by a single statement
    /// that joins `semantic_atlas` to `symbolic_fact` and returns `recorded_hash` and `current_hash` side by
    /// side, and `StaleEntry` is built from those rows. The comparison is the SQL, so a batch lookup of facts
    /// by name has nothing to contribute — the render path already has both hashes by the time it needs them.
    ///
    /// Which makes this different from the five other uncalled functions this project has found. `open_folds`,
    /// `last_indexed`, `ImpactEntry::stale`, `render_anchor_block` and `is_backend_unavailable` were each a
    /// **guarantee with no caller**, and each was fixed by wiring the caller up. This is an accessor whose
    /// caller was designed around it — the join is the design, and it is the better one.
    ///
    /// Left in place as library surface, with the reasoning here rather than a promise about future work.
    /// **The earlier comment was the real defect**: it told a reader a path was coming, which would have
    /// invited someone to build it.
    pub async fn facts_by_names(&self, names: Vec<String>) -> Result<Vec<SymbolicFact>> {
        if names.is_empty() {
            return Ok(Vec::new());
        }
        self.db
            .with(move |c| {
                let mut out = Vec::new();
                let mut stmt = c.prepare(&format!(
                    "SELECT {FACT_COLS} FROM symbolic_fact WHERE qualified_name = ?1"
                ))?;
                for n in &names {
                    if let Some(f) = stmt.query_row([n], map_fact).optional()? {
                        out.push(f);
                    }
                }
                Ok(out)
            })
            .await
    }
}

/// Column list for episodes, kept in one place so every mapper agrees.
const EPISODE_COLS: &str = "episode_id, seq, session_id, slot_id, role, content, tool_name, \
                            token_count, created_at, fold_id, eviction_tier, droppable, superseded_by, \
                            project_id";

const FACT_COLS: &str = "fact_id, kind, qualified_name, file_path, line_start, line_end, \
                         signature, ast_hash, source, project_id, parent_name, body, updated_at";

fn map_episode(r: &Row<'_>) -> rusqlite::Result<EpisodeRow> {
    let tier: String = r.get(10)?;
    Ok(EpisodeRow {
        episode_id: r.get(0)?,
        seq: r.get(1)?,
        session_id: r.get(2)?,
        slot_id: r.get(3)?,
        role: r.get(4)?,
        content: r.get(5)?,
        tool_name: r.get(6)?,
        token_count: r.get(7)?,
        created_at: r.get(8)?,
        fold_id: r.get(9)?,
        eviction_tier: EpisodeTier::parse(&tier).unwrap_or(EpisodeTier::Live),
        droppable: r.get::<_, i64>(11)? != 0,
        superseded_by: r.get(12)?,
        project_id: r.get(13)?,
    })
}

fn map_fact(r: &Row<'_>) -> rusqlite::Result<SymbolicFact> {
    let kind: String = r.get(1)?;
    let source: String = r.get(8)?;
    Ok(SymbolicFact {
        fact_id: r.get(0)?,
        kind: FactKind::parse(&kind).unwrap_or(FactKind::Type),
        qualified_name: r.get(2)?,
        file_path: r.get(3)?,
        line_start: r.get(4)?,
        line_end: r.get(5)?,
        signature: r.get(6)?,
        ast_hash: r.get(7)?,
        source: match source.as_str() {
            "tree_sitter" => FactSource::TreeSitter,
            "tool_output_parser" => FactSource::ToolOutputParser,
            _ => FactSource::CommandName,
        },
        project_id: r.get(9)?,
        parent_name: r.get(10)?,
        body: r.get(11)?,
        updated_at: r.get(12)?,
    })
}

fn map_semantic(r: &Row<'_>) -> rusqlite::Result<SemanticEntry> {
    let at: String = r.get(2)?;
    Ok(SemanticEntry::from_row(
        r.get(0)?,
        r.get(1)?,
        AnchorType::parse(&at).unwrap_or(AnchorType::SymbolicFact),
        r.get(3)?,
        r.get(4)?,
        r.get(5)?,
        r.get(6)?,
        r.get(7)?,
        r.get(8)?,
        r.get(9)?,
    ))
}

fn preview_of(text: &str, limit: usize) -> String {
    let flat = text.replace('\n', " ⏎ ");
    if flat.chars().count() <= limit {
        flat
    } else {
        let mut s: String = flat.chars().take(limit).collect();
        s.push('…');
        s
    }
}

/// Insert-or-refresh a deterministic fact. Shared by the Fabric and the indexer.
pub(crate) fn upsert_symbolic_fact_tx(
    tx: &crate::store::WriteTxn<'_>,
    fact: &SymbolicFact,
) -> Result<()> {
    // Natural key: (project, qualified name, kind, file), with NULLs folded so
    // the key behaves as one key rather than as "distinct whenever a column is
    // absent". Re-parsing the same symbol refreshes its row instead of
    // accumulating duplicates, which is what makes incremental re-indexing cheap
    // and staleness detection meaningful.
    tx.execute(
        "INSERT INTO symbolic_fact
            (fact_id, kind, qualified_name, file_path, line_start, line_end, signature,
             ast_hash, source, project_id, parent_name, body, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
         ON CONFLICT(COALESCE(project_id, ''), qualified_name, kind, COALESCE(file_path, ''))
         DO UPDATE SET
            line_start = excluded.line_start,
            line_end = excluded.line_end,
            signature = excluded.signature,
            ast_hash = excluded.ast_hash,
            source = excluded.source,
            parent_name = excluded.parent_name,
            body = excluded.body,
            updated_at = excluded.updated_at",
        rusqlite::params![
            fact.fact_id,
            fact.kind.as_str(),
            fact.qualified_name,
            fact.file_path,
            fact.line_start,
            fact.line_end,
            fact.signature,
            fact.ast_hash,
            fact.source.as_str(),
            fact.project_id,
            fact.parent_name,
            fact.body,
            fact.updated_at,
        ],
    )?;
    Ok(())
}

/// Insert-or-refresh an edge.
pub(crate) fn insert_edge_tx(
    tx: &crate::store::WriteTxn<'_>,
    src: &NodeRef,
    dst: &NodeRef,
    kind: EdgeKind,
) -> Result<()> {
    let edge = EdgeRow::new(src, dst, kind);
    let (a, b, c, d, e, f, g, h) = edge.params();
    tx.execute(EdgeRow::insert_sql(), rusqlite::params![a, b, c, d, e, f, g, h])?;
    Ok(())
}
