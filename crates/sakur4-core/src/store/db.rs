//! Connection management.
//!
//! One connection, one mutex, all work on blocking threads. See the module
//! documentation in [`crate::store`] for the reasoning.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::Mutex;
use rusqlite::Connection;

use crate::error::{Error, Result};
use crate::store::schema;
use crate::store::vector::VectorBackend;

/// How many times a write transaction is retried on `SQLITE_BUSY` before giving
/// up. SQLite's own `busy_timeout` handles most contention; this covers the
/// upgrade-to-write-lock race that `busy_timeout` does not retry.
const BUSY_RETRIES: usize = 6;

/// Handle to the Memory Fabric store.
///
/// Cheap to clone: it is an `Arc` around a mutex-protected connection.
#[derive(Clone)]
pub struct Db {
    conn: Arc<Mutex<Connection>>,
    path: PathBuf,
    read_only: bool,
    vector_backend: VectorBackend,
    fts5: bool,
}

/// Observable facts about the open store, surfaced by `sakur4d doctor`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct DbStats {
    pub path: String,
    pub schema_version: i64,
    pub vector_backend: String,
    pub fts5: bool,
    pub journal_mode: String,
    pub page_size: i64,
    pub size_bytes: u64,
    pub episodes: i64,
    pub symbolic_facts: i64,
    pub semantic_entries: i64,
    pub anchors: i64,
    pub stale_entries: i64,
    pub folds_open: i64,
}

impl Db {
    /// Open (creating if needed) a store at `path`, applying migrations.
    ///
    /// A loadable `sqlite-vec` extension is used when one can be located via
    /// `SAKUR4_SQLITE_VEC_PATH` or next to the executable; otherwise Sakur4
    /// transparently falls back to its built-in exact scan.
    pub async fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }

        let p = path.clone();
        let (vector_backend, fts5, schema_version) = tokio::task::spawn_blocking(move || {
            let mut conn = Connection::open(&p)?;
            let vector_backend = crate::store::db::try_load_sqlite_vec(&conn);
            let (schema_version, _applied) = schema::migrate(&mut conn)
                .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
            let fts5 = conn
                .query_row(
                    "SELECT 1 FROM sqlite_master WHERE type='table' AND name='episodic_fts'",
                    [],
                    |_| Ok(true),
                )
                .unwrap_or(false);
            if !fts5 {
                // Lexical recall is P0 (FR-12); if FTS5 is missing we say so
                // loudly rather than silently degrading to a LIKE scan.
                tracing::error!(
                    "FTS5 is unavailable in this SQLite build; lexical recall disabled. \
                     sakur4-core expects the `bundled` feature of rusqlite."
                );
            }
            schema::record_vector_backend(&conn, vector_backend)
                .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
            conn.execute(
                "INSERT INTO meta(key,value) VALUES('fts5', ?1)
                 ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                [if fts5 { "1" } else { "0" }],
            )?;
            Ok::<_, rusqlite::Error>((vector_backend, fts5, schema_version))
        })
        .await
        .map_err(|e| Error::Pool(format!("migration task panicked: {e}")))??;

        tracing::info!(
            path = %path.display(),
            schema_version,
            vector_backend = vector_backend.as_str(),
            fts5,
            "memory fabric opened"
        );

        Ok(Self {
            conn: Arc::new(Mutex::new(Connection::open(&path)?)),
            path,
            read_only: false,
            vector_backend,
            fts5,
        })
    }

    /// Open an in-memory store. Used by tests and by the `--ephemeral` demo mode.
    ///
    /// # The one subtlety
    ///
    /// SQLite gives every `:memory:` connection its *own* private database, so
    /// migrating one connection and then handing out another would produce a
    /// store with no tables at all. The connection is therefore created once,
    /// migrated in place, and moved into the store — never reopened.
    pub async fn open_in_memory() -> Result<Self> {
        let (conn, vector_backend, fts5) = tokio::task::spawn_blocking(move || {
            let mut conn = Connection::open_in_memory()?;
            // An in-memory database cannot use WAL; the rest of the pragmas
            // still apply, and callers must not assume WAL semantics here.
            conn.execute_batch(
                "PRAGMA foreign_keys = ON;
                 PRAGMA temp_store = MEMORY;",
            )?;
            let vector_backend = try_load_sqlite_vec(&conn);
            schema::migrate(&mut conn)
                .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
            let fts5 = conn
                .query_row(
                    "SELECT 1 FROM sqlite_master WHERE type='table' AND name='episodic_fts'",
                    [],
                    |_| Ok(true),
                )
                .unwrap_or(false);
            schema::record_vector_backend(&conn, vector_backend)
                .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
            Ok::<_, rusqlite::Error>((conn, vector_backend, fts5))
        })
        .await
        .map_err(|e| Error::Pool(format!("migration task panicked: {e}")))??;

        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            path: PathBuf::from(":memory:"),
            read_only: false,
            vector_backend,
            fts5,
        })
    }

    /// Path of the backing file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Which vector engine is live.
    pub fn vector_backend(&self) -> VectorBackend {
        self.vector_backend
    }

    /// Whether FTS5 indexing is available.
    pub fn has_fts5(&self) -> bool {
        self.fts5
    }

    /// Run a closure with shared access to the connection on a blocking thread.
    ///
    /// The closure receives `&mut Connection` because rusqlite prepares
    /// statements through a mutable borrow; it must not run DDL.
    pub async fn with<T, F>(&self, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> Result<T> + Send + 'static,
    {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let mut guard = conn.lock();
            f(&mut guard)
        })
        .await
        .map_err(|e| Error::Pool(format!("db task panicked: {e}")))?
    }

    /// Run a closure inside an immediate (write) transaction, retrying on
    /// `SQLITE_BUSY`.
    ///
    /// Failures roll back. This is the only path that mutates the Fabric, which
    /// keeps NFR-5/NFR-6 ("no crash may corrupt the Episodic Stream") a property
    /// of one function rather than a property of every call site.
    pub async fn write<T, F>(&self, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&WriteTxn<'_>) -> Result<T> + Send + 'static,
    {
        if self.read_only {
            return Err(Error::Integrity("store opened read-only".into()));
        }
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let mut guard = conn.lock();
            let mut attempt = 0usize;
            loop {
                match guard.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate) {
                    Ok(tx) => {
                        let txn = WriteTxn { tx: &tx };
                        return match f(&txn) {
                            Ok(value) => {
                                tx.commit()?;
                                Ok(value)
                            }
                            Err(e) => {
                                // Explicit rollback; dropping would also work but
                                // this keeps the failure visible in traces.
                                let _ = tx.rollback();
                                Err(e)
                            }
                        };
                    }
                    Err(e) if is_busy(&e) && attempt < BUSY_RETRIES => {
                        attempt += 1;
                        std::thread::sleep(std::time::Duration::from_millis(
                            4u64 << attempt.min(5),
                        ));
                    }
                    Err(e) => return Err(Error::Sqlite(e)),
                }
            }
        })
        .await
        .map_err(|e| Error::Pool(format!("db task panicked: {e}")))?
    }

    /// Snapshot of store contents for `doctor` / the MCP `context.receipt` surface.
    pub async fn stats(&self) -> Result<DbStats> {
        let path = self.path.clone();
        let vector_backend = self.vector_backend;
        let fts5_available = self.fts5;
        self.with(move |c| {
            let journal_mode: String = c
                .query_row("PRAGMA journal_mode", [], |r| r.get(0))
                .unwrap_or_else(|_| "unknown".into());
            let page_size: i64 = c
                .query_row("PRAGMA page_size", [], |r| r.get(0))
                .unwrap_or(0);
            let size_bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            // Counts are best-effort: a store mid-migration reports zero rather
            // than failing `doctor`, which is the command an operator reaches for
            // precisely when something is wrong.
            let scalar = |sql: &str| -> i64 {
                c.query_row(sql, [], |r| r.get::<_, i64>(0)).unwrap_or(0)
            };
            Ok(DbStats {
                path: path.display().to_string(),
                schema_version: scalar("SELECT CAST(value AS INTEGER) FROM meta WHERE key='schema_version'"),
                vector_backend: vector_backend.as_str().to_string(),
                fts5: fts5_available,
                journal_mode,
                page_size,
                size_bytes,
                episodes: scalar("SELECT COUNT(*) FROM episodic_stream"),
                symbolic_facts: scalar("SELECT COUNT(*) FROM symbolic_fact"),
                semantic_entries: scalar("SELECT COUNT(*) FROM semantic_atlas"),
                anchors: scalar("SELECT COUNT(*) FROM anchor_set"),
                stale_entries: scalar(
                    "SELECT COUNT(*) FROM semantic_atlas_staleness
                     WHERE current_anchor_hash IS NOT NULL
                       AND current_anchor_hash <> anchor_hash_at_write",
                ),
                folds_open: scalar("SELECT COUNT(*) FROM folds WHERE status='open'"),
            })
        })
        .await
    }

    /// Direct access for tests that need to assert on the schema itself.
    #[doc(hidden)]
    pub fn connection(&self) -> &Mutex<Connection> {
        &self.conn
    }
}

/// The transaction handle passed to [`Db::write`] closures.
pub struct WriteTxn<'a> {
    tx: &'a rusqlite::Transaction<'a>,
}

impl std::ops::Deref for WriteTxn<'_> {
    type Target = rusqlite::Connection;
    fn deref(&self) -> &Self::Target {
        self.tx
    }
}

impl WriteTxn<'_> {
    pub fn execute<P: rusqlite::Params>(&self, sql: &str, params: P) -> Result<usize> {
        Ok(self.tx.execute(sql, params)?)
    }

    pub fn query_row<T, P, F>(&self, sql: &str, params: P, f: F) -> Result<T>
    where
        P: rusqlite::Params,
        F: FnOnce(&rusqlite::Row<'_>) -> rusqlite::Result<T>,
    {
        Ok(self.tx.query_row(sql, params, f)?)
    }

    /// Insert a row, ignoring uniqueness conflicts on the natural key.
    pub fn upsert(&self, sql: &str, params: impl rusqlite::Params) -> Result<()> {
        self.tx.execute(sql, params)?;
        Ok(())
    }
}

fn is_busy(e: &rusqlite::Error) -> bool {
    matches!(
        e,
        rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error {
                code: rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked,
                ..
            },
            _
        )
    )
}

/// Best-effort load of a `sqlite-vec` loadable extension.
///
/// Detection order: explicit `SAKUR4_SQLITE_VEC_PATH`, then a platform-default
/// library name sitting beside the running executable. Returns which backend is
/// live. Failure is never fatal — this is precisely the "dynamic" behaviour the
/// design calls for: the same binary gets in-database ANN where the extension
/// exists and an exact scan where it does not.
pub(crate) fn try_load_sqlite_vec(conn: &Connection) -> VectorBackend {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(explicit) = std::env::var("SAKUR4_SQLITE_VEC_PATH")
        && !explicit.trim().is_empty() {
            candidates.push(PathBuf::from(explicit));
        }
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent() {
            for name in [
                "vec0.dll",
                "libvec0.so",
                "vec0.so",
                "libvec0.dylib",
                "vec0.dylib",
            ] {
                candidates.push(dir.join(name));
            }
        }

    for candidate in candidates {
        if !candidate.exists() {
            continue;
        }
        // SAFETY: loading a shared library runs arbitrary code. Sakur4 only ever
        // loads a path the operator explicitly provided or placed next to the
        // binary, and never downloads one (NFR-10: no mandatory network egress).
        let loaded = unsafe {
            match conn.load_extension_enable() {
                Ok(()) => {
                    let r = conn.load_extension(&candidate, None::<&str>);
                    let _ = conn.load_extension_disable();
                    r.is_ok()
                }
                Err(_) => false,
            }
        };
        if loaded {
            let works = conn
                .query_row("SELECT vec_version()", [], |r| r.get::<_, String>(0))
                .is_ok();
            if works {
                tracing::info!(path = %candidate.display(), "sqlite-vec loaded");
                return VectorBackend::SqliteVec;
            }
        }
        tracing::warn!(path = %candidate.display(), "sqlite-vec present but unusable; using exact scan");
    }
    VectorBackend::BruteForce
}
