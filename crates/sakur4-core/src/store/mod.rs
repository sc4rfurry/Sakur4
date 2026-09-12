//! Storage layer: SQLite connection management, migrations, lexical and vector
//! search backends.
//!
//! # Why a single writer connection
//!
//! SQLite in WAL mode permits one writer at a time regardless of how many
//! connections exist. Sakur4 therefore keeps exactly one `rusqlite::Connection`
//! in-process, guarded by a mutex and always driven on a blocking thread via
//! [`tokio::task::spawn_blocking`]. Readers cannot block on it because WAL
//! readers are independent, and the harness-facing write path (`commit_episode`)
//! inherits that non-blocking property for free (FR-1).

pub mod db;
pub mod fts;
pub mod schema;
pub mod vector;

pub use db::{Db, DbStats, WriteTxn};
pub use fts::LexicalBackend;
pub use schema::{migrate, MIGRATIONS};
pub use vector::{VectorBackend, VectorHit};
