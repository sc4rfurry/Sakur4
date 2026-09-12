//! Identifier and hashing helpers.
//!
//! Sakur4 uses UUIDv7 for every primary key so ids sort by creation time, which
//! matters for the Episodic Stream (recovery, range scans) and for reading
//! `sakur4d` logs. Content hashes are BLAKE3 truncated to 16 hex characters:
//! 64 bits is far beyond what collision resistance a staleness check needs, and
//! short hashes keep the repo map and receipts readable — FR-15 requires the
//! receipt be human-readable as-is, and 64-character hashes in a printed outline
//! are noise.

use uuid::Uuid;

/// A fresh time-ordered id with a readable prefix, e.g. `ep_018f...`.
pub fn new_id(prefix: &str) -> String {
    format!("{prefix}_{}", Uuid::now_v7().simple())
}

/// A raw UUIDv7 string, for fields that are not namespaced.
pub fn uuid_v7() -> String {
    Uuid::now_v7().to_string()
}

/// BLAKE3 of `bytes`, truncated to 16 hex characters.
pub fn short_hash(bytes: &[u8]) -> String {
    let hash = blake3::hash(bytes);
    hash.to_hex()[..16].to_string()
}

/// BLAKE3 of a string, truncated to 16 hex characters.
pub fn short_hash_str(s: &str) -> String {
    short_hash(s.as_bytes())
}

/// Full 64-character BLAKE3 hex digest, for whole-file content addressing.
pub fn content_hash(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

/// Current time as RFC 3339 with millisecond precision, in UTC.
///
/// Sakur4 stores timestamps as text so that a `sqlite3` CLI session is readable
/// without extensions, and so lexicographic ordering equals chronological
/// ordering.
pub fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// Parse a stored RFC 3339 timestamp.
pub fn parse_rfc3339(s: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|d| d.with_timezone(&chrono::Utc))
}

/// Normalise a repository-relative path to forward slashes.
///
/// Paths are part of the Symbolic Ledger's natural key, so a Windows run and a
/// WSL run over the same repository must produce the same keys.
pub fn normalize_rel_path(p: &std::path::Path) -> String {
    p.components()
        .filter_map(|c| match c {
            std::path::Component::Normal(s) => Some(s.to_string_lossy().to_string()),
            std::path::Component::ParentDir => Some("..".to_string()),
            std::path::Component::CurDir => None,
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_prefixed_and_sort_by_time() {
        let a = new_id("ep");
        std::thread::sleep(std::time::Duration::from_millis(2));
        let b = new_id("ep");
        assert!(a.starts_with("ep_"));
        assert!(a < b, "uuidv7 ids must be time-ordered");
    }

    #[test]
    fn hashes_are_stable_and_short() {
        assert_eq!(short_hash_str("abc"), short_hash_str("abc"));
        assert_ne!(short_hash_str("abc"), short_hash_str("abd"));
        assert_eq!(short_hash_str("abc").len(), 16);
        assert_eq!(content_hash(b"abc").len(), 64);
    }

    #[test]
    fn rel_paths_use_forward_slashes() {
        let p = std::path::Path::new("src").join("nested").join("mod.rs");
        assert_eq!(normalize_rel_path(&p), "src/nested/mod.rs");
    }
}
