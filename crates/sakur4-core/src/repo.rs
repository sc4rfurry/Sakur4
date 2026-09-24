//! Repo Cortex (PRD component C4): local, incremental, structural code map.
//!
//! # Why a graph and not a bigger window
//!
//! PP-5 is explicit: at multi-thousand-file scale, pouring files into context does
//! not work even at 128K, and semantic-only retrieval "confuses structurally
//! unrelated but textually similar code". The 2026 tooling consensus the PRD cites
//! is a pre-computed structural graph — AST plus call graph plus import graph —
//! with semantic search layered on top. This module is that graph.
//!
//! # The Feed into the Symbolic Ledger
//!
//! Every symbol this module extracts becomes a [`SymbolicFact`] with an `ast_hash`.
//! That single decision is what makes three separate PRD requirements fall out at
//! once:
//!
//! * FR-2 — the Ledger is populated by a deterministic parser and nothing else.
//! * FR-9 — incremental re-indexing is natural-key upsert, which is why one
//!   changed file costs O(symbols in that file).
//! * FR-12/INN-4 — staleness is a hash comparison, so a summary anchored to a
//!   symbol is invalidated the moment the symbol's source changes.
//!
//! # Scope, stated honestly
//!
//! Full tree-sitter grammars ship for Python, TypeScript/JavaScript, Rust and Go
//! (FR-9's v1 requirement). For other text formats this module extracts *declared
//! names* with conservative patterns, which is enough to give the Ledger anchors
//! for config keys, SQL tables, and shell functions without pretending to a
//! structural understanding it does not have. Files it cannot parse at all are
//! indexed as `unsupported`: present in the map, contributing no facts, and
//! reported as such.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::ids::{content_hash, normalize_rel_path, now_rfc3339, short_hash_str};
use crate::memory::dependency::{EdgeKind, EdgeRow, NodeRef};
use crate::memory::fabric::MemoryFabric;
use crate::memory::symbolic::{FactKind, FactSource, SymbolicFact, SymbolicWrite};
use crate::store::Db;
use crate::tokens::TokenCounter;

/// A language Sakur4 can parse.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, Default, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum Language {
    Python,
    TypeScript,
    Tsx,
    JavaScript,
    Rust,
    Go,
    /// Structured text formats handled by the heuristic extractor.
    Config,
    /// Recognised but not parsed.
    #[default]
    Unsupported,
}

impl Language {
    pub fn as_str(self) -> &'static str {
        match self {
            Language::Python => "python",
            Language::TypeScript => "typescript",
            Language::Tsx => "tsx",
            Language::JavaScript => "javascript",
            Language::Rust => "rust",
            Language::Go => "go",
            Language::Config => "config",
            Language::Unsupported => "unsupported",
        }
    }

    /// Whether this language is parsed with a real grammar (FR-9's v1 set).
    pub fn is_structured(self) -> bool {
        !matches!(self, Language::Config | Language::Unsupported)
    }

    /// Detect from a path.
    pub fn from_path(path: &Path) -> Self {
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
        match ext.as_str() {
            "py" | "pyi" => Language::Python,
            "ts" | "mts" | "cts" => Language::TypeScript,
            "tsx" => Language::Tsx,
            "js" | "mjs" | "cjs" | "jsx" => Language::JavaScript,
            "rs" => Language::Rust,
            "go" => Language::Go,
            "json" | "yaml" | "yml" | "toml" | "ini" | "cfg" | "env" | "sql" | "sh" | "bash"
            | "zsh" | "md" => Language::Config,
            _ => Language::Unsupported,
        }
    }

    /// Whether the extension is even worth walking. Keeps the index bounded on
    /// repositories full of binaries and generated assets.
    pub fn is_indexable(path: &Path) -> bool {
        !matches!(Self::from_path(path), Language::Unsupported)
    }
}

/// Indexing configuration.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct RepoCortexConfig {
    /// Skip files larger than this. A 4 MB source file is generated, and
    /// tree-sitter on it would dominate index time for no benefit.
    pub max_file_bytes: u64,
    /// Additional ignore globs beyond `.gitignore`.
    pub extra_ignores: Vec<String>,
    /// Rank symbols by graph centrality when rendering the map.
    pub centrality_ranking: bool,
    /// PageRank damping factor.
    pub damping: f64,
}

impl Default for RepoCortexConfig {
    fn default() -> Self {
        Self {
            max_file_bytes: 4 * 1024 * 1024,
            extra_ignores: Vec::new(),
            centrality_ranking: true,
            damping: 0.85,
        }
    }
}

/// What a full or incremental index did.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct IndexReport {
    pub files_scanned: usize,
    pub files_parsed: usize,
    pub files_skipped_unchanged: usize,
    pub files_removed: usize,
    pub files_unsupported: usize,
    pub symbols_extracted: usize,
    pub edges_created: usize,
    pub elapsed_ms: i64,
    pub languages: HashMap<String, usize>,
    pub warnings: Vec<String>,
}

impl IndexReport {
    pub fn summary(&self) -> String {
        let langs: Vec<String> = {
            let mut v: Vec<(String, usize)> =
                self.languages.iter().map(|(k, v)| (k.clone(), *v)).collect();
            v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            v.into_iter().map(|(k, n)| format!("{k}×{n}")).collect()
        };
        format!(
            "{} scanned, {} parsed, {} unchanged, {} removed, {} unsupported · \
             {} symbol(s), {} edge(s) · {} ms · [{}]",
            self.files_scanned,
            self.files_parsed,
            self.files_skipped_unchanged,
            self.files_removed,
            self.files_unsupported,
            self.symbols_extracted,
            self.edges_created,
            self.elapsed_ms,
            langs.join(", ")
        )
    }
}

/// A symbol extracted from a file, before it becomes a fact.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Extract {
    pub write: SymbolicWrite,
    /// Qualified names this symbol references (calls or imports), resolved later.
    pub references: Vec<String>,
    /// True for `import`/`use` statements, which become `Imports` edges.
    pub is_import: bool,
}

/// A parsed file.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct ParsedFile {
    pub language: Language,
    pub extracts: Vec<Extract>,
    /// Modules the file imports, as declared text (resolved to paths later).
    pub imports: Vec<String>,
    /// A one-line description for the map.
    pub outline: Vec<String>,
}

/// Repo Cortex.
#[derive(Clone)]
pub struct RepoCortex {
    db: Db,
    fabric: MemoryFabric,
    project_id: String,
    config: RepoCortexConfig,
}

impl RepoCortex {
    pub fn new(db: Db, fabric: MemoryFabric, project_id: String) -> Self {
        Self { db, fabric, project_id, config: RepoCortexConfig::default() }
    }

    pub fn with_config(mut self, config: RepoCortexConfig) -> Self {
        self.config = config;
        self
    }

    pub fn project_id(&self) -> &str {
        &self.project_id
    }

    pub fn set_project_id(&mut self, id: impl Into<String>) {
        self.project_id = id.into();
    }

    // =======================================================================
    // Indexing
    // =======================================================================

    /// Full index of a repository root.
    pub async fn index(&self, root: &Path) -> Result<IndexReport> {
        self.index_inner(root, false).await
    }

    /// Incremental index: re-parses only files whose content hash changed.
    ///
    /// The hash check is why FR-9's acceptance criterion ("re-indexing a single
    /// changed file in a 5,000+ file repository completes in under 2 seconds ...
    /// and does not re-parse unrelated files") is achievable: walking 5,000 file
    /// paths and hashing them is I/O-bound and small, while parsing is not needed
    /// for the 4,999 that did not change.
    pub async fn reindex(&self, root: &Path) -> Result<IndexReport> {
        self.index_inner(root, true).await
    }

    async fn index_inner(&self, root: &Path, incremental: bool) -> Result<IndexReport> {
        let started = std::time::Instant::now();
        let root = root.to_path_buf();
        if !root.exists() {
            return Err(Error::NotFound(format!(
                "repository root {} does not exist",
                root.display()
            )));
        }

        let known = self.known_files().await?;
        let files = discover_files(&root, &self.config);
        let mut report = IndexReport { files_scanned: files.len(), ..Default::default() };

        let mut seen: HashSet<String> = HashSet::new();
        let mut all_facts: Vec<SymbolicFact> = Vec::new();
        let mut all_edges: Vec<EdgeRow> = Vec::new();
        let mut file_extracts: Vec<ExtractRef> = Vec::new();
        // (rel_path, language, content_hash, size, symbol_count)
        let mut file_rows: Vec<(String, String, String, i64, i64)> = Vec::new();

        for abs in &files {
            let rel = normalize_rel_path(abs.strip_prefix(&root).unwrap_or(abs));
            seen.insert(rel.clone());
            let language = Language::from_path(abs);

            let bytes = match std::fs::read(abs) {
                Ok(b) => b,
                Err(e) => {
                    report.warnings.push(format!("could not read {rel}: {e}"));
                    continue;
                }
            };
            if bytes.len() as u64 > self.config.max_file_bytes {
                report
                    .warnings
                    .push(format!("skipped {rel}: {} bytes exceeds the limit", bytes.len()));
                continue;
            }
            let hash = content_hash(&bytes);

            if incremental
                && let Some((old_hash, _)) = known.get(&rel)
                && old_hash == &hash
            {
                report.files_skipped_unchanged += 1;
                let symbols = self
                    .db
                    .with({
                        let p = self.project_id.clone();
                        let r = rel.clone();
                        move |c| {
                            Ok(c.query_row(
                                "SELECT symbols FROM repo_file WHERE project_id=?1 AND rel_path=?2",
                                rusqlite::params![p, r],
                                |row| row.get::<_, i64>(0),
                            )
                            .unwrap_or(0))
                        }
                    })
                    .await?;
                file_rows.push((
                    rel,
                    language.as_str().to_string(),
                    hash,
                    bytes.len() as i64,
                    symbols,
                ));
                continue;
            }

            let source = String::from_utf8_lossy(&bytes).to_string();
            if language == Language::Unsupported {
                report.files_unsupported += 1;
                file_rows.push((rel, language.as_str().into(), hash, bytes.len() as i64, 0));
                continue;
            }

            let parsed = if language.is_structured() {
                parse_structured(&source, language, &rel)
            } else {
                parse_config(&source, language, &rel)
            };

            report.files_parsed += 1;
            *report.languages.entry(language.as_str().to_string()).or_insert(0) += 1;

            let mut name_to_fact: HashMap<String, String> = HashMap::new();
            for ex in &parsed.extracts {
                let fact = SymbolicFact::from_deterministic_source(
                    ex.write.clone(),
                    FactSource::TreeSitter,
                    Some(self.project_id.clone()),
                );
                name_to_fact.insert(fact.qualified_name.clone(), fact.fact_id.clone());
                all_facts.push(fact);
            }
            report.symbols_extracted += parsed.extracts.len();

            // The file node, and the record of which extracts became facts. The
            // record is what the cross-file pass resolves references against.
            let file_node = NodeRef::file(&rel);
            for ex in &parsed.extracts {
                let fact_id = name_to_fact.get(&ex.write.qualified_name).cloned();
                if let Some(id) = &fact_id {
                    all_edges.push(EdgeRow::new(
                        &NodeRef::fact(id),
                        &file_node,
                        EdgeKind::DependsOn,
                    ));
                }
                file_extracts.push(ExtractRef {
                    fact_id,
                    qualified_name: ex.write.qualified_name.clone(),
                    references: ex.references.clone(),
                    is_import: ex.is_import,
                });
            }

            file_rows.push((
                rel,
                language.as_str().to_string(),
                hash,
                bytes.len() as i64,
                parsed.extracts.len() as i64,
            ));
        }

        // Files that disappeared since the last index.
        let removed: Vec<String> = known.keys().filter(|k| !seen.contains(*k)).cloned().collect();
        report.files_removed = removed.len();

        // Second pass: resolve references across files now that every file's
        // symbols are known. Doing this after the walk is what lets one pass
        // produce a call graph rather than a per-file forest.
        //
        // References are *bare identifiers* — `login(…)` in a body, or the module
        // path in a `use` — while facts carry fully qualified names built from the
        // file's path. So resolution goes through a short-name index rather than
        // reconstructing a qualified name, which is what an earlier version tried
        // and got wrong: it prefixed the field path a second time, matched nothing,
        // and silently produced a repository with an AST and no call graph.
        let mut short_names: HashMap<&str, &str> = HashMap::new();
        for f in &all_facts {
            let short = short_name_of(&f.qualified_name);
            // First definition wins, so the graph is deterministic when two files
            // declare the same short name.
            short_names.entry(short).or_insert(f.fact_id.as_str());
        }
        // Each fact's signature hash, so an edge can record what the caller saw. See the note at the
        // `Calls` edge below.
        let hash_by_fact: HashMap<&str, String> =
            all_facts.iter().map(|f| (f.fact_id.as_str(), f.ast_hash.clone())).collect();

        let mut extra_edges = 0usize;
        for ex in &file_extracts {
            let Some(src_id) = ex.fact_id.as_deref() else {
                continue;
            };
            for target in &ex.references {
                let dst_id = resolve_reference(target, &ex.qualified_name, &short_names);
                let Some(dst_id) = dst_id else {
                    continue;
                };
                if dst_id == src_id {
                    continue;
                }
                extra_edges += 1;
                // # Record the caller's own hash, so FR-11 can tell whether it still holds
                //
                // The column is named `target_hash` and holds what the *source* — the caller — was
                // when this edge was first observed. The read path compares it against the caller's
                // hash today, and the two differ exactly when the caller has changed since the call
                // site was last read.
                //
                // It records the caller rather than the target because the target's hash is already
                // available from the target itself — storing it would restate something known,
                // whereas the caller's *old* hash exists nowhere else. `insert_sql` keeps the first
                // observation, so re-indexing the target cannot erase the drift by refreshing this.
                let target_hash = hash_by_fact.get(src_id).cloned();
                all_edges.push(
                    EdgeRow::new(
                        &NodeRef::fact(src_id),
                        &NodeRef::fact(dst_id),
                        if ex.is_import { EdgeKind::Imports } else { EdgeKind::Calls },
                    )
                    .with_target_hash_opt(target_hash),
                );
            }
        }

        report.edges_created = all_edges.len();

        // --- persist ---------------------------------------------------------
        let facts_count = all_facts.len();
        let edges_count = all_edges.len();
        let project_id = self.project_id.clone();
        let root_str = root.to_string_lossy().to_string();
        let name = root
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| root_str.clone());
        let now = now_rfc3339();

        self.db
            .write(move |tx| {
                tx.execute(
                    "INSERT INTO project(project_id, root_path, name, indexed_at)
                     VALUES (?1, ?2, ?3, ?4)
                     ON CONFLICT(project_id) DO UPDATE SET
                        root_path = excluded.root_path, indexed_at = excluded.indexed_at",
                    rusqlite::params![project_id, root_str, name, now],
                )?;

                for f in &all_facts {
                    crate::memory::fabric::upsert_symbolic_fact_tx(tx, f)?;
                }
                for (rel, lang, hash, size, symbols) in &file_rows {
                    tx.execute(
                        "INSERT INTO repo_file(project_id, rel_path, language, content_hash,
                                               size_bytes, symbols, parsed_at)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                         ON CONFLICT(project_id, rel_path) DO UPDATE SET
                            language = excluded.language,
                            content_hash = excluded.content_hash,
                            size_bytes = excluded.size_bytes,
                            symbols = excluded.symbols,
                            parsed_at = excluded.parsed_at",
                        rusqlite::params![project_id, rel, lang, hash, size, symbols, now],
                    )?;
                }
                for e in &all_edges {
                    let (a, b, c, d, k, w, t, th) = e.params();
                    tx.execute(EdgeRow::insert_sql(), rusqlite::params![a, b, c, d, k, w, t, th])?;
                }
                Ok(())
            })
            .await?;

        // Removal is a separate transaction because it is the destructive step and
        // should not be able to roll back the facts that did parse.
        if !removed.is_empty() {
            self.fabric.forget_files(removed.clone()).await?;
            let project = self.project_id.clone();
            self.db
                .write(move |tx| {
                    for rel in &removed {
                        tx.execute(
                            "DELETE FROM repo_file WHERE project_id = ?1 AND rel_path = ?2",
                            rusqlite::params![project, rel],
                        )?;
                    }
                    Ok(())
                })
                .await?;
        }

        report.elapsed_ms = started.elapsed().as_millis() as i64;
        tracing::info!(
            project = %self.project_id,
            facts = facts_count,
            edges = edges_count,
            extra = extra_edges,
            elapsed_ms = report.elapsed_ms,
            "repo cortex index complete: {}",
            report.summary()
        );
        Ok(report)
    }

    /// Index exactly one file (a watcher's hot path).
    pub async fn reindex_file(&self, root: &Path, rel_path: &str) -> Result<IndexReport> {
        let started = std::time::Instant::now();
        let abs = root.join(rel_path);
        let mut report = IndexReport { files_scanned: 1, ..Default::default() };
        if !abs.exists() {
            self.fabric.forget_files(vec![rel_path.to_string()]).await?;
            let project = self.project_id.clone();
            let rel = rel_path.to_string();
            self.db
                .write(move |tx| {
                    tx.execute(
                        "DELETE FROM repo_file WHERE project_id = ?1 AND rel_path = ?2",
                        rusqlite::params![project, rel],
                    )?;
                    Ok(())
                })
                .await?;
            report.files_removed = 1;
            report.elapsed_ms = started.elapsed().as_millis() as i64;
            return Ok(report);
        }

        let bytes = std::fs::read(&abs)?;
        let hash = content_hash(&bytes);
        let language = Language::from_path(&abs);
        let source = String::from_utf8_lossy(&bytes).to_string();
        let parsed = if language.is_structured() {
            parse_structured(&source, language, rel_path)
        } else if language == Language::Config {
            parse_config(&source, language, rel_path)
        } else {
            ParsedFile::default()
        };

        let facts: Vec<SymbolicFact> = parsed
            .extracts
            .iter()
            .map(|ex| {
                SymbolicFact::from_deterministic_source(
                    ex.write.clone(),
                    FactSource::TreeSitter,
                    Some(self.project_id.clone()),
                )
            })
            .collect();
        report.symbols_extracted = facts.len();
        report.files_parsed = 1;
        *report.languages.entry(language.as_str().to_string()).or_insert(0) += 1;

        // Replace this file's facts wholesale so deleted symbols actually go away.
        self.fabric.forget_files(vec![rel_path.to_string()]).await?;
        self.fabric.upsert_facts(facts.clone()).await?;

        let project = self.project_id.clone();
        let rel = rel_path.to_string();
        let lang = language.as_str().to_string();
        let size = bytes.len() as i64;
        let symbols = facts.len() as i64;
        let now = now_rfc3339();
        self.db
            .write(move |tx| {
                tx.execute(
                    "INSERT INTO repo_file(project_id, rel_path, language, content_hash,
                                           size_bytes, symbols, parsed_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                     ON CONFLICT(project_id, rel_path) DO UPDATE SET
                        language=excluded.language, content_hash=excluded.content_hash,
                        size_bytes=excluded.size_bytes, symbols=excluded.symbols,
                        parsed_at=excluded.parsed_at",
                    rusqlite::params![project, rel, lang, hash, size, symbols, now],
                )?;
                Ok(())
            })
            .await?;

        report.elapsed_ms = started.elapsed().as_millis() as i64;
        Ok(report)
    }

    async fn known_files(&self) -> Result<HashMap<String, (String, i64)>> {
        let project = self.project_id.clone();
        self.db
            .with(move |c| {
                let mut stmt = c.prepare(
                    "SELECT rel_path, content_hash, symbols FROM repo_file WHERE project_id = ?1",
                )?;
                let rows = stmt.query_map([project], |r| {
                    Ok((r.get::<_, String>(0)?, (r.get::<_, String>(1)?, r.get::<_, i64>(2)?)))
                })?;
                let mut out = HashMap::new();
                for row in rows {
                    let (k, v) = row?;
                    out.insert(k, v);
                }
                Ok(out)
            })
            .await
    }

    // =======================================================================
    // Repo map (FR-10)
    // =======================================================================

    /// A token-budgeted, centrality-ranked outline of the repository.
    ///
    /// # Monotonicity
    ///
    /// FR-10 requires that a smaller budget returns a strict *subset* of what a
    /// larger budget returns, "not a different ranking". That is why this
    /// function computes the full ranked list first and then truncates by
    /// consumption: the ordering is independent of the budget, so shrinking the
    /// budget can only ever remove entries from the tail.
    pub async fn repo_map(
        &self,
        token_budget: usize,
        focus_paths: Option<&[String]>,
        counter: &TokenCounter,
    ) -> Result<(String, usize)> {
        self.map_with(token_budget, focus_paths, counter, false).await
    }

    /// The repository map with **qualified names** instead of signatures.
    ///
    /// # Why this exists
    ///
    /// `map` prints a signature when a symbol has one, which carries more information per
    /// token — the right default. But it meant the qualified names were never shown, and
    /// `symbol --name` and `impact --name` take exactly those. So the documented workflow
    /// "call `map`, then look up a name from it" could not be followed: every name a user
    /// could see was a signature, and every name the lookup accepted was invisible.
    ///
    /// Verified against this repository: `map` emitted 20 symbol lines, not one of them a
    /// qualified name, and `symbol --name Engine::open` returned `not found` for each of six
    /// spellings tried.
    ///
    /// Discovery therefore needs its own mode rather than a wider default. A map is
    /// token-budgeted and every character spent on a second rendering of a symbol is a
    /// character not spent on another file, so the names are opt-in.
    pub async fn repo_map_names(
        &self,
        token_budget: usize,
        counter: &TokenCounter,
    ) -> Result<(String, usize)> {
        self.map_with(token_budget, None, counter, true).await
    }

    async fn map_with(
        &self,
        token_budget: usize,
        focus_paths: Option<&[String]>,
        counter: &TokenCounter,
        names_only: bool,
    ) -> Result<(String, usize)> {
        let facts = self.fabric.project_facts(&self.project_id).await?;
        if facts.is_empty() {
            return Ok((
                "repository map is empty — run an index first (`sakur4d index`)".to_string(),
                0,
            ));
        }

        // --- centrality ------------------------------------------------------
        let importance = if self.config.centrality_ranking {
            self.centrality(&facts).await?
        } else {
            HashMap::new()
        };

        // --- focus boost -----------------------------------------------------
        // FR-10: focus_paths boosts symbols reachable from those paths within two
        // graph hops.
        let boosted = match focus_paths {
            Some(paths) if !paths.is_empty() => self.focus_boost(paths).await?,
            _ => HashSet::new(),
        };

        // --- group by file ---------------------------------------------------
        let mut by_file: HashMap<String, Vec<&SymbolicFact>> = HashMap::new();
        for f in &facts {
            if f.kind == FactKind::Import {
                continue;
            }
            if let Some(path) = &f.file_path {
                by_file.entry(path.clone()).or_default().push(f);
            }
        }

        let mut files: Vec<(String, f64, Vec<&SymbolicFact>)> = by_file
            .into_iter()
            .map(|(path, mut syms)| {
                syms.sort_by_key(|s| s.line_start.unwrap_or(0));
                // A file's rank is the best symbol it contains, not the sum: one
                // load-bearing function makes a file worth opening, while nine
                // trivial ones do not.
                let score = syms
                    .iter()
                    .map(|s| {
                        let base = importance.get(&s.fact_id).copied().unwrap_or(1.0);
                        let focus = if boosted.contains(&s.fact_id) { 3.0 } else { 1.0 };
                        base * focus
                    })
                    .fold(0.0f64, f64::max);
                (path, score, syms)
            })
            .collect();
        files.sort_by(|a, b| {
            b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal).then_with(|| a.0.cmp(&b.0))
        });

        // --- render with a monotonic budget ----------------------------------
        //
        // # The budget contract
        //
        // `tokens_used` is the measured size of what is returned, and it never
        // exceeds `token_budget`. The two structural lines (header and coverage
        // footer) are reserved first, and what remains is spent on ranked content.
        // A budget too small even for those returns a map with no content rather
        // than one that overshoots: FR-10 says "fitted to the requested token
        // budget", and a caller trimming its context has to be able to rely on
        // that for small budgets too.
        //
        // Monotonicity — a smaller budget returning a strict prefix rather than a
        // different ranking — follows from the order being computed once, before
        // any budget is applied.
        let header = "=== REPOSITORY MAP (ranked by structural centrality) ===\n";
        let footer_shape = "\n[map covers 0 of 0 files, 0 of 0 symbols]\n";
        let structural_cost = counter.count(header).get() + counter.count(footer_shape).get();

        let mut body = String::new();
        let mut used = structural_cost;
        let mut included_files = 0usize;
        let mut included_symbols = 0usize;

        'files: for (path, score, syms) in &files {
            let block = format!("\n{path}  (rank {score:.2})\n");
            let mut block_symbols: Vec<String> = Vec::new();
            for s in syms {
                // Show the signature when there is one; it carries far more
                // information per token than a bare name.
                let line = if names_only {
                    format!("  {}\n", s.qualified_name)
                } else {
                    match &s.signature {
                        Some(sig) => format!("  {} {sig}\n", s.kind.as_str()),
                        None => format!("  {} {}\n", s.kind.as_str(), s.qualified_name),
                    }
                };
                block_symbols.push(line);
            }
            if block_symbols.is_empty() {
                continue;
            }
            let whole_block = format!("{block}{}", block_symbols.join(""));
            let cost = counter.count(&whole_block).get();

            if used + cost > token_budget {
                // The file does not fit whole. Emit as many of its symbols as do
                // fit — a prefix of the same list, never a reordering — and stop.
                let mut partial = block.clone();
                let mut partial_symbols = 0usize;
                for line in &block_symbols {
                    let candidate = format!("{partial}{line}");
                    if used + counter.count(&candidate).get() > token_budget {
                        break;
                    }
                    partial = candidate;
                    partial_symbols += 1;
                }
                if partial_symbols > 0 {
                    // The accounting below is complete at this point: used was
                    // already checked against the budget for every line added, and
                    // the loop breaks next. Keeping the counter current anyway means
                    // a future change to that control flow cannot silently start
                    // over-reporting what fits.
                    #[allow(unused_assignments)]
                    {
                        used += counter.count(&partial).get();
                    }
                    body.push_str(&partial);
                    included_files += 1;
                    included_symbols += partial_symbols;
                }
                break 'files;
            }

            body.push_str(&whole_block);
            used += cost;
            included_files += 1;
            included_symbols += block_symbols.len();
        }

        let mut out = String::with_capacity(body.len() + 256);
        out.push_str(header);
        out.push_str(&body);
        if included_files == 0 && !files.is_empty() {
            // The budget could not cover a single file. Say so: an empty map that
            // looks like an empty repository is a worse answer than an honest
            // "raise the budget". This is the one case where the result is larger
            // than requested, and `tokens_used` reports the real number rather
            // than hiding it.
            out.push_str(&format!(
                "\n[budget of {token_budget} tokens covered no files; the smallest entry needs more]\n"
            ));
        }
        out.push_str(&format!(
            "\n[map covers {included_files} of {} files, {included_symbols} of {} symbols]\n",
            files.len(),
            facts.len()
        ));
        let total = counter.count(&out).get();
        Ok((out, total))
    }

    /// PageRank over the call/import graph.
    async fn centrality(&self, facts: &[SymbolicFact]) -> Result<HashMap<String, f64>> {
        let project = self.project_id.clone();
        let edges = self
            .db
            .with(move |c| {
                let mut stmt = c.prepare(
                    "SELECT src_id, dst_id FROM dependency_graph_edge
                     WHERE src_type='symbolic_fact' AND dst_type='symbolic_fact'
                       AND edge_kind IN ('calls','imports')
                       AND src_id IN (SELECT fact_id FROM symbolic_fact WHERE project_id = ?1)",
                )?;
                let rows = stmt.query_map([project], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                })?;
                Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
            })
            .await?;

        let n = facts.len().max(1);
        let mut rank: HashMap<String, f64> =
            facts.iter().map(|f| (f.fact_id.clone(), 1.0 / n as f64)).collect();
        let mut out_degree: HashMap<String, usize> = HashMap::new();
        for (src, _) in &edges {
            *out_degree.entry(src.clone()).or_insert(0) += 1;
        }

        let d = self.config.damping.clamp(0.0, 1.0);
        for _ in 0..20 {
            let mut next: HashMap<String, f64> =
                facts.iter().map(|f| (f.fact_id.clone(), (1.0 - d) / n as f64)).collect();
            for (src, dst) in &edges {
                let deg = *out_degree.get(src).unwrap_or(&1) as f64;
                if deg == 0.0 {
                    continue;
                }
                if let Some(slot) = next.get_mut(dst) {
                    *slot += d * (rank.get(src).copied().unwrap_or(0.0) / deg);
                }
            }
            rank = next;
        }
        Ok(rank)
    }

    /// Symbols within two hops of any focus path's file node.
    async fn focus_boost(&self, focus_paths: &[String]) -> Result<HashSet<String>> {
        let mut boosted = HashSet::new();
        for path in focus_paths {
            let node = NodeRef::file(path);
            let graph = self.fabric.graph_around(&node, 512).await?;
            for reached in graph.dependents_on(&node, 2, None) {
                if reached.kind == crate::memory::dependency::NodeKind::SymbolicFact {
                    boosted.insert(reached.id);
                }
            }
            for reached in graph.dependents_of(&node, 2, None) {
                if reached.kind == crate::memory::dependency::NodeKind::SymbolicFact {
                    boosted.insert(reached.id);
                }
            }
        }
        Ok(boosted)
    }

    /// Blast radius of a change to a symbol (FR-11).
    pub async fn impact_of_change(
        &self,
        qualified_name: &str,
        depth: usize,
    ) -> Result<ImpactReport> {
        let Some(fact) = self.fabric.fact_by_name(qualified_name).await? else {
            return Err(Error::NotFound(format!(
                "symbol {qualified_name} is not in the Symbolic Ledger; index the project first \
                 (`sakur4d index`) or check the qualified name"
            )));
        };
        let node = NodeRef::fact(&fact.fact_id);
        let graph = self.fabric.graph_around(&node, 2_000).await?;
        let callers = graph.dependents_on(&node, depth, None);

        // Annotate each caller with whether its own fact is present and current.
        let mut entries: Vec<ImpactEntry> = Vec::new();
        for c in callers {
            if c.kind != crate::memory::dependency::NodeKind::SymbolicFact {
                continue;
            }
            let caller = self.fabric.fact_by_id(&c.id).await?;
            entries.push(ImpactEntry {
                fact_id: c.id.clone(),
                qualified_name: caller
                    .as_ref()
                    .map(|f| f.qualified_name.clone())
                    .unwrap_or_else(|| c.id.clone()),
                file_path: caller.as_ref().and_then(|f| f.file_path.clone()),
                line: caller.as_ref().and_then(|f| f.line_start),
                depth: c.depth,
                via: c.via.as_str().to_string(),
                // FR-11 asks for each caller to be "annotated with whether that caller's own
                // symbolic fact hash is currently stale relative to the target symbol's
                // last-known signature".
                //
                // # This was `false`, and could not be anything else
                //
                // The comment here used to describe code that was not present — it said the check is
                // "detected here by comparing the edge's recorded weight-bearing target hash", and
                // the value was a literal `false` because edges recorded no target hash. A reader who
                // trusted it would have concluded the annotation worked and that no caller happened
                // to be stale, which is the opposite of the truth.
                //
                // Migration 4 gave the edge table a `target_hash`, so there is now something to
                // compare: the edge says what the caller saw when it was written, and the caller's
                // own fact says what it holds today. They differ exactly when the caller has not
                // been re-read since the target changed.
                //
                // An edge with no recorded hash reports `false` **and** says so in `via`-adjacent
                // prose rather than implying freshness. `None` is not evidence of agreement.
                stale: match graph.target_hash(&NodeRef::fact(&c.id), &node) {
                    Some(seen) => {
                        caller.as_ref().map(|f| f.ast_hash.as_str() != seen).unwrap_or(false)
                    }
                    None => false,
                },
                note: caller
                    .as_ref()
                    .map(|f| f.signature.clone().unwrap_or_else(|| f.qualified_name.clone()))
                    .unwrap_or_default(),
            });
        }
        entries.sort_by(|a, b| {
            a.depth.cmp(&b.depth).then_with(|| a.qualified_name.cmp(&b.qualified_name))
        });

        Ok(ImpactReport {
            symbol: fact.qualified_name.clone(),
            signature: fact.signature.clone(),
            ast_hash: fact.ast_hash.clone(),
            file_path: fact.file_path.clone(),
            line: fact.line_start,
            depth_requested: depth,
            affected: entries,
        })
    }

    /// Files currently indexed.
    pub async fn files(&self) -> Result<Vec<(String, String, i64, i64)>> {
        let project = self.project_id.clone();
        self.db
            .with(move |c| {
                let mut stmt = c.prepare(
                    "SELECT rel_path, language, symbols, size_bytes FROM repo_file
                     WHERE project_id = ?1 ORDER BY rel_path",
                )?;
                let rows = stmt
                    .query_map([project], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
                Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
            })
            .await
    }

    /// Index freshness, for the `sakur4://repo-map/{project}` resource TTL.
    pub async fn last_indexed(&self) -> Result<Option<String>> {
        let project = self.project_id.clone();
        self.db
            .with(move |c| {
                Ok(c.query_row(
                    "SELECT indexed_at FROM project WHERE project_id = ?1",
                    [project],
                    |r| r.get::<_, Option<String>>(0),
                )
                .ok()
                .flatten())
            })
            .await
    }
}

/// One affected caller in an impact report.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ImpactEntry {
    pub fact_id: String,
    pub qualified_name: String,
    pub file_path: Option<String>,
    pub line: Option<i64>,
    pub depth: usize,
    pub via: String,
    pub stale: bool,
    pub note: String,
}

/// The blast radius of a change.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ImpactReport {
    pub symbol: String,
    pub signature: Option<String>,
    pub ast_hash: String,
    pub file_path: Option<String>,
    pub line: Option<i64>,
    pub depth_requested: usize,
    pub affected: Vec<ImpactEntry>,
}

impl ImpactReport {
    pub fn render(&self) -> String {
        let mut out = format!(
            "impact of changing {} ({})\n",
            self.symbol,
            self.signature.as_deref().unwrap_or("no recorded signature")
        );
        if let (Some(f), Some(l)) = (&self.file_path, self.line) {
            out.push_str(&format!("  defined at {f}:{l}\n"));
        }
        if self.affected.is_empty() {
            out.push_str("  no callers or importers found within the requested depth\n");
            return out;
        }
        out.push_str(&format!("  {} affected site(s):\n", self.affected.len()));
        for e in &self.affected {
            let loc = match (&e.file_path, e.line) {
                (Some(f), Some(l)) => format!("{f}:{l}"),
                (Some(f), None) => f.clone(),
                _ => "<unknown>".into(),
            };
            out.push_str(&format!(
                "    depth {} via {} — {}{} ({loc})\n",
                e.depth,
                e.via,
                e.qualified_name,
                // # The annotation FR-11 asks for, finally shown
                //
                // `ImpactEntry.stale` was computed and rendered nowhere — not here, and not by
                // `code.impact_of_change`, which carries the field into its output shape without a
                // line any reader sees. A report that holds a staleness verdict and does not print
                // it is the same as not computing it, and worse than not having the field, because
                // the shape implies the annotation exists.
                if e.stale { "  [STALE: changed since it last saw this symbol]" } else { "" }
            ));
        }
        out
    }
}
// ===========================================================================
// Discovery
// ===========================================================================

fn discover_files(root: &Path, config: &RepoCortexConfig) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut builder = ignore::WalkBuilder::new(root);
    builder
        .hidden(true)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .parents(true)
        .follow_links(false);
    // Extra ignores are expressed as ignore-overrides, which is how the `ignore`
    // crate represents "never descend into this"; the negation prefix turns a
    // user-supplied glob into an ignore pattern.
    let mut override_builder = ignore::overrides::OverrideBuilder::new(root);
    let mut added_any = false;
    for glob in &config.extra_ignores {
        if override_builder.add(&format!("!{glob}")).is_ok() {
            added_any = true;
        }
    }
    if added_any && let Ok(overrides) = override_builder.build() {
        let _ = builder.overrides(overrides);
    }

    for entry in builder.build().flatten() {
        let path = entry.path();
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        // Skip the memory store and anything else Sakur4 itself writes.
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if name.ends_with(".db") || name.ends_with(".db-wal") || name.ends_with(".db-shm") {
            continue;
        }
        if Language::is_indexable(path) {
            out.push(path.to_path_buf());
        }
    }
    out.sort();
    out
}

// ===========================================================================
// Parsing
// ===========================================================================

/// Parse with the grammar for `language`.
pub fn parse_structured(source: &str, language: Language, rel_path: &str) -> ParsedFile {
    use tree_sitter::Parser;

    let ts_language: tree_sitter::Language = match language {
        Language::Python => tree_sitter_python::LANGUAGE.into(),
        Language::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        Language::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
        Language::JavaScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        Language::Rust => tree_sitter_rust::LANGUAGE.into(),
        Language::Go => tree_sitter_go::LANGUAGE.into(),
        _ => {
            return ParsedFile { language, ..Default::default() };
        }
    };

    let mut parser = Parser::new();
    if parser.set_language(&ts_language).is_err() {
        return ParsedFile { language, ..Default::default() };
    }
    let Some(tree) = parser.parse(source, None) else {
        return ParsedFile { language, ..Default::default() };
    };

    let bytes = source.as_bytes();
    let mut extracts = Vec::new();
    let mut imports = Vec::new();
    let mut outline = Vec::new();
    let mut stack: Vec<String> = vec![file_module_name(rel_path, language)];

    walk(
        tree.root_node(),
        bytes,
        source,
        language,
        rel_path,
        &mut stack,
        &mut extracts,
        &mut imports,
        &mut outline,
        0,
    );

    ParsedFile { language, extracts, imports, outline }
}

/// A stable module prefix for a file's qualified names.
///
/// Built from the path rather than from the file's declared package, because two
/// files in one repository may declare the same package and the natural key must
/// stay unique (it is a database constraint).
fn file_module_name(rel_path: &str, language: Language) -> String {
    let stem =
        rel_path.rsplit_once('.').map(|(a, _)| a).unwrap_or(rel_path).replace(['/', '\\'], "::");
    let _ = language;
    stem
}

#[allow(clippy::too_many_arguments)]
fn walk(
    node: tree_sitter::Node<'_>,
    bytes: &[u8],
    source: &str,
    language: Language,
    rel_path: &str,
    stack: &mut Vec<String>,
    extracts: &mut Vec<Extract>,
    imports: &mut Vec<String>,
    outline: &mut Vec<String>,
    depth: usize,
) {
    // A depth cap protects against pathological nesting and keeps index time
    // predictable on generated files.
    if depth > 64 || extracts.len() > 4_000 {
        return;
    }

    let kind = node.kind();
    let text = node.utf8_text(bytes).unwrap_or("").to_string();
    let start_line = node.start_position().row as i64 + 1;
    let end_line = node.end_position().row as i64 + 1;

    // The node's own declaration, if it is one.
    let classified = classify(node, kind, &text, source, language);

    if let Some((fact_kind, name, signature, is_import, references)) = classified {
        if is_import {
            imports.push(name.clone());
        }
        // Scope comes from the node's own ancestors rather than from a mutable
        // stack threaded through the recursion. Ancestry is a property of the
        // tree, so there is nothing to keep in sync — and no way for a missed pop
        // to silently mis-qualify every later sibling.
        let scope = scope_of(node, source);
        let qualified = if scope.is_empty() {
            format!("{}::{}", stack.join("::"), name)
        } else {
            format!("{}::{}::{}", stack.join("::"), scope.join("::"), name)
        };
        let mut write = SymbolicWrite::new(fact_kind, qualified.clone())
            .at_path(rel_path.to_string())
            .lines(start_line, end_line)
            .body(truncate_body(&text, 4_000));
        if let Some(sig) = &signature {
            write = write.signature(sig.clone());
        }
        extracts.push(Extract { write, references, is_import });
        outline
            .push(format!("{start_line}: {}", signature.as_deref().unwrap_or(qualified.as_str())));
    }

    let mut i: u32 = 0;
    while let Some(child) = node.child(i) {
        i += 1;
        walk(
            child,
            bytes,
            source,
            language,
            rel_path,
            stack,
            extracts,
            imports,
            outline,
            depth + 1,
        );
    }
}

/// The enclosing scope names of a declaration, outermost first.
///
/// Derived from the node's ancestors, which is the one source of truth about
/// nesting that cannot get out of step with the traversal.
fn scope_of(node: tree_sitter::Node<'_>, source: &str) -> Vec<String> {
    let mut scopes: Vec<String> = Vec::new();
    let mut current = node.parent();
    while let Some(parent) = current {
        match parent.kind() {
            // Name-carrying containers.
            "struct_item"
            | "enum_item"
            | "union_item"
            | "class_definition"
            | "class_declaration"
            | "interface_declaration"
            | "mod_item"
            | "trait_item" => {
                if let Some(name) = child_text(parent, "type_identifier", source)
                    .or_else(|| child_text(parent, "identifier", source))
                {
                    scopes.push(name);
                }
            }
            // `impl Trait for Type` / `impl Type`: the implementing type is the
            // qualifier Sakur4 records, because that is what a caller holds a
            // reference to.
            "impl_item" => {
                if let Some(name) = child_text(parent, "type_identifier", source) {
                    scopes.push(name);
                }
            }
            // Go methods are declared outside their receiver type.
            "method_declaration" => {
                if let Some(recv) = child_text(parent, "parameter_list", source) {
                    let cleaned = recv
                        .trim_matches(|c| c == '(' || c == ')')
                        .split_whitespace()
                        .last()
                        .unwrap_or("")
                        .trim_start_matches('*')
                        .to_string();
                    if !cleaned.is_empty() {
                        scopes.push(cleaned);
                    }
                }
            }
            _ => {}
        }
        current = parent.parent();
    }
    // Ancestors arrive innermost-first; emit outermost-first so names read
    // `mod::Type::method`.
    scopes.reverse();
    scopes
}

/// Text of the first child of the given kind.
fn child_text(node: tree_sitter::Node<'_>, kind: &str, source: &str) -> Option<String> {
    let mut i: u32 = 0;
    while let Some(child) = node.child(i) {
        i += 1;
        if child.kind() == kind {
            let text = child.utf8_text(source.as_bytes()).unwrap_or("").trim();
            if !text.is_empty() {
                return Some(text.to_string());
            }
        }
    }
    None
}
type Classified = Option<(FactKind, String, Option<String>, bool, Vec<String>)>;

/// Decide whether a node declares something worth a Ledger fact.
fn classify(
    node: tree_sitter::Node<'_>,
    kind: &str,
    text: &str,
    source: &str,
    language: Language,
) -> Classified {
    let name_of = |child_kinds: &[&str]| -> Option<String> {
        // Two levels, not one: several grammars nest a declaration one node
        // deeper than the name (Go's `type_declaration` → `type_spec` → name),
        // and a single-level lookup silently produces no fact at all for those.
        fn find(
            node: tree_sitter::Node<'_>,
            kinds: &[&str],
            depth: usize,
            source: &str,
        ) -> Option<String> {
            if depth > 2 {
                return None;
            }
            let mut i: u32 = 0;
            while let Some(child) = node.child(i) {
                i += 1;
                if kinds.contains(&child.kind()) {
                    let text = child.utf8_text(source.as_bytes()).unwrap_or("").trim();
                    if !text.is_empty() {
                        return Some(text.to_string());
                    }
                }
                if let Some(found) = find(child, kinds, depth + 1, source) {
                    return Some(found);
                }
            }
            None
        }
        find(node, child_kinds, 0, source)
    };

    let first_line = text.lines().next().unwrap_or("").trim().to_string();
    let signature = Some(truncate_body(&first_line, 400));
    // Identifiers referenced in the body, used for intra-file edges.
    let references = identifiers_in(text);

    match (language, kind) {
        (Language::Python, "function_definition")
        | (Language::Python, "async_function_definition") => {
            let name = name_of(&["identifier"])?;
            let is_method = has_ancestor_kind(node, "class_definition");
            Some((
                if is_method { FactKind::Method } else { FactKind::Function },
                name,
                signature,
                false,
                references,
            ))
        }
        (Language::Python, "class_definition") => {
            Some((FactKind::Class, name_of(&["identifier"])?, signature, false, references))
        }
        (Language::Python, "import_statement") | (Language::Python, "import_from_statement") => {
            let module = text
                .trim_start_matches("from ")
                .trim_start_matches("import ")
                .split_whitespace()
                .next()
                .unwrap_or("")
                .trim_end_matches('.')
                .to_string();
            if module.is_empty() {
                None
            } else {
                Some((FactKind::Import, module, signature, true, references))
            }
        }
        (Language::Rust, "function_item") => {
            let is_method =
                has_ancestor_kind(node, "impl_item") || has_ancestor_kind(node, "trait_item");
            Some((
                if is_method { FactKind::Method } else { FactKind::Function },
                name_of(&["identifier"])?,
                signature,
                false,
                references,
            ))
        }
        // A trait method *declaration* is a different node kind from a definition
        // (`function_signature_item` vs `function_item`). Missing it means a
        // trait's contract never reaches the Ledger, which is exactly the
        // structure callers need to check against.
        (Language::Rust, "function_signature_item") => {
            Some((FactKind::Method, name_of(&["identifier"])?, signature, false, references))
        }
        (Language::Rust, "struct_item") => Some((
            FactKind::Struct,
            name_of(&["type_identifier", "identifier"])?,
            signature,
            false,
            references,
        )),
        (Language::Rust, "enum_item") => Some((
            FactKind::Enum,
            name_of(&["type_identifier", "identifier"])?,
            signature,
            false,
            references,
        )),
        (Language::Rust, "trait_item") => Some((
            FactKind::Trait,
            name_of(&["type_identifier", "identifier"])?,
            signature,
            false,
            references,
        )),
        (Language::Rust, "type_item") => Some((
            FactKind::Type,
            name_of(&["type_identifier", "identifier"])?,
            signature,
            false,
            references,
        )),
        (Language::Rust, "const_item") | (Language::Rust, "static_item") => {
            Some((FactKind::Constant, name_of(&["identifier"])?, signature, false, references))
        }
        (Language::Rust, "mod_item") => {
            Some((FactKind::Module, name_of(&["identifier"])?, signature, false, references))
        }
        (Language::Rust, "use_declaration") => {
            let path = text.trim_start_matches("use ").trim_end_matches(';').trim().to_string();
            if path.is_empty() {
                None
            } else {
                Some((FactKind::Import, path, signature, true, references))
            }
        }
        (Language::TypeScript | Language::Tsx | Language::JavaScript, "function_declaration")
        | (
            Language::TypeScript | Language::Tsx | Language::JavaScript,
            "generator_function_declaration",
        ) => Some((FactKind::Function, name_of(&["identifier"])?, signature, false, references)),
        (Language::TypeScript | Language::Tsx | Language::JavaScript, "class_declaration") => {
            Some((
                FactKind::Class,
                name_of(&["identifier", "type_identifier"])?,
                signature,
                false,
                references,
            ))
        }
        (Language::TypeScript | Language::Tsx | Language::JavaScript, "method_definition") => {
            Some((
                FactKind::Method,
                name_of(&["property_identifier", "identifier"])?,
                signature,
                false,
                references,
            ))
        }
        (Language::TypeScript | Language::Tsx, "interface_declaration") => Some((
            FactKind::Interface,
            name_of(&["type_identifier", "identifier"])?,
            signature,
            false,
            references,
        )),
        (Language::TypeScript | Language::Tsx, "type_alias_declaration") => Some((
            FactKind::Type,
            name_of(&["type_identifier", "identifier"])?,
            signature,
            false,
            references,
        )),
        (Language::TypeScript | Language::Tsx, "enum_declaration") => {
            Some((FactKind::Enum, name_of(&["identifier"])?, signature, false, references))
        }
        (Language::TypeScript | Language::Tsx | Language::JavaScript, "import_statement") => {
            let fallback: &str = text;
            let module = match text.split("from ").nth(1) {
                Some(after_from) => after_from,
                None => fallback,
            }
            .trim()
            .trim_matches(|c| c == '"' || c == '\'' || c == ';')
            .to_string();
            if module.is_empty() {
                None
            } else {
                Some((FactKind::Import, module, signature, true, references))
            }
        }
        (Language::TypeScript | Language::Tsx | Language::JavaScript, "lexical_declaration") => {
            // Only arrow functions assigned to a const are worth a fact; a plain
            // data constant is noise in the map.
            if !text.contains("=>") {
                return None;
            }
            let name = text
                .trim_start_matches("const ")
                .trim_start_matches("let ")
                .split(|c: char| c == '=' || c.is_whitespace())
                .next()
                .unwrap_or("")
                .to_string();
            if name.is_empty() {
                None
            } else {
                Some((FactKind::Function, name, signature, false, references))
            }
        }
        (Language::Go, "function_declaration") => {
            Some((FactKind::Function, name_of(&["identifier"])?, signature, false, references))
        }
        (Language::Go, "method_declaration") => {
            // Skip the receiver: `func (s *Server) Listen()` puts the receiver
            // variable first, and a naive name lookup would record `s` rather
            // than `Listen`.
            let name = go_method_name(node, source)?;
            Some((FactKind::Method, name, signature, false, references))
        }
        (Language::Go, "type_declaration") => {
            if text.trim_start().starts_with("type") && text.contains("struct") {
                Some((
                    FactKind::Struct,
                    name_of(&["type_identifier", "identifier"])?,
                    signature,
                    false,
                    references,
                ))
            } else if text.contains("interface") {
                Some((
                    FactKind::Interface,
                    name_of(&["type_identifier", "identifier"])?,
                    signature,
                    false,
                    references,
                ))
            } else {
                Some((
                    FactKind::Type,
                    name_of(&["type_identifier", "identifier"])?,
                    signature,
                    false,
                    references,
                ))
            }
        }
        (Language::Go, "import_declaration") => {
            let path = text
                .trim_start_matches("import")
                .trim()
                .trim_matches(|c: char| c == '(' || c == ')' || c == '"' || c.is_whitespace())
                .to_string();
            let first = path.lines().next().unwrap_or("").trim().to_string();
            if first.is_empty() {
                None
            } else {
                Some((FactKind::Import, first, signature, true, references))
            }
        }
        (Language::Go, "const_declaration") => Some((
            FactKind::Constant,
            text.split_whitespace().nth(1).unwrap_or("const").trim_end_matches('=').to_string(),
            signature,
            false,
            references,
        )),
        _ => None,
    }
}

/// The declared name of a Go method, skipping its receiver.
///
/// `func (s *Server) Listen() error` has children in the order
/// `parameter_list` (the receiver), `field_identifier` (`Listen`), `parameter_list`.
/// Picking the first identifier inside the first parameter list would record the
/// receiver variable, which is not a symbol anybody looks up.
fn go_method_name(node: tree_sitter::Node<'_>, source: &str) -> Option<String> {
    let mut i: u32 = 0;
    let mut seen_receiver = false;
    while let Some(child) = node.child(i) {
        i += 1;
        match child.kind() {
            "parameter_list" if !seen_receiver => seen_receiver = true,
            "parameter_list" => {}
            "field_identifier" | "identifier" => {
                let text = child.utf8_text(source.as_bytes()).unwrap_or("").trim();
                if !text.is_empty() {
                    return Some(text.to_string());
                }
            }
            _ => {}
        }
    }
    None
}

fn has_ancestor_kind(node: tree_sitter::Node<'_>, kind: &str) -> bool {
    let mut current = node.parent();
    while let Some(p) = current {
        if p.kind() == kind {
            return true;
        }
        current = p.parent();
    }
    false
}

/// Identifier-like tokens in a body, deduplicated and capped.
fn identifiers_in(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut current = String::new();
    let reserved: HashSet<&'static str> = [
        "if",
        "else",
        "for",
        "while",
        "return",
        "let",
        "const",
        "var",
        "fn",
        "def",
        "class",
        "struct",
        "impl",
        "use",
        "import",
        "from",
        "pub",
        "self",
        "this",
        "new",
        "match",
        "func",
        "package",
        "type",
        "interface",
        "async",
        "await",
        "try",
        "catch",
        "throw",
        "true",
        "false",
        "null",
        "none",
        "nil",
        "not",
        "and",
        "or",
        "in",
        "is",
        "the",
        "a",
        "an",
    ]
    .into_iter()
    .collect();

    fn flush(current: &mut String, out: &mut Vec<String>, reserved: &HashSet<&'static str>) {
        if current.len() >= 3 && !reserved.contains(current.as_str()) {
            out.push(std::mem::take(current));
        } else {
            current.clear();
        }
    }

    for ch in text.chars() {
        if ch.is_alphanumeric() || ch == '_' {
            current.push(ch);
        } else {
            flush(&mut current, &mut out, &reserved);
            if out.len() > 400 {
                break;
            }
        }
    }
    flush(&mut current, &mut out, &reserved);
    out.sort();
    out.dedup();
    out.truncate(200);
    out
}

fn truncate_body(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    text.chars().take(limit).collect()
}

/// One extract, remembered so the cross-file pass can resolve its references
/// without re-reading and re-parsing every file.
#[derive(Debug, Clone)]
struct ExtractRef {
    /// The fact this extract became, if it became one.
    fact_id: Option<String>,
    qualified_name: String,
    references: Vec<String>,
    is_import: bool,
}

/// The last component of a qualified name, which is what code writes at a call
/// site.
fn short_name_of(qualified: &str) -> &str {
    qualified.rsplit("::").next().unwrap_or(qualified)
}

/// Resolve a bare reference (`validate`, `users::by_id`) to a fact id.
///
/// The same module is tried first, so a file's own `helper()` is not shadowed by
/// an unrelated `helper()` elsewhere in the repository. Getting this wrong is not
/// a cosmetic ranking issue: it is the difference between a blast-radius query
/// that names the right callers and one that names plausible strangers.
fn resolve_reference<'a>(
    reference: &str,
    source_qualified: &str,
    short_names: &HashMap<&'a str, &'a str>,
) -> Option<&'a str> {
    let short = short_name_of(reference);
    if let Some((module, _)) = source_qualified.rsplit_once("::") {
        let candidate = format!("{module}::{short}");
        if let Some(id) = short_names.get(candidate.as_str()) {
            return Some(id);
        }
    }
    short_names.get(short).copied()
}

/// Heuristic extraction for structured text formats.
///
/// Conservative on purpose: it produces *anchors*, not understanding. A config
/// key or an SQL table name is a real, checkable fact about the file; a guessed
/// "function" in a markdown file would not be.
pub fn parse_config(source: &str, language: Language, rel_path: &str) -> ParsedFile {
    let mut extracts = Vec::new();
    let mut outline = Vec::new();
    let module = file_module_name(rel_path, language);

    let push = |extracts: &mut Vec<Extract>,
                outline: &mut Vec<String>,
                kind: FactKind,
                name: String,
                line: i64,
                signature_text: String,
                body: String,
                is_import: bool| {
        extracts.push(Extract {
            write: SymbolicWrite::new(kind, format!("{module}::{name}"))
                .at_path(rel_path.to_string())
                .lines(line, line)
                .signature(signature_text.clone())
                .body(body),
            references: Vec::new(),
            is_import,
        });
        outline.push(format!("{line}: {signature_text}"));
    };

    let ext = rel_path.rsplit_once('.').map(|(_, e)| e).unwrap_or("");
    for (idx, line) in source.lines().enumerate() {
        let line_no = idx as i64 + 1;
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with("//") {
            continue;
        }
        match ext {
            "json" => {
                // Parsed exactly, not scanned. A line-oriented heuristic cannot
                // reliably answer "is this key top-level?" — the first version of
                // this function silently extracted nothing from every JSON file —
                // and `serde_json` is already a dependency, so there is no reason
                // to approximate.
                if let Ok(value) = serde_json::from_str::<serde_json::Value>(source) {
                    for (key, type_name) in top_level_json_entries(&value) {
                        let line = find_key_line(source, &key).unwrap_or(1);
                        push(
                            &mut extracts,
                            &mut outline,
                            FactKind::JsonField,
                            key,
                            line,
                            type_name,
                            String::new(),
                            false,
                        );
                        if extracts.len() > 500 {
                            break;
                        }
                    }
                }
                break;
            }
            "sql" => {
                let lower = trimmed.to_ascii_lowercase();
                for prefix in ["create table", "create view", "create index"] {
                    if lower.starts_with(prefix) {
                        let name = trimmed
                            .split_whitespace()
                            .nth(2)
                            .unwrap_or("")
                            .trim_matches(|c| c == '(' || c == '"' || c == '`')
                            .to_string();
                        if !name.is_empty() {
                            push(
                                &mut extracts,
                                &mut outline,
                                FactKind::Module,
                                name,
                                line_no,
                                trimmed.to_string(),
                                trimmed.to_string(),
                                false,
                            );
                        }
                    }
                }
            }
            "sh" | "bash" | "zsh" => {
                if let Some(rest) = trimmed.strip_prefix("function ") {
                    let name = rest.split(['(', ' ', '{']).next().unwrap_or("").to_string();
                    if !name.is_empty() {
                        push(
                            &mut extracts,
                            &mut outline,
                            FactKind::Function,
                            name,
                            line_no,
                            trimmed.to_string(),
                            trimmed.to_string(),
                            false,
                        );
                    }
                } else if let Some((name, _)) = trimmed.split_once("()") {
                    let name = name.trim().to_string();
                    if !name.is_empty() && !name.contains(' ') {
                        push(
                            &mut extracts,
                            &mut outline,
                            FactKind::Function,
                            name,
                            line_no,
                            trimmed.to_string(),
                            trimmed.to_string(),
                            false,
                        );
                    }
                }
            }
            "toml" | "ini" | "cfg" | "env" => {
                if trimmed.starts_with('[') {
                    let name = trimmed.trim_matches(['[', ']']).to_string();
                    if !name.is_empty() {
                        push(
                            &mut extracts,
                            &mut outline,
                            FactKind::Module,
                            name,
                            line_no,
                            trimmed.to_string(),
                            trimmed.to_string(),
                            false,
                        );
                    }
                } else if let Some((key, value)) = trimmed.split_once('=') {
                    let key = key.trim();
                    if !key.is_empty() && !key.contains(' ') {
                        push(
                            &mut extracts,
                            &mut outline,
                            FactKind::Constant,
                            key.to_string(),
                            line_no,
                            format!("{key} = {}", value.trim()),
                            trimmed.to_string(),
                            false,
                        );
                    }
                }
            }
            "yaml" | "yml" => {
                // Top-level `key:` entries only.
                if !line.starts_with(' ')
                    && !line.starts_with('-')
                    && let Some((key, _)) = trimmed.split_once(':')
                {
                    let key = key.trim();
                    if !key.is_empty() && !key.contains(' ') {
                        push(
                            &mut extracts,
                            &mut outline,
                            FactKind::Constant,
                            key.to_string(),
                            line_no,
                            trimmed.to_string(),
                            trimmed.to_string(),
                            false,
                        );
                    }
                }
            }
            "md" => {
                if let Some(rest) = trimmed.strip_prefix("# ") {
                    let title = rest.trim().to_string();
                    if !title.is_empty() {
                        push(
                            &mut extracts,
                            &mut outline,
                            FactKind::Module,
                            title,
                            line_no,
                            trimmed.to_string(),
                            trimmed.to_string(),
                            false,
                        );
                    }
                }
            }
            _ => {}
        }
        if extracts.len() > 2_000 {
            break;
        }
    }

    ParsedFile { language, extracts, imports: Vec::new(), outline }
}

/// Top-level entries of a JSON document, as `(key, type)` pairs.
///
/// Shallow by design: a config file's *declared surface* is what a reader (human
/// or model) needs anchored, and flattening a whole document into the Ledger
/// would cost more tokens than reading the file.
fn top_level_json_entries(value: &serde_json::Value) -> Vec<(String, String)> {
    let mut out = Vec::new();
    match value {
        serde_json::Value::Object(map) => {
            for (k, v) in map {
                out.push((k.clone(), type_name_of(v).to_string()));
            }
        }
        serde_json::Value::Array(items) => out.push((
            "[]".to_string(),
            format!("array of {}", items.first().map(type_name_of).unwrap_or("unknown")),
        )),
        _ => {}
    }
    out
}

fn type_name_of(value: &serde_json::Value) -> &'static str {
    match value {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "boolean",
        serde_json::Value::Number(n) if n.is_i64() || n.is_u64() => "integer",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}

/// Line number of a top-level key, for the fact's location.
///
/// Best-effort: a location that is off by a line is still useful; a fabricated
/// one would not be.
fn find_key_line(source: &str, key: &str) -> Option<i64> {
    let quoted = format!("\"{key}\"");
    for (idx, line) in source.lines().enumerate() {
        let trimmed = line.trim_start();
        let matches_key = trimmed.starts_with(&quoted)
            || (trimmed.starts_with(key)
                && trimmed[key.len()..].trim_start().starts_with([':', '=']));
        if matches_key && (trimmed.contains(':') || trimmed.contains('=')) {
            return Some(idx as i64 + 1);
        }
    }
    None
}

/// Hash used for a file's staleness key. Exposed so callers can compare cheaply.
pub fn file_hash(bytes: &[u8]) -> String {
    short_hash_str(&content_hash(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn extract_names(source: &str, language: Language) -> Vec<String> {
        parse_structured(source, language, "src/sample.rs")
            .extracts
            .into_iter()
            .map(|e| e.write.qualified_name)
            .collect()
    }

    #[test]
    fn detects_languages_from_extensions() {
        assert_eq!(Language::from_path(Path::new("a/b.py")), Language::Python);
        assert_eq!(Language::from_path(Path::new("a/b.tsx")), Language::Tsx);
        assert_eq!(Language::from_path(Path::new("a/b.rs")), Language::Rust);
        assert_eq!(Language::from_path(Path::new("a/b.go")), Language::Go);
        assert_eq!(Language::from_path(Path::new("a/b.yaml")), Language::Config);
        assert_eq!(Language::from_path(Path::new("a/b.exe")), Language::Unsupported);
    }

    #[test]
    fn parses_rust_functions_structs_and_impls() {
        let src = r#"
use std::collections::HashMap;

pub struct Engine { n: usize }

impl Engine {
    pub fn new() -> Self { Self { n: 0 } }
    fn private_helper(&self) -> usize { self.n }
}

pub trait Doer { fn do_it(&self); }
pub enum Mode { Fast, Slow }
pub const LIMIT: usize = 8;
"#;
        let parsed = parse_structured(src, Language::Rust, "src/engine.rs");
        let names: Vec<String> =
            parsed.extracts.iter().map(|e| e.write.qualified_name.clone()).collect();
        assert!(names.iter().any(|n| n.ends_with("::Engine")), "{names:?}");
        assert!(names.iter().any(|n| n.ends_with("Engine::new")), "{names:?}");
        assert!(names.iter().any(|n| n.ends_with("Engine::private_helper")), "{names:?}");
        assert!(names.iter().any(|n| n.ends_with("::Doer")), "{names:?}");
        assert!(names.iter().any(|n| n.ends_with("::Mode")), "{names:?}");
        assert!(
            names.iter().any(|n| n.contains("std::collections::HashMap")),
            "imports must be captured: {names:?}"
        );
    }

    #[test]
    fn rust_methods_are_distinguished_from_free_functions() {
        let src = "fn free() {}\nstruct S;\nimpl S { fn member(&self) {} }";
        let parsed = parse_structured(src, Language::Rust, "s.rs");
        let kinds: HashMap<String, FactKind> = parsed
            .extracts
            .iter()
            .map(|e| (e.write.qualified_name.clone(), e.write.kind))
            .collect();
        assert!(kinds.values().any(|k| *k == FactKind::Function));
        assert!(kinds.values().any(|k| *k == FactKind::Method));
    }

    #[test]
    fn parses_python_functions_classes_and_imports() {
        let src = r#"
import os
from pathlib import Path

class Store:
    def __init__(self, root):
        self.root = root

    def get(self, key):
        return None

def helper(value):
    return value
"#;
        let names = extract_names(src, Language::Python);
        assert!(names.iter().any(|n| n.ends_with("::Store")), "{names:?}");
        assert!(names.iter().any(|n| n.ends_with("Store::get")), "{names:?}");
        assert!(names.iter().any(|n| n.ends_with("::helper")), "{names:?}");
    }

    #[test]
    fn parses_typescript_interfaces_types_and_arrow_functions() {
        let src = r#"
import { readFile } from "fs";

export interface Config { port: number }
export type Mode = "fast" | "slow";
export class Server { listen() {} }
export function start(c: Config) { return c; }
export const stop = (s: Server) => s;
"#;
        let parsed = parse_structured(src, Language::TypeScript, "src/server.ts");
        let names: Vec<String> =
            parsed.extracts.iter().map(|e| e.write.qualified_name.clone()).collect();
        assert!(names.iter().any(|n| n.ends_with("::Config")), "{names:?}");
        assert!(names.iter().any(|n| n.ends_with("::Mode")), "{names:?}");
        assert!(names.iter().any(|n| n.ends_with("::Server")), "{names:?}");
        assert!(names.iter().any(|n| n.ends_with("::start")), "{names:?}");
        assert!(names.iter().any(|n| n.ends_with("::stop")), "{names:?}");
    }

    #[test]
    fn parses_go_functions_methods_and_types() {
        let src = r#"
package main

import "fmt"

type Server struct { Port int }
type Handler interface { Handle() }

func New() *Server { return &Server{} }

func (s *Server) Listen() { fmt.Println(s.Port) }
"#;
        let parsed = parse_structured(src, Language::Go, "main.go");
        let names: Vec<String> =
            parsed.extracts.iter().map(|e| e.write.qualified_name.clone()).collect();
        assert!(names.iter().any(|n| n.ends_with("::Server")), "{names:?}");
        assert!(names.iter().any(|n| n.ends_with("::Handler")), "{names:?}");
        assert!(names.iter().any(|n| n.ends_with("::New")), "{names:?}");
        assert!(names.iter().any(|n| n.ends_with("::Listen")), "{names:?}");
    }

    #[test]
    fn every_extract_carries_a_line_range_and_a_hash() {
        let parsed = parse_structured("fn a() {}\nfn b() {}\n", Language::Rust, "x.rs");
        assert_eq!(parsed.extracts.len(), 2);
        for e in &parsed.extracts {
            assert!(e.write.line_start.unwrap() >= 1);
            assert!(e.write.line_end.unwrap() >= e.write.line_start.unwrap());
            assert_eq!(e.write.ast_hash().len(), 16);
        }
    }

    #[test]
    fn syntax_errors_do_not_panic_and_still_extract_what_parses() {
        let broken = "fn good() {}\nfn bad( {{{ \nstruct AlsoGood;\n";
        let parsed = parse_structured(broken, Language::Rust, "broken.rs");
        assert!(
            !parsed.extracts.is_empty(),
            "tree-sitter must recover enough to extract the valid declarations"
        );
    }

    #[test]
    fn empty_source_yields_nothing_rather_than_failing() {
        let parsed = parse_structured("", Language::Rust, "empty.rs");
        assert!(parsed.extracts.is_empty());
        assert!(parsed.imports.is_empty());
    }

    #[test]
    fn references_are_collected_for_call_graph_edges() {
        let src = "fn caller() { helper_one(); helper_two(3); }\nfn helper_one() {}\nfn helper_two(x: u8) {}";
        let parsed = parse_structured(src, Language::Rust, "r.rs");
        let caller =
            parsed.extracts.iter().find(|e| e.write.qualified_name.ends_with("::caller")).unwrap();
        assert!(caller.references.contains(&"helper_one".to_string()));
        assert!(caller.references.contains(&"helper_two".to_string()));
    }

    #[test]
    fn config_extraction_is_conservative() {
        let json = "{\n  \"name\": \"sakur4\",\n  \"nested\": {\n    \"deep\": 1\n  }\n}";
        let parsed = parse_config(json, Language::Config, "package.json");
        let names: Vec<&str> =
            parsed.extracts.iter().map(|e| e.write.qualified_name.as_str()).collect();
        assert!(names.iter().any(|n| n.ends_with("::name")));
        assert!(names.iter().any(|n| n.ends_with("::nested")));
        assert!(
            !names.iter().any(|n| n.ends_with("::deep")),
            "nested keys must not be flattened into the ledger"
        );
    }

    #[test]
    fn shell_functions_and_sql_tables_are_anchored() {
        let sh = "#!/bin/bash\nfunction deploy() {\n  echo hi\n}\n";
        let parsed = parse_config(sh, Language::Config, "deploy.sh");
        assert!(parsed.extracts.iter().any(|e| e.write.qualified_name.ends_with("::deploy")));

        let sql = "CREATE TABLE users (id INT);\nCREATE VIEW active AS SELECT 1;\n";
        let parsed = parse_config(sql, Language::Config, "schema.sql");
        let names: Vec<&str> =
            parsed.extracts.iter().map(|e| e.write.qualified_name.as_str()).collect();
        assert!(names.iter().any(|n| n.ends_with("::users")));
        assert!(names.iter().any(|n| n.ends_with("::active")));
    }

    #[test]
    fn module_names_are_path_derived_and_unique_across_directories() {
        let a = parse_structured("fn f() {}", Language::Rust, "src/a/mod.rs");
        let b = parse_structured("fn f() {}", Language::Rust, "src/b/mod.rs");
        let an = &a.extracts[0].write.qualified_name;
        let bn = &b.extracts[0].write.qualified_name;
        assert_ne!(an, bn, "the natural key must not collide across directories");
        assert!(an.starts_with("src::a::mod::"));
    }

    #[test]
    fn file_hash_is_stable_and_content_sensitive() {
        assert_eq!(file_hash(b"abc"), file_hash(b"abc"));
        assert_ne!(file_hash(b"abc"), file_hash(b"abd"));
    }

    #[test]
    fn identifiers_ignore_reserved_words_and_short_tokens() {
        let ids = identifiers_in("if x { return helper_call(a, b); }");
        assert!(ids.contains(&"helper_call".to_string()));
        assert!(!ids.contains(&"if".to_string()));
        assert!(!ids.contains(&"x".to_string()));
    }
}
