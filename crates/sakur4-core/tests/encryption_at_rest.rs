//! FR-20: optional encryption at rest.
//!
//! # The acceptance criterion, verbatim
//!
//! > Enabling encryption at setup produces a store unreadable by a plain sqlite3
//! > client without the configured key.
//!
//! That is testable without trusting anything: open a store with a key, then try to
//! read it with a connection that does not have one. The read must fail. The file
//! must also not look like a SQLite database, because "unreadable by a plain client"
//! is a property of the bytes on disk rather than of a flag inside them.
//!
//! # Why these tests are gated
//!
//! They only mean anything when `rusqlite` is built against SQLCipher, which is what
//! the `encryption` feature does. Without it `PRAGMA key` is accepted and ignored —
//! SQLite has no such pragma, and an unknown pragma is not an error — so a test
//! asserting "the file is unreadable" would fail, and a test asserting "the pragma
//! was accepted" would pass while proving nothing. Gating is the honest option: the
//! suite says which build it verified.
//!
//! ```text
//! cargo test -p sakur4-core --features encryption --test encryption_at_rest
//! ```

#![cfg(feature = "encryption")]

use rusqlite::Connection;
use sakur4_core::memory::episodic::NewEpisode;
use sakur4_core::store::Db;
use sakur4_core::store::db::{generate_key, key_pragma};
use sakur4_core::tokens::TokenCounter;

/// A valid key for tests. Generated rather than hardcoded, so no test fixture
/// becomes a key somebody reuses.
fn a_key() -> String {
    generate_key()
}

// ===========================================================================
// The acceptance criterion
// ===========================================================================

#[tokio::test]
async fn an_encrypted_store_cannot_be_read_without_the_key() {
    // The criterion, directly. Two things are asserted, because either alone is
    // insufficient: the read must fail (the store is protected), and it must fail on
    // data that is really there (the protection is not simply an empty file).
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("encrypted.db");
    let key = a_key();

    {
        let db = Db::open_encrypted(&path, &key).await.expect("open encrypted");
        db.write(|tx| {
            tx.execute(
                "INSERT INTO episodic_stream
                 (episode_id, seq, session_id, role, content, token_count, created_at)
                 VALUES ('e0', 1, 's1', 'user', 'a secret that must not leak', 7,
                         '2026-01-01T00:00:00Z')",
                [],
            )?;
            Ok(())
        })
        .await
        .expect("write");
    }

    // Proof the data is really in there, read back with the key.
    {
        let db = Db::open_encrypted(&path, &key).await.expect("reopen with the key");
        let content: String = db
            .with(|c| {
                Ok(c.query_row(
                    "SELECT content FROM episodic_stream WHERE episode_id='e0'",
                    [],
                    |r| r.get(0),
                )?)
            })
            .await
            .expect("read with the key");
        assert_eq!(content, "a secret that must not leak");
    }

    // A plain client, with no key. This is the assertion the requirement names.
    let plain = Connection::open(&path);
    match plain {
        Ok(conn) => {
            let attempt: rusqlite::Result<String> =
                conn.query_row("SELECT content FROM episodic_stream LIMIT 1", [], |r| r.get(0));
            assert!(attempt.is_err(), "a plain client read the encrypted store: {attempt:?}");
        }
        Err(_) => {
            // Refusing to open at all is also a pass — arguably a stronger one.
        }
    }
}

#[tokio::test]
async fn the_wrong_key_does_not_open_the_store() {
    // A key that is well-formed but not the one used. SQLCipher has no separate
    // "wrong key" signal, so this must fail at the first read rather than returning
    // garbage — which is the behaviour worth pinning, because a store that opened
    // and returned nonsense would be worse than one that refused.
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("encrypted.db");

    {
        let db = Db::open_encrypted(&path, &a_key()).await.expect("open");
        db.write(|tx| {
            tx.execute(
                "INSERT INTO episodic_stream
                 (episode_id, seq, session_id, role, content, token_count, created_at)
                 VALUES ('e0', 1, 's1', 'user', 'body', 1, '2026-01-01T00:00:00Z')",
                [],
            )?;
            Ok(())
        })
        .await
        .expect("write");
    }

    let result = Db::open_encrypted(&path, &a_key()).await;
    assert!(result.is_err(), "the store opened with the wrong key; encryption is not in effect");
}

#[tokio::test]
async fn the_file_does_not_look_like_a_sqlite_database() {
    // A plain SQLite file begins with the 16 bytes "SQLite format 3\0". An encrypted
    // one must not — that header is what a `sqlite3` client reads first, so leaving it
    // in place would advertise the format even if the pages were ciphertext.
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("encrypted.db");

    {
        let db = Db::open_encrypted(&path, &a_key()).await.expect("open");
        db.write(|tx| {
            tx.execute(
                "INSERT INTO episodic_stream
                 (episode_id, seq, session_id, role, content, token_count, created_at)
                 VALUES ('e0', 1, 's1', 'user', 'body', 1, '2026-01-01T00:00:00Z')",
                [],
            )?;
            Ok(())
        })
        .await
        .expect("write");
    }

    let bytes = std::fs::read(&path).expect("read the file");
    assert!(bytes.len() >= 16, "file is too short to be a database");
    assert_ne!(
        &bytes[..16],
        b"SQLite format 3\0",
        "the encrypted store still carries a plaintext SQLite header"
    );
}

// ===========================================================================
// The key itself
// ===========================================================================

#[test]
fn a_generated_key_is_usable_and_unique() {
    let a = generate_key();
    let b = generate_key();
    assert_eq!(a.len(), 64, "a 256-bit key is 64 hex characters");
    assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    assert_ne!(a, b, "two generated keys must differ");
    assert!(key_pragma(&a).is_ok());
}

#[test]
fn a_short_key_is_refused_rather_than_stretched() {
    // The failure this prevents: a user sets a memorable four-character key, the
    // store is encrypted with something guessable, and the word "encrypted" gives
    // them confidence it does not deserve. Refusing loudly is the only safe answer,
    // because silently stretching cannot be distinguished from working.
    for bad in ["", "hunter2", "0123456789abcdef", &"a".repeat(63), &"a".repeat(65)] {
        assert!(
            key_pragma(bad).is_err(),
            "a key of {} character(s) was accepted; only 64 hex characters may be",
            bad.len()
        );
    }
}

#[test]
fn a_key_in_sqlcipher_notation_is_accepted_as_written() {
    // A caller following SQLCipher's own documentation writes `x'…'`. Making them
    // reshape it would be a papercut with no safety benefit — the inner value is
    // validated either way.
    let hex = generate_key();
    assert_eq!(key_pragma(&hex).unwrap(), format!("x'{hex}'"));
    assert_eq!(key_pragma(&format!("x'{hex}'")).unwrap(), format!("x'{hex}'"));
}

#[test]
fn surrounding_whitespace_is_tolerated_but_nothing_else_is() {
    // Keys arrive from files and environment variables, and a trailing newline from
    // either is an accident rather than a different key. Anything beyond whitespace
    // is a mistake worth surfacing.
    let hex = generate_key();
    assert!(key_pragma(&format!("  {hex}\n")).is_ok());
    assert!(key_pragma(&format!("{hex}zz")).is_err());
    assert!(key_pragma(&format!("{hex} ")).is_ok());
}

// ===========================================================================
// Encryption must not break what the store does
// ===========================================================================

#[tokio::test]
async fn an_encrypted_store_supports_the_full_memory_fabric() {
    // Encryption that quietly disabled FTS5 or the symbolic track would be a poor
    // trade, and would only be noticed much later. So the ordinary path is exercised
    // through an encrypted store: commit, retrieve, and confirm the lexical index
    // still answers.
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("encrypted.db");
    let key = a_key();

    let db = Db::open_encrypted(&path, &key).await.expect("open");
    assert!(db.has_fts5(), "FTS5 must survive encryption; lexical recall is P0");

    let fabric = sakur4_core::memory::fabric::MemoryFabric::new(db.clone());
    let tokens = TokenCounter::heuristic();
    fabric
        .commit_episode(
            NewEpisode::user("s1", "the retry helper takes max_attempts not retries"),
            &tokens,
            true,
            true,
        )
        .await
        .expect("commit");

    // The whole point of the retrieval fix applies here too: an encrypted store must
    // not reintroduce the natural-language query failure.
    let hits = db
        .search_episodes("how many retries does the helper take", 5, None, false)
        .await
        .expect("search");
    assert!(!hits.is_empty(), "an encrypted store must recall as well as a plain one");
}
