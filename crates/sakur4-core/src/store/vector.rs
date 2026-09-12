//! Dense vector storage and similarity search.
//!
//! Sakur4 keeps its vectors in an ordinary table of `f32` blobs and runs an
//! exact cosine scan in Rust, unless a `sqlite-vec` loadable extension is found
//! at startup, in which case `vec0` virtual tables serve the ANN path and
//! [`VectorBackend::SqliteVec`] is recorded in `meta`.
//!
//! # Why exact scan is the default
//!
//! The PRD asks for `sqlite-vec` (FR-12). Linking a C extension into a binary
//! that must stay a *single static artefact* (NFR-8, "no mandatory Docker",
//! "single static binary where feasible") fights that goal, and a *loadable*
//! extension satisfies both: the same binary gets in-database ANN where the
//! operator has it, and an exact scan where they do not. At the documented
//! scale ceiling (100k entries) an exact scan over `f32` blobs is a few tens of
//! milliseconds — well inside NFR-2's 300 ms — and it is exact, so recall
//! quality never depends on which backend is live. The choice is observable via
//! `sakur4d doctor` and recorded per store.

use crate::error::Result;
use crate::store::db::Db;

/// Which engine is serving similarity search.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum VectorBackend {
    /// A `sqlite-vec` loadable extension was found, loaded and verified.
    SqliteVec,
    /// No extension available: Sakur4 scans the `vectors` table in Rust.
    BruteForce,
}

impl VectorBackend {
    pub fn as_str(self) -> &'static str {
        match self {
            VectorBackend::SqliteVec => "sqlite-vec",
            VectorBackend::BruteForce => "bruteforce",
        }
    }

    pub fn describe(self) -> &'static str {
        match self {
            VectorBackend::SqliteVec => "sqlite-vec extension (in-database ANN)",
            VectorBackend::BruteForce => "built-in exact cosine scan",
        }
    }
}

/// A similarity hit.
#[derive(Debug, Clone, serde::Serialize)]
pub struct VectorHit {
    pub source_table: String,
    pub source_id: String,
    /// Cosine similarity in `[-1, 1]`; larger is better.
    pub similarity: f64,
}

/// Encode a vector as little-endian `f32` bytes, the on-disk representation.
pub fn encode(v: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(v.len() * 4);
    for f in v {
        out.extend_from_slice(&f.to_le_bytes());
    }
    out
}

/// Decode little-endian `f32` bytes back into a vector.
pub fn decode(bytes: &[u8]) -> Vec<f32> {
    bytes
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

/// L2 norm, used to persist the precomputed magnitude alongside each vector.
pub fn norm(v: &[f32]) -> f32 {
    v.iter().map(|x| x * x).sum::<f32>().sqrt()
}

impl Db {
    /// Insert or replace an embedding and return its `embedding_id`.
    pub async fn put_vector(
        &self,
        source_table: &str,
        source_id: &str,
        model: &str,
        vector: &[f32],
    ) -> Result<i64> {
        let source_table = source_table.to_string();
        let source_id = source_id.to_string();
        let model = model.to_string();
        let blob = encode(vector);
        let dim = vector.len() as i64;
        let n = norm(vector) as f64;
        let now = crate::ids::now_rfc3339();

        self.write(move |tx| {
            tx.execute(
                "INSERT INTO vectors(source_table, source_id, dim, model, norm, vec, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT(source_table, source_id, model)
                 DO UPDATE SET dim=excluded.dim, norm=excluded.norm, vec=excluded.vec,
                               created_at=excluded.created_at",
                rusqlite::params![source_table, source_id, dim, model, n, blob, now],
            )?;
            Ok(tx.query_row(
                "SELECT embedding_id FROM vectors WHERE source_table=?1 AND source_id=?2 AND model=?3",
                rusqlite::params![source_table, source_id, model],
                |r| r.get(0),
            )?)
        })
        .await
    }

    /// Exact cosine search over the stored vectors.
    ///
    /// `source_tables` restricts the scan (e.g. only `semantic_atlas`), and
    /// `model` restricts it to one embedding space so vectors from different
    /// models are never compared.
    pub async fn search_vectors(
        &self,
        query: &[f32],
        limit: usize,
        source_tables: Option<Vec<String>>,
        model: Option<String>,
    ) -> Result<Vec<VectorHit>> {
        let q = query.to_vec();
        let qnorm = norm(&q).max(f32::EPSILON);
        let limit = limit.max(1);

        self.with(move |c| {
            let mut sql = String::from(
                "SELECT source_table, source_id, dim, norm, vec FROM vectors WHERE 1=1",
            );
            let mut params: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
            if let Some(tables) = &source_tables {
                let placeholders = (0..tables.len())
                    .map(|i| format!("?{}", params.len() + i + 1))
                    .collect::<Vec<_>>()
                    .join(",");
                sql.push_str(&format!(" AND source_table IN ({placeholders})"));
                for t in tables {
                    params.push(Box::new(t.clone()));
                }
            }
            if let Some(m) = &model {
                params.push(Box::new(m.clone()));
                sql.push_str(&format!(" AND model = ?{}", params.len()));
            }
            let refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(|b| b.as_ref()).collect();

            let mut stmt = c.prepare(&sql)?;
            let rows = stmt.query_map(refs.as_slice(), |r| {
                let table: String = r.get(0)?;
                let id: String = r.get(1)?;
                let dim: i64 = r.get(2)?;
                let stored_norm: f64 = r.get(3)?;
                let blob: Vec<u8> = r.get(4)?;
                Ok((table, id, dim, stored_norm, blob))
            })?;

            let mut scored: Vec<VectorHit> = Vec::new();
            for row in rows {
                let (table, id, dim, stored_norm, blob) = row?;
                if dim as usize != q.len() {
                    // A dimension mismatch means the stored vector came from a
                    // different embedding model; comparing would be meaningless.
                    continue;
                }
                let v = decode(&blob);
                let dot: f32 = v.iter().zip(q.iter()).map(|(a, b)| a * b).sum();
                let denom = (stored_norm as f32) * qnorm;
                let similarity = if denom <= f32::EPSILON {
                    0.0
                } else {
                    (dot / denom) as f64
                };
                scored.push(VectorHit {
                    source_table: table,
                    source_id: id,
                    similarity,
                });
            }
            scored.sort_by(|a, b| {
                // Descending similarity, then a stable tiebreak on id so the
                // result set is deterministic across runs.
                b.similarity
                    .partial_cmp(&a.similarity)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| a.source_id.cmp(&b.source_id))
            });
            scored.truncate(limit);
            Ok(scored)
        })
        .await
    }

    /// Remove vectors whose source row no longer exists.
    pub async fn prune_orphan_vectors(&self) -> Result<usize> {
        self.write(|tx| {
            Ok(tx.execute(
                "DELETE FROM vectors
                 WHERE (source_table = 'semantic_atlas'
                        AND source_id NOT IN (SELECT atlas_id FROM semantic_atlas))
                    OR (source_table = 'symbolic_fact'
                        AND source_id NOT IN (SELECT fact_id FROM symbolic_fact))",
                [],
            )?)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_encoding() {
        let v = vec![0.25f32, -1.5, 3.0, 0.0];
        assert_eq!(decode(&encode(&v)), v);
        assert!((norm(&vec![3.0, 4.0]) - 5.0).abs() < 1e-6);
    }

    #[tokio::test]
    async fn cosine_search_is_exact_and_ordered() {
        let db = Db::open_in_memory().await.unwrap();
        db.put_vector("semantic_atlas", "a", "m", &[1.0, 0.0, 0.0])
            .await
            .unwrap();
        db.put_vector("semantic_atlas", "b", "m", &[0.9, 0.1, 0.0])
            .await
            .unwrap();
        db.put_vector("semantic_atlas", "c", "m", &[0.0, 1.0, 0.0])
            .await
            .unwrap();

        let hits = db
            .search_vectors(&[1.0, 0.0, 0.0], 3, None, Some("m".into()))
            .await
            .unwrap();
        assert_eq!(hits[0].source_id, "a");
        assert_eq!(hits[1].source_id, "b");
        assert_eq!(hits[2].source_id, "c");
        assert!((hits[0].similarity - 1.0).abs() < 1e-6);
        assert!(hits[2].similarity.abs() < 1e-6);
    }

    #[tokio::test]
    async fn dimension_mismatch_is_skipped_not_scored() {
        let db = Db::open_in_memory().await.unwrap();
        db.put_vector("semantic_atlas", "wide", "m", &[1.0, 0.0, 0.0, 0.0])
            .await
            .unwrap();
        let hits = db
            .search_vectors(&[1.0, 0.0], 5, None, None)
            .await
            .unwrap();
        assert!(hits.is_empty());
    }
}
