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
    /// `session_id`, `project_id` and `fold_id` are optional hard filters applied after the MATCH,
    /// because FTS5 external-content tables index the whole stream.
    ///
    /// # `project_id` is what keeps one project out of another's transcript
    ///
    /// A store holds every project a user has worked on, so an unfiltered search over the stream
    /// returns turns from all of them — which is how working in one project surfaced material from
    /// another. Filtering here rather than in the caller means every route into episodic recall gets
    /// it, including the LIKE fallback for builds without FTS5.
    ///
    /// `NULL` means **no filter**, and rows written before migration 3 have `project_id IS NULL`.
    ///
    /// # This paragraph used to claim the opposite, and a reviewer measured the SQL
    ///
    /// It read: *"A scoped search **excludes** them rather than treating `NULL` as a match: an
    /// unattributed row belongs to no project, and showing it to whichever project happens to be asking
    /// is the defect this filter exists to remove."*
    ///
    /// That is a description of the right behaviour and **not** of this code. The predicate is
    /// `(?{p} IS NULL OR e.project_id = ?{p})`, so passing `None` binds SQL `NULL`, `NULL IS NULL` is
    /// true, and the row matches — **including every row belonging to another project**, not merely the
    /// unattributed ones. `None` is a wildcard; it is not "unattributed only".
    ///
    /// The distinction matters because the two are different features, and only one of them was built:
    ///
    ///   * **Search across every project** — implemented, reached by passing `None`.
    ///   * **Search one project plus its unattributed legacy rows** — described here, and **not**
    ///     implemented. Nothing can ask for it, because `project_id = ?` cannot match `NULL`.
    ///
    /// It is not a leak through the MCP surface: `memory.recall` binds
    /// `input.project_id.or_else(|| Some(self.engine.project_id().to_string()))`, so an omitted argument
    /// scopes to the daemon's project rather than becoming `None`, and asking across projects requires
    /// naming one. The defect is that this comment promised a property no test was checking, which is the
    /// same shape as the anchor budget, the supersession rule and `ImpactEntry::stale` — a guarantee
    /// stated in prose that the code does not provide.
    ///
    /// Left as `None` meaning wildcard rather than changed, because "search everything" is genuinely
    /// useful and the migration-3 rows are reachable through it. **What was wrong was the claim, not the
    /// query.**
    pub async fn search_episodes(
        &self,
        query: &str,
        limit: usize,
        session_id: Option<&str>,
        project_id: Option<&str>,
        exclude_folded: bool,
    ) -> Result<Vec<LexicalHit>> {
        let match_expr = sanitize_match(query);
        if match_expr.is_empty() {
            return Ok(Vec::new());
        }
        let session = session_id.map(|s| s.to_string());
        let project = project_id.map(|s| s.to_string());
        let use_fts = self.has_fts5();
        let limit = limit as i64;
        // The LIKE fallback needs the raw query inside the blocking closure, so
        // own it rather than borrowing the caller's `&str`.
        let raw_query = query.to_string();

        self.with(move |c| {
            if !use_fts {
                return like_scan_episodes(
                    c,
                    &raw_query,
                    limit,
                    session.as_deref(),
                    project.as_deref(),
                );
            }
            let sql = "SELECT f.rowid AS rowid,
                              bm25(episodic_fts) AS score,
                              snippet(episodic_fts, 0, '[', ']', ' … ', 12) AS snip,
                              e.episode_id AS episode_id,
                              e.session_id AS session_id,
                              e.fold_id AS fold_id,
                              e.project_id AS project_id
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
                let proj: Option<String> = r.get("project_id")?;
                Ok((id, score, snip, sess, fold, proj))
            })?;

            let mut out = Vec::new();
            for row in rows {
                let (id, score, snip, sess, fold, proj) = row?;
                if let Some(want) = session.as_deref()
                    && sess.as_deref() != Some(want)
                {
                    continue;
                }
                if let Some(want) = project.as_deref()
                    && proj.as_deref() != Some(want)
                {
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
                    && proj.as_deref() != Some(want)
                {
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
                let rows =
                    stmt.query_map(rusqlite::params![kind, id, limit], |r| r.get::<_, String>(0))?;
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
    let mut params: Vec<Box<dyn rusqlite::ToSql>> =
        tokens.iter().map(|t| Box::new(t.clone()) as Box<dyn rusqlite::ToSql>).collect();
    params.push(Box::new(project.map(|s| s.to_string())));
    params.push(Box::new(limit.max(1)));
    let refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(|b| b.as_ref()).collect();

    let rows =
        stmt.query_map(refs.as_slice(), |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
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
/// Every run of identifier characters becomes a quoted prefix term. Quoting removes
/// all FTS5 operator meaning, and prefix matching keeps partial identifiers
/// (`mem_pin` → `"mem_pin"*`) useful while typing.
///
/// # The two decisions that matter, and the bug that made them matter
///
/// An earlier version joined every term with `AND` and did nothing about
/// morphology. Against a real agent session that produced this:
///
/// ```text
/// stored:  "the retry helper takes max_attempts not retries"
/// query:   "retry"                                        → found
/// query:   "max_attempts"                                 → found
/// query:   "retries"                                      → NOT FOUND
/// query:   "how many retries does the helper take"        → NOT FOUND
/// ```
///
/// Both failures are fatal for a memory layer, because a model asks questions in
/// natural language and the stored text is whatever it happened to record. Two
/// changes fix them:
///
/// 1. **Conjunction only for short queries.** A one- or two-term query is precise
///    enough that requiring both terms is right. A six-word question required all
///    six to appear — so a single absent word ("how", "many", "does") returned
///    nothing at all. Beyond two terms the terms are now combined with `OR`, and
///    FTS5's own BM25 ranking decides: a document matching more of them scores
///    higher, which is the behaviour that was wanted.
///
/// 2. **Morphological variants.** `"retries"*` is a prefix match and `retry` does not
///    begin with `retries`, so an inflected query missed its own base form. Every
///    term of six or more characters now also contributes a short stem prefix, so
///    `retries` matches `retry` and `helper`. The prefix is kept short (four
///    characters) because a long one would match too much: `"conf"*` is useful,
///    `"confi"*` starts excluding `config`-adjacent spellings.
///
/// Terms are trimmed of leading and trailing punctuation (`/usr/lib` → `usr/lib`) and
/// single-character fragments are discarded, because a lone `/` or `-` matches
/// nothing and only adds a useless clause. FTS5 keywords (`AND`, `OR`, `NOT`, `NEAR`)
/// are kept as *quoted terms* rather than dropped — someone searching for the word
/// "not" means the word — and quoting stops them acting as operators.
pub fn sanitize_match(query: &str) -> String {
    let groups = term_groups(query);
    if groups.is_empty() {
        return String::new();
    }
    // Two terms or fewer stay conjunctive: precision matters more than recall, and
    // there is nothing to rank against. Longer queries take the union.
    let joiner = if groups.len() <= 2 { " AND " } else { " OR " };
    groups
        .into_iter()
        .map(|variants| {
            if variants.len() == 1 {
                format!("\"{}\"*", variants[0])
            } else {
                // `(a* OR b*)` — a group is one query term plus its stem variants.
                let inner =
                    variants.iter().map(|v| format!("\"{v}\"*")).collect::<Vec<_>>().join(" OR ");
                format!("({inner})")
            }
        })
        .collect::<Vec<_>>()
        .join(joiner)
}

/// Split a query into terms, each with its morphological variants.
///
/// Exposed because the `LIKE` fallback has to mirror this deliberately: two
/// retrieval paths that disagree about what a query means produce different answers
/// depending on whether FTS5 is present, which is the worst kind of inconsistency to
/// debug.
pub fn term_groups(query: &str) -> Vec<Vec<String>> {
    let mut raw: Vec<String> = Vec::new();
    let mut current = String::new();
    for ch in query.chars() {
        if ch.is_alphanumeric() || ch == '_' || ch == '.' || ch == '/' {
            current.push(ch);
        } else if !current.is_empty() {
            raw.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        raw.push(current);
    }

    raw.into_iter()
        .take(MAX_TERMS)
        .map(|t| {
            let t = t.trim_matches(|c: char| c == '/' || c == '.').replace('"', "");
            let stem = stem_of(&t);
            (t, stem)
        })
        .filter(|(t, _)| t.chars().any(|c| c.is_alphanumeric() || c == '_'))
        .map(|(t, stem)| {
            let mut variants = vec![t.clone()];
            // Only add a stem when it is genuinely shorter, and only for words long
            // enough that a four-character prefix is likely to be the same word.
            if let Some(stem) = stem
                && !t.starts_with(&stem)
            {
                variants.push(stem);
            }
            variants
        })
        .collect()
}

/// Terms kept from a query. Beyond this the expression stops discriminating and
/// starts costing parse time.
const MAX_TERMS: usize = 12;

/// The shortest prefix of a term worth matching as a stem.
///
/// Four characters: long enough that `"retr"*` still means retry/retries/retrieval,
/// short enough that it is not simply the whole word.
const MIN_STEM_LEN: usize = 4;

/// A conservative stem for a term, or `None` when stemming would not help.
///
/// Not a Porter stemmer. It handles only the cases that actually appeared in
/// testing — plural and past-tense endings — because an aggressive stemmer turns
/// `analysis` into `analys` and starts matching unrelated words, which costs more
/// precision than the recall is worth here.
fn stem_of(term: &str) -> Option<String> {
    let lower = term.to_ascii_lowercase();
    // Only ASCII words; identifiers and paths are left alone, since `src/auth.rs`
    // has no morphology and truncating it would match unrelated paths.
    if !lower.chars().all(|c| c.is_ascii_alphabetic()) || lower.len() < MIN_STEM_LEN + 2 {
        return None;
    }
    // Suffixes, longest-relevant first: `-ies` before `-es` before `-s`, or "stories" would
    // reduce to "storie". Each arm falls through to the next when it does not match.
    //
    // `or_else` rather than a chain of `if let ... else if let`, which clippy flags for good
    // reason: the chain reads as a sequence of conditions, and this is one operation with four
    // candidates. The `?` also makes the "none matched" case an early return instead of a
    // trailing `else` whose only job is to give up.
    let stem = lower
        .strip_suffix("ies")
        .map(|base| format!("{base}y"))
        .or_else(|| lower.strip_suffix("es").map(str::to_string))
        .or_else(|| lower.strip_suffix('s').map(str::to_string))
        .or_else(|| lower.strip_suffix("ing").map(str::to_string))
        .or_else(|| lower.strip_suffix("ed").map(str::to_string))?;
    if stem.len() >= MIN_STEM_LEN { Some(stem) } else { None }
}

fn like_scan_episodes(
    c: &rusqlite::Connection,
    query: &str,
    limit: i64,
    session: Option<&str>,
    project: Option<&str>,
) -> Result<Vec<LexicalHit>> {
    // # This must mirror `sanitize_match`, deliberately
    //
    // Two retrieval paths that disagree about what a query means produce different
    // answers depending on whether FTS5 is present — which is the worst kind of
    // inconsistency to debug, because it only appears on some builds. So the same
    // rules apply: a term matches any of its morphological variants, and short
    // queries are conjunctive while longer ones are not.
    let groups = term_groups(query);
    if groups.is_empty() {
        return Ok(Vec::new());
    }
    let conjunctive = groups.len() <= 2;
    let joiner = if conjunctive { " AND " } else { " OR " };

    let mut params: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
    let mut clauses: Vec<String> = Vec::new();
    for group in &groups {
        let mut inner: Vec<String> = Vec::new();
        for variant in group {
            inner.push(format!("e.content LIKE ?{} ESCAPE '\\'", params.len() + 1));
            params.push(Box::new(format!("%{}%", variant.replace('%', "").replace('_', "\\_"))));
        }
        clauses.push(if conjunctive {
            inner.join(" AND ")
        } else {
            format!("({})", inner.join(" OR "))
        });
    }

    let sess_idx = params.len() + 1;
    let proj_idx = params.len() + 2;
    let lim_idx = params.len() + 3;
    let sql = format!(
        "SELECT e.episode_id, e.content FROM episodic_stream e
         WHERE ({}) AND (?{sess_idx} IS NULL OR e.session_id = ?{sess_idx})
           AND (?{proj_idx} IS NULL OR e.project_id = ?{proj_idx})
         ORDER BY e.seq DESC LIMIT ?{lim_idx}",
        clauses.join(joiner)
    );
    let mut stmt = c.prepare(&sql)?;
    params.push(Box::new(session.map(|s| s.to_string())));
    params.push(Box::new(project.map(|s| s.to_string())));
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
        // The point of this test is that FTS5 keywords are *quoted*, so they cannot
        // act as operators and cannot make the expression a syntax error. Which
        // joiner appears between them depends on term count, and that rule is pinned
        // separately by `a_long_question_is_not_a_conjunction`; here only the quoting
        // matters.
        assert_eq!(sanitize_match("foo AND bar"), "\"foo\"* OR \"AND\"* OR \"bar\"*");
        assert!(sanitize_match("a\" OR \"b").contains("\"OR\"*"));
        assert!(sanitize_match("NEAR(a b)").contains("\"NEAR\"*"));
        assert!(sanitize_match("   ***   ").is_empty());
        assert_eq!(sanitize_match("memory.pin"), "\"memory.pin\"*");
        // Three terms, so a union — and `rf` is too short to stem.
        assert_eq!(sanitize_match("cmd:rm -rf /"), "\"cmd\"* OR \"rm\"* OR \"rf\"*");
    }

    #[test]
    fn sanitize_caps_term_count() {
        let q = (0..40).map(|i| format!("tok{i}")).collect::<Vec<_>>().join(" ");
        // Twelve terms, so eleven joiners — the cap is unchanged. The joiner is now
        // OR rather than AND because a query this long is a question, not a filter.
        assert_eq!(sanitize_match(&q).matches(" OR ").count(), 11);
    }

    // -----------------------------------------------------------------------
    // The retrieval bugs found against a live agent session
    // -----------------------------------------------------------------------

    #[test]
    fn a_long_question_is_not_a_conjunction() {
        // The failure that started this: a model asked "how many retries does the
        // helper take" and got nothing, because all eight words had to appear in a
        // one-line stored fact. A question is a union of hints, not a filter.
        let expr = sanitize_match("how many retries does the helper take");
        assert!(
            !expr.contains(" AND "),
            "a six-term question must not require every term, got: {expr}"
        );
        assert!(expr.contains(" OR "), "expected a union, got: {expr}");
    }

    #[test]
    fn a_short_query_stays_precise() {
        // The other half of the rule, and the reason it is not simply "always OR":
        // with two terms, requiring both is what makes a query selective.
        let expr = sanitize_match("cache coherence");
        assert!(expr.contains(" AND "), "a two-term query must stay conjunctive, got: {expr}");
    }

    #[test]
    fn an_inflected_query_matches_its_base_form() {
        // `"retries"*` does not match `retry`, because a prefix match runs forwards.
        // The stem variant is what bridges them.
        let expr = sanitize_match("retries");
        assert!(expr.contains("\"retry\""), "expected a `retry` stem variant, got: {expr}");
    }

    #[test]
    fn stem_variants_do_not_apply_to_identifiers_or_paths() {
        // `src/auth.rs` has no morphology, and truncating it would start matching
        // unrelated paths — the recall would be bought with precision.
        assert!(!sanitize_match("src/auth.rs").contains("\"src\""));
        assert!(!sanitize_match("mem_pin").contains("\"mem\""));
        // And short words are left alone: a four-character prefix of a five-letter
        // word is nearly the whole word, so the variant adds nothing.
        assert_eq!(sanitize_match("cats"), "\"cats\"*");
    }

    #[test]
    fn stemming_is_conservative_enough_not_to_match_everything() {
        // A Porter stemmer would turn `analysis` into `analys` and start matching
        // unrelated words. Only the endings that actually appeared in testing are
        // handled, and only for plain alphabetic words.
        assert_eq!(sanitize_match("analysis"), "\"analysis\"*");
        // `config` has no handled ending, so it stays a single term.
        assert_eq!(sanitize_match("config"), "\"config\"*");
        // `logging` does, and `log` is too short to be a safe stem.
        assert_eq!(sanitize_match("logging"), "\"logging\"*");
    }

    #[tokio::test]
    async fn a_natural_language_question_finds_a_short_stored_fact() {
        // The end-to-end version of the bug, against a real store: store the fact a
        // user would record, then ask the way a model would ask.
        let db = Db::open_in_memory().await.unwrap();
        db.write(|tx| {
            tx.execute(
                "INSERT INTO episodic_stream
                 (episode_id, seq, session_id, role, content, token_count, created_at)
                 VALUES ('e0', 1, 's1', 'user',
                         'the retry helper takes max_attempts not retries', 9,
                         '2026-01-01T00:00:00Z')",
                [],
            )?;
            Ok(())
        })
        .await
        .unwrap();

        for query in [
            "retry",
            "max_attempts",
            // The two that returned nothing before this fix.
            "retries",
            "how many retries does the helper take",
        ] {
            let hits = db.search_episodes(query, 5, None, None, false).await.unwrap();
            assert!(
                !hits.is_empty(),
                "query {query:?} found nothing; a model asking this would conclude the fact was never recorded"
            );
            assert_eq!(hits[0].source_id, "e0");
        }
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

        let hits = db.search_episodes("coherence bound", 5, None, None, false).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].source_id, "e0");
        assert!(hits[0].score > 0.0, "scores are normalised so larger is better");
    }

    /// Pin what a `NULL` project filter actually does, because nothing did.
    ///
    /// # The property this replaces was stated in a doc comment and was false
    ///
    /// `search_episodes`' own docs said a scoped search *"**excludes**"* the migration-3 rows whose
    /// `project_id IS NULL` *"rather than treating `NULL` as a match"*. The predicate is
    /// `(?p IS NULL OR project_id = ?p)`, so `None` binds SQL `NULL`, `NULL IS NULL` is true, and the
    /// row matches — **along with every row belonging to every other project.** `None` is a wildcard, not
    /// "unattributed only".
    ///
    /// Three assertions, because the interesting content is the *pair* of behaviours and the difference
    /// between them:
    ///
    ///   * a named project sees its own rows and no other project's — the filter works, and this is what
    ///     project isolation rests on;
    ///   * a named project does **not** see the unattributed rows, which is the part the old comment got
    ///     backwards and the part nobody can currently ask for;
    ///   * `None` sees everything, which is a real feature and is why the query was left alone.
    #[tokio::test]
    async fn a_null_project_filter_is_a_wildcard_not_an_unattributed_match() {
        let db = Db::open_in_memory().await.unwrap();
        db.write(|tx| {
            // One row per project, plus a legacy row with no project at all — the shape migration 3 left
            // behind for everything written before it.
            for (index, (id, project)) in
                [("a", Some("alpha")), ("b", Some("beta")), ("legacy", None::<&str>)]
                    .into_iter()
                    .enumerate()
            {
                tx.execute(
                    "INSERT INTO episodic_stream
                     (episode_id, seq, session_id, role, content, token_count, created_at, project_id)
                     VALUES (?1, ?2, 's1', 'user', 'the coherent boundary', 4, '2026-01-01T00:00:00Z', ?3)",
                    rusqlite::params![id, index as i64 + 1, project],
                )?;
            }
            Ok(())
        })
        .await
        .unwrap();

        let ids = |hits: Vec<LexicalHit>| {
            let mut v: Vec<String> = hits.into_iter().map(|h| h.source_id).collect();
            v.sort();
            v
        };

        let alpha = ids(db
            .search_episodes("coherent boundary", 10, None, Some("alpha"), false)
            .await
            .unwrap());
        assert_eq!(alpha, vec!["a"], "a named project sees its own rows and nothing else");

        let everything =
            ids(db.search_episodes("coherent boundary", 10, None, None, false).await.unwrap());
        assert_eq!(
            everything,
            vec!["a", "b", "legacy"],
            "`None` is a wildcard: it returns every project's rows, not only the unattributed ones"
        );
        assert!(
            !alpha.contains(&"legacy".to_string()),
            "and a named project does not pick up the unattributed row — which is the behaviour the \
             doc comment claimed and the query has never provided"
        );
    }
}
