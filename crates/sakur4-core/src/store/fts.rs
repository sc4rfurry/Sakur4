//! Lexical (BM25) retrieval over the Episodic Stream, Symbolic Ledger and
//! Semantic Atlas using SQLite FTS5.
//!
//! FTS5 ships with the `bundled` SQLite that `rusqlite` builds, so lexical
//! recall is always available in a Sakur4-built binary. The one thing FTS5
//! cannot do safely is accept arbitrary user text as a MATCH expression: a
//! query containing quotes, `*`, `:` or `-` is a syntax error at best and a
//! surprising interpretation at worst. [`sanitize_match`] converts free text
//! into an explicit AND-of-prefixes expression, which is also the behaviour
//! that suits code search (identifiers, partial symbol names).

use crate::error::Result;
use crate::store::db::Db;

/// Which backend served a lexical query.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum LexicalBackend {
    Fts5,
    /// Fallback when FTS5 is unavailable: anchored `LIKE` scan. Correct but
    /// slower; surfaced so the receipt can say which one ran.
    LikeScan,
}

/// One lexical hit.
#[derive(Debug, Clone, serde::Serialize)]
pub struct LexicalHit {
    pub source_table: String,
    pub source_id: String,
    /// BM25 score, normalised so larger is better.
    pub score: f64,
    pub snippet: String,
}

impl Db {
    /// Run a BM25 query against `episodic_fts`.
    ///
    /// `session_id` and `fold_id` are optional hard filters applied after the
    /// MATCH, because FTS5 external-content tables index the whole stream.
    pub async fn search_episodes(
        &self,
        query: &str,
        limit: usize,
        session_id: Option<&str>,
        exclude_folded: bool,
    ) -> Result<Vec<LexicalHit>> {
        let match_expr = sanitize_match(query);
        if match_expr.is_empty() {
            return Ok(Vec::new());
        }
        let session = session_id.map(|s| s.to_string());
        let use_fts = self.has_fts5();
        let limit = limit as i64;
        // The LIKE fallback needs the raw query inside the blocking closure, so
        // own it rather than borrowing the caller's `&str`.
        let raw_query = query.to_string();

        self.with(move |c| {
            if !use_fts {
                return like_scan_episodes(c, &raw_query, limit, session.as_deref());
            }
            let sql = "SELECT f.rowid AS rowid,
                              bm25(episodic_fts) AS score,
                              snippet(episodic_fts, 0, '[', ']', ' … ', 12) AS snip,
                              e.episode_id AS episode_id,
                              e.session_id AS session_id,
                              e.fold_id AS fold_id
                       FROM episodic_fts f
                       JOIN episodic_stream e ON e.rowid = f.rowid
                       WHERE episodic_fts MATCH ?1
                       ORDER BY bm25(episodic_fts)
                       LIMIT ?2";
            let mut stmt = c.prepare(sql)?;
            let rows = stmt.query_map(rusqlite::params![match_expr, limit.max(1)], |r| {
                let id: String = r.get("episode_id")?;
                let score: f64 = r.get("score")?;
                let snip: String = r.get("snip")?;
                let sess: Option<String> = r.get("session_id")?;
                let fold: Option<String> = r.get("fold_id")?;
                Ok((id, score, snip, sess, fold))
            })?;

            let mut out = Vec::new();
            for row in rows {
                let (id, score, snip, sess, fold) = row?;
                if let Some(want) = session.as_deref()
                    && sess.as_deref() != Some(want) {
                        continue;
                    }
                if exclude_folded && fold.is_some() {
                    continue;
                }
                out.push(LexicalHit {
                    source_table: "episodic_stream".into(),
                    source_id: id,
                    // bm25() in SQLite returns a negative value where more
                    // negative is a better match; flip it so callers can treat
                    // "bigger is better" uniformly across all three retrievers.
                    score: -score,
                    snippet: snip,
                });
            }
            Ok(out)
        })
        .await
    }

    /// BM25 over the Semantic Atlas.
    ///
    /// The Atlas needs its own retriever rather than being reached through the
    /// stream: an entry is derived *from* an episode or a fact, so graph
    /// traversal from a stream hit never arrives at it, and dense retrieval only
    /// finds entries that happen to have been embedded. Without this, summaries
    /// were reachable only when a vector existed — which made staleness detection
    /// untestable and, worse, made the Atlas effectively invisible in practice.
    pub async fn search_semantic(
        &self,
        query: &str,
        limit: usize,
        project_id: Option<&str>,
    ) -> Result<Vec<LexicalHit>> {
        let match_expr = sanitize_match(query);
        if match_expr.is_empty() {
            return Ok(Vec::new());
        }
        let project = project_id.map(|s| s.to_string());
        let limit = limit as i64;
        let use_fts = self.has_fts5();
        let raw = query.to_string();

        self.with(move |c| {
            if !use_fts {
                return like_scan_semantic(c, &raw, limit, project.as_deref());
            }
            // FTS5 exposes only `rowid` for an external-content table, so the
            // Atlas id comes from the content table via the rowid join.
            let sql = "SELECT sa.atlas_id AS atlas_id,
                              bm25(semantic_fts) AS score,
                              snippet(semantic_fts, 0, '[', ']', ' … ', 12) AS snip,
                              sa.project_id AS project_id
                       FROM semantic_fts f
                       JOIN semantic_atlas sa ON sa.rowid = f.rowid
                       WHERE semantic_fts MATCH ?1
                       ORDER BY bm25(semantic_fts)
                       LIMIT ?2";
            let mut stmt = c.prepare(sql)?;
            let rows = stmt.query_map(rusqlite::params![match_expr, limit.max(1)], |r| {
                Ok((
                    r.get::<_, String>("atlas_id")?,
                    r.get::<_, f64>("score")?,
                    r.get::<_, String>("snip")?,
                    r.get::<_, Option<String>>("project_id")?,
                ))
            })?;

            let mut out = Vec::new();
            for row in rows {
                let (id, score, snip, proj) = row?;
                if let Some(want) = project.as_deref()
                    && proj.as_deref() != Some(want) {
                        continue;
                    }
                out.push(LexicalHit {
                    source_table: "semantic_atlas".into(),
                    source_id: id,
                    score: -score,
                    snippet: snip,
                });
            }
            Ok(out)
        })
        .await
    }

    /// Semantic Atlas entries anchored to any of the given anchors.
    ///
    /// This is the structural half of Atlas retrieval: if a symbol matched the
    /// query, the interpretation *of that symbol* is relevant regardless of
    /// whether its wording overlaps the query.
    pub async fn semantic_entries_for_anchors(
        &self,
        anchors: Vec<(String, String)>,
        limit: usize,
    ) -> Result<Vec<String>> {
        if anchors.is_empty() {
            return Ok(Vec::new());
        }
        let limit = limit as i64;
        self.with(move |c| {
            let mut out: Vec<String> = Vec::new();
            let mut stmt = c.prepare(
                "SELECT DISTINCT sa.atlas_id
                 FROM semantic_atlas sa
                 JOIN semantic_anchor_link l ON l.atlas_id = sa.atlas_id
                 WHERE l.anchor_type = ?1 AND l.anchor_id = ?2
                 LIMIT ?3",
            )?;
            for (kind, id) in &anchors {
                let rows = stmt.query_map(rusqlite::params![kind, id, limit], |r| {
                    r.get::<_, String>(0)
                })?;
                for row in rows {
                    out.push(row?);
                }
                if out.len() >= limit as usize {
                    break;
                }
            }
            out.dedup();
            Ok(out)
        })
        .await
    }
}

fn like_scan_semantic(
    c: &rusqlite::Connection,
    query: &str,
    limit: i64,
    project: Option<&str>,
) -> Result<Vec<LexicalHit>> {
    let tokens: Vec<String> = query
        .split_whitespace()
        .filter(|t| t.len() >= 2)
        .map(|t| format!("%{}%", t.replace('%', "")))
        .collect();
    if tokens.is_empty() {
        return Ok(Vec::new());
    }
    let where_clause = tokens
        .iter()
        .enumerate()
        .map(|(i, _)| format!("sa.content LIKE ?{}", i + 1))
        .collect::<Vec<_>>()
        .join(" AND ");
    let sql = format!(
        "SELECT sa.atlas_id, sa.content FROM semantic_atlas sa
         WHERE {where_clause} AND (?{p} IS NULL OR sa.project_id = ?{p})
         ORDER BY sa.created_at DESC LIMIT ?{l}",
        p = tokens.len() + 1,
        l = tokens.len() + 2
    );
    let mut stmt = c.prepare(&sql)?;
    let mut params: Vec<Box<dyn rusqlite::ToSql>> = tokens
        .iter()
        .map(|t| Box::new(t.clone()) as Box<dyn rusqlite::ToSql>)
        .collect();
    params.push(Box::new(project.map(|s| s.to_string())));
    params.push(Box::new(limit.max(1)));
    let refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(|b| b.as_ref()).collect();

    let rows = stmt.query_map(refs.as_slice(), |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (id, content) = row?;
        out.push(LexicalHit {
            source_table: "semantic_atlas".into(),
            source_id: id,
            score: 0.0,
            snippet: content.chars().take(240).collect(),
        });
    }
    Ok(out)
}

/// Turn free text into a safe FTS5 MATCH expression.
///
/// Every run of identifier characters becomes a quoted prefix term, joined with
/// `AND`. Quoting removes all FTS5 operator meaning, and prefix matching keeps
/// partial identifiers (`mem_pin` → `"mem_pin"*`) useful while typing.
///
/// Two deliberate details:
///
/// * FTS5 keywords (`AND`, `OR`, `NOT`, `NEAR`) are kept as *quoted terms* rather
///   than dropped. Someone searching for the word "not" means the word, and
///   because it is quoted it cannot act as an operator.
/// * Terms are trimmed of leading and trailing punctuation (`/usr/lib` →
///   `usr/lib`) and single-character fragments are discarded, because a lone
///   `/` or `-` matches nothing and only adds a useless AND clause.
pub fn sanitize_match(query: &str) -> String {
    let mut terms: Vec<String> = Vec::new();
    let mut current = String::new();
    for ch in query.chars() {
        if ch.is_alphanumeric() || ch == '_' || ch == '.' || ch == '/' {
            current.push(ch);
        } else if !current.is_empty() {
            terms.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        terms.push(current);
    }
    terms.truncate(12);
    terms
        .into_iter()
        .map(|t| {
            let t = t.trim_matches(|c: char| c == '/' || c == '.');
            t.replace('"', "")
        })
        .filter(|t| t.chars().any(|c| c.is_alphanumeric() || c == '_'))
        .map(|t| format!("\"{t}\"*"))
        .collect::<Vec<_>>()
        .join(" AND ")
}

fn like_scan_episodes(
    c: &rusqlite::Connection,
    query: &str,
    limit: i64,
    session: Option<&str>,
) -> Result<Vec<LexicalHit>> {
    // Split on whitespace and require every token to appear, mirroring the
    // AND semantics of the FTS5 path so ranking behaviour does not change
    // shape when FTS5 is missing.
    let tokens: Vec<String> = query
        .split_whitespace()
        .filter(|t| t.len() >= 2)
        .map(|t| format!("%{}%", t.replace('%', "").replace('_', "\\_")))
        .collect();
    if tokens.is_empty() {
        return Ok(Vec::new());
    }
    let where_clause = tokens
        .iter()
        .enumerate()
        .map(|(i, _)| format!("e.content LIKE ?{} ESCAPE '\\'", i + 1))
        .collect::<Vec<_>>()
        .join(" AND ");
    let sql = format!(
        "SELECT e.episode_id, e.content FROM episodic_stream e
         WHERE {where_clause} AND (?{sess} IS NULL OR e.session_id = ?{sess})
         ORDER BY e.seq DESC LIMIT ?{lim}",
        sess = tokens.len() + 1,
        lim = tokens.len() + 2
    );
    let mut stmt = c.prepare(&sql)?;
    let mut params: Vec<Box<dyn rusqlite::ToSql>> = tokens
        .iter()
        .map(|t| Box::new(t.clone()) as Box<dyn rusqlite::ToSql>)
        .collect();
    params.push(Box::new(session.map(|s| s.to_string())));
    params.push(Box::new(limit.max(1)));
    let refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(|b| b.as_ref()).collect();

    let rows = stmt.query_map(refs.as_slice(), |r| {
        let id: String = r.get(0)?;
        let content: String = r.get(1)?;
        Ok((id, content))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (id, content) = row?;
        let snippet: String = content.chars().take(240).collect();
        out.push(LexicalHit {
            source_table: "episodic_stream".into(),
            source_id: id,
            score: 0.0,
            snippet,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_strips_operators() {
        assert_eq!(sanitize_match("foo AND bar"), "\"foo\"* AND \"AND\"* AND \"bar\"*");
        assert_eq!(sanitize_match("a\" OR \"b"), "\"a\"* AND \"OR\"* AND \"b\"*");
        // FTS5 keywords survive as *quoted* terms, which removes their operator
        // meaning: the query can no longer be a syntax error or a surprise.
        assert!(sanitize_match("NEAR(a b)").contains("\"NEAR\"*"));
        assert!(sanitize_match("   ***   ").is_empty());
        assert_eq!(sanitize_match("memory.pin"), "\"memory.pin\"*");
        assert_eq!(sanitize_match("cmd:rm -rf /"), "\"cmd\"* AND \"rm\"* AND \"rf\"*");
    }

    #[test]
    fn sanitize_caps_term_count() {
        let q = (0..40)
            .map(|i| format!("tok{i}"))
            .collect::<Vec<_>>()
            .join(" ");
        assert_eq!(sanitize_match(&q).matches(" AND ").count(), 11);
    }

    #[tokio::test]
    async fn fts5_is_available_and_finds_episodes() {
        let db = Db::open_in_memory().await.unwrap();
        assert!(db.has_fts5(), "bundled sqlite must ship FTS5");
        db.write(|tx| {
            for (i, body) in ["the cache coherence layer snaps boundaries", "unrelated chatter"]
                .iter()
                .enumerate()
            {
                tx.execute(
                    "INSERT INTO episodic_stream
                     (episode_id, seq, session_id, role, content, token_count, created_at)
                     VALUES (?1, ?2, 's1', 'user', ?3, 4, '2026-01-01T00:00:00Z')",
                    rusqlite::params![format!("e{i}"), i as i64 + 1, body],
                )?;
            }
            Ok(())
        })
        .await
        .unwrap();

        let hits = db.search_episodes("coherence bound", 5, None, false).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].source_id, "e0");
        assert!(hits[0].score > 0.0, "scores are normalised so larger is better");
    }
}
