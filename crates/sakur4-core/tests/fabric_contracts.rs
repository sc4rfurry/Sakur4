//! Contracts for the Memory Fabric — the API everything else is built on.
//!
//! # Why this file exists
//!
//! `fabric.rs` holds 47 functions and, before this, no tests of its own. It was
//! covered incidentally: eviction tests committed episodes, recall tests pinned
//! anchors. That is enough to keep it *running*, and not enough to keep it *correct* —
//! a regression in how episodes are ordered, or in what `evictable` returns, would
//! surface as a confusing failure three layers away.
//!
//! So these are direct: call the fabric, assert the contract. They are deliberately
//! small and boring, because the point is to pin behaviour that other code assumes,
//! not to demonstrate anything.
//!
//! # The invariants being pinned
//!
//! Several of these are guarantees the README makes about the whole system. If one
//! breaks here, the claim made to a user is false.

use sakur4_core::memory::anchor::{AnchorKind, PinRequest};
use sakur4_core::memory::episodic::{EpisodeTier, NewEpisode};
use sakur4_core::memory::fabric::MemoryFabric;
use sakur4_core::memory::semantic::{AnchorType, SemanticWrite};
use sakur4_core::store::Db;
use sakur4_core::tokens::TokenCounter;

async fn fabric() -> (MemoryFabric, TokenCounter) {
    let db = Db::open_in_memory().await.expect("in-memory store");
    (MemoryFabric::new(db), TokenCounter::heuristic())
}

/// Commit one turn and return its id.
async fn commit(fabric: &MemoryFabric, tokens: &TokenCounter, session: &str, text: &str) -> String {
    fabric
        .commit_episode(NewEpisode::user(session, text), tokens, true, true)
        .await
        .expect("commit")
        .episode_id
}

// ===========================================================================
// The append-only guarantee
// ===========================================================================

#[tokio::test]
async fn an_evicted_episode_still_recalls_byte_identically() {
    // The README states this as a property of the schema rather than of careful
    // coding. This is the test that makes that statement checkable: evict it, then
    // read it back and compare the bytes.
    let (fabric, tokens) = fabric().await;
    let original = "the exact text, with punctuation — and a unicode snowman ☃";
    let id = commit(&fabric, &tokens, "s1", original).await;

    fabric.set_tier(&id, EpisodeTier::Masked).await.expect("mask");
    fabric.set_tier(&id, EpisodeTier::Archived).await.expect("archive");

    let row = fabric.episode(&id).await.expect("read back");
    assert_eq!(
        row.content, original,
        "eviction changed the stored bytes — the round-trip guarantee is broken"
    );
    assert_eq!(row.eviction_tier, EpisodeTier::Archived, "but the tier must have moved");
}

#[tokio::test]
async fn episodes_come_back_in_the_order_they_were_written() {
    // Sequence numbers drive the prompt assembler and the receipt's timeline. An
    // ordering regression would reorder a conversation without changing any content,
    // which is the kind of bug that reads as a model problem rather than a bug.
    let (fabric, tokens) = fabric().await;
    for i in 0..12 {
        commit(&fabric, &tokens, "s1", &format!("turn {i}")).await;
    }

    let episodes = fabric.session_episodes("s1").await.expect("list");
    assert_eq!(episodes.len(), 12);

    let seqs: Vec<i64> = episodes.iter().map(|e| e.seq).collect();
    let mut sorted = seqs.clone();
    sorted.sort_unstable();
    assert_eq!(seqs, sorted, "episodes returned out of sequence order");

    for (i, e) in episodes.iter().enumerate() {
        assert_eq!(e.content, format!("turn {i}"), "content and position disagree");
    }
}

#[tokio::test]
async fn sessions_do_not_bleed_into_each_other() {
    let (fabric, tokens) = fabric().await;
    commit(&fabric, &tokens, "alpha", "alpha's turn").await;
    commit(&fabric, &tokens, "beta", "beta's turn").await;
    commit(&fabric, &tokens, "alpha", "alpha's second").await;

    let alpha = fabric.session_episodes("alpha").await.expect("alpha");
    let beta = fabric.session_episodes("beta").await.expect("beta");
    assert_eq!(alpha.len(), 2);
    assert_eq!(beta.len(), 1);
    assert!(alpha.iter().all(|e| e.content.starts_with("alpha")));
    assert!(beta.iter().all(|e| e.content.starts_with("beta")));
}

#[tokio::test]
async fn recent_episodes_returns_the_newest_and_respects_the_limit() {
    let (fabric, tokens) = fabric().await;
    for i in 0..10 {
        commit(&fabric, &tokens, "s1", &format!("turn {i}")).await;
    }

    let recent = fabric.recent_episodes("s1", 3).await.expect("recent");
    assert_eq!(recent.len(), 3, "limit not honoured");
    let contents: Vec<&str> = recent.iter().map(|e| e.content.as_str()).collect();
    assert!(
        contents.contains(&"turn 9"),
        "the newest turn must be among the recent ones, got {contents:?}"
    );
}

// ===========================================================================
// Anchors — the guarantee most likely to be relied on
// ===========================================================================

#[tokio::test]
async fn an_anchor_is_never_in_the_evictable_set() {
    // This is the structural claim: eviction selects from episodes and anchors live in
    // a different table, so evicting a pinned constraint is not expressible. If this
    // test ever fails, the headline guarantee in the README is false.
    let (fabric, tokens) = fabric().await;
    let episode_id = commit(&fabric, &tokens, "s1", "never force-push to main").await;

    let _ = &episode_id;
    fabric
        .pin(PinRequest {
            session_id: Some("s1".into()),
            kind: AnchorKind::SafetyConstraint,
            content: "never force-push to main".into(),
            project_id: None,
            pinned_by: "user".into(),
        })
        .await
        .expect("pin");

    let anchors = fabric.anchors(Some("s1")).await.expect("anchors");
    assert_eq!(anchors.len(), 1);
    assert_eq!(anchors[0].kind, AnchorKind::SafetyConstraint);

    // The anchor is not an episode, so it cannot appear in the evictable set whatever
    // happens to the episode it came from.
    let evictable = fabric.evictable_episodes("s1").await.expect("evictable");
    let anchor_ids: Vec<&str> = anchors.iter().map(|a| a.anchor_id.as_str()).collect();
    for episode in &evictable {
        assert!(
            !anchor_ids.contains(&episode.episode_id.as_str()),
            "an anchor id appeared in the evictable episode set"
        );
    }

    // And the anchor survives its source being evicted.
    fabric.set_tier(&episode_id, EpisodeTier::Archived).await.expect("evict the source");
    let after = fabric.anchors(Some("s1")).await.expect("anchors after");
    assert_eq!(after.len(), 1, "the anchor died with its source episode");
}

#[tokio::test]
async fn unpinning_removes_exactly_one_anchor() {
    let (fabric, _tokens) = fabric().await;
    let mut ids = Vec::new();
    for (kind, text) in [
        (AnchorKind::SafetyConstraint, "never force-push"),
        (AnchorKind::UserCorrection, "it is validateUser, not checkUser"),
        (AnchorKind::TaskContract, "keep the public API stable"),
    ] {
        let anchor = fabric
            .pin(PinRequest {
                session_id: Some("s1".into()),
                kind,
                content: text.into(),
                project_id: None,
                pinned_by: "user".into(),
            })
            .await
            .expect("pin");
        ids.push(anchor.anchor_id);
    }
    assert_eq!(fabric.anchors(Some("s1")).await.expect("before").len(), 3);

    assert!(fabric.unpin(&ids[1]).await.expect("unpin"), "unpin reported nothing removed");
    let remaining = fabric.anchors(Some("s1")).await.expect("after");
    assert_eq!(remaining.len(), 2);
    assert!(
        !remaining.iter().any(|a| a.anchor_id == ids[1]),
        "the unpinned anchor is still present"
    );
    assert!(remaining.iter().any(|a| a.anchor_id == ids[0]), "unpin removed the wrong one");
    assert!(remaining.iter().any(|a| a.anchor_id == ids[2]), "unpin removed the wrong one");

    assert!(!fabric.unpin(&ids[1]).await.expect("unpin twice"), "unpinning twice must be a no-op");
}

#[tokio::test]
async fn anchors_can_be_filtered_by_session_and_listed_globally() {
    let (fabric, _tokens) = fabric().await;
    for (session, text) in [("a", "rule for a"), ("b", "rule for b")] {
        fabric
            .pin(PinRequest {
                session_id: Some(session.into()),
                kind: AnchorKind::TaskContract,
                content: text.into(),
                project_id: None,
                pinned_by: "user".into(),
            })
            .await
            .expect("pin");
    }

    assert_eq!(fabric.anchors(Some("a")).await.expect("a").len(), 1);
    assert_eq!(
        fabric.anchors(None).await.expect("all").len(),
        2,
        "an unfiltered listing must return anchors from every session"
    );
}

// ===========================================================================
// Symbolic facts — the track a model cannot write
// ===========================================================================

#[tokio::test]
async fn a_file_outline_is_empty_for_a_file_with_no_facts() {
    // Callers print this directly, so an error here would surface as a failure rather
    // than as an empty listing. Absent is not the same as broken.
    let (fabric, _tokens) = fabric().await;
    let outline = fabric.file_outline("nothing/here.rs").await.expect("outline");
    assert!(outline.is_empty(), "expected an empty outline, got {outline:?}");
}

// ===========================================================================
// Semantic entries and staleness
// ===========================================================================

#[tokio::test]
async fn a_semantic_entry_cannot_be_anchored_to_something_that_does_not_exist() {
    // FR-3 makes anchoring mandatory, and an *unanchored* write is not even
    // expressible — `SemanticWrite` requires an anchor id. The stronger contract is
    // this one: the id must resolve. An interpretation citing a fact that was never
    // recorded is exactly the drift Sakur4 exists to catch, and storing it would
    // leave a dangling entry that nothing can ever mark stale.
    let (fabric, _tokens) = fabric().await;
    let result = fabric
        .put_semantic(SemanticWrite::new(
            "a claim citing a fact that was never recorded",
            AnchorType::SymbolicFact,
            "sym_does_not_exist",
        ))
        .await;

    assert!(
        result.is_err(),
        "a semantic entry anchored to a non-existent fact was accepted; \
         FR-3 requires the anchor to resolve"
    );
    let message = format!("{}", result.unwrap_err());
    assert!(
        message.contains("non-existent") || message.contains("FR-3"),
        "the refusal must say what was wrong, got: {message}"
    );
}

#[tokio::test]
async fn extra_anchors_must_resolve_too() {
    // The same rule applied to incidental dependencies: a summary that claims to
    // depend on something imaginary is equally undetectable later, so the check is
    // not limited to the primary anchor.
    let (fabric, tokens) = fabric().await;
    let episode_id = commit(&fabric, &tokens, "s1", "a real turn").await;

    let mut write = SemanticWrite::new(
        "a summary with one real anchor and one invented one",
        AnchorType::EpisodicStream,
        episode_id,
    );
    write.extra_anchors.push((AnchorType::SymbolicFact, "sym_imaginary".into()));

    assert!(
        fabric.put_semantic(write).await.is_err(),
        "an entry was accepted with an extra anchor that does not exist"
    );
}

#[tokio::test]
async fn a_staleness_report_on_an_empty_store_is_empty_not_an_error() {
    let (fabric, _tokens) = fabric().await;
    let report = fabric.staleness_report(None, 50).await.expect("report");
    assert_eq!(report.total, 0);
    assert_eq!(report.stale, 0);
}
