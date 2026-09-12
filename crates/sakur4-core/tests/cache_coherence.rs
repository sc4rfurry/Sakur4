//! Behavioural contracts for cache-coherent compaction.
//!
//! # Why these are integration tests
//!
//! The unit tests in `evict.rs` and `cache/mod.rs` check the pieces. These check
//! the *claim the project is built on*: that a compaction can leave the head of
//! the prompt untouched, so the inference server reuses its KV cache instead of
//! re-prefilling.
//!
//! That claim is easy to break silently. During development it broke three times,
//! each in a way that left every existing test green:
//!
//! 1. The boundary was proposed at token 0, so no prefix survived to align to.
//! 2. A ring that had already wrapped was treated as "nothing alignable" rather
//!    than "align forward onto the oldest thing you still hold".
//! 3. The plan reported the checkpoint it *aimed* for rather than the prefix it
//!    actually preserved, so the receipt described a prompt that never existed.
//!
//! Each test below fails if its corresponding mistake comes back.

use std::sync::Arc;

use sakur4_core::cache::{CacheStatus, Coherence, CoherenceConfig};
use sakur4_core::evict::{EvictionEngine, EvictionPolicy, Pressure};
use sakur4_core::llama::embedded::EmbeddedBackend;
use sakur4_core::llama::InferenceBackend;
use sakur4_core::memory::episodic::NewEpisode;
use sakur4_core::memory::fabric::MemoryFabric;
use sakur4_core::prompt::PromptParts;
use sakur4_core::store::Db;
use sakur4_core::tokens::{CharTokenizer, TokenCounter};

const WINDOW: usize = 32_768;

fn counter() -> TokenCounter {
    TokenCounter::new(CharTokenizer { chars_per_token: 4 })
}

struct Rig {
    fabric: MemoryFabric,
    engine: EvictionEngine,
    coherence: Coherence,
    backend: Arc<EmbeddedBackend>,
    counter: TokenCounter,
}

async fn rig() -> Rig {
    let db = Db::open_in_memory().await.expect("in-memory store");
    let backend = Arc::new(EmbeddedBackend::new());
    let counter = counter();
    let fabric = MemoryFabric::new(db.clone());
    let coherence = Coherence::new(db.clone(), backend.clone(), CoherenceConfig::default());
    let engine = EvictionEngine::new(
        fabric.clone(),
        coherence.clone(),
        EvictionPolicy::default(),
        counter.clone(),
    );
    Rig {
        fabric,
        engine,
        coherence,
        backend,
        counter,
    }
}

impl Rig {
    /// Commit a session of `turns` bulky turns, and keep the simulated slot in
    /// step with it — which is what a real harness does by sending each prompt.
    ///
    /// Returns the live token count, so a caller can assert it actually exceeded
    /// the compaction threshold rather than silently testing nothing.
    async fn seed_session(&self, turns: usize) -> usize {
        for i in 0..turns {
            self.fabric
                .commit_episode(
                    NewEpisode::user(
                        "s1",
                        format!(
                            "Turn {i}: refactor the CacheCoherenceLayer so that eviction \
                             boundaries snap onto checkpoints, keeping boundary_snap_delta within \
                             tolerance_tokens so the surviving prefix stays an LCP match. {}",
                            "This turn also carries enough incident detail to be worth several \
                             hundred tokens on its own. "
                                .repeat(8)
                        ),
                    )
                    .with_slot("0"),
                    &self.counter,
                    false,
                    false,
                )
                .await
                .expect("commit");
        }
        let live = self
            .fabric
            .session_live_tokens("s1", &self.counter)
            .await
            .expect("live tokens");
        // The server has now seen the whole conversation.
        self.backend.advance_to(live as i64);
        live
    }

    async fn parts(&self) -> PromptParts {
        let anchors = self
            .fabric
            .anchors(Some("s1"))
            .await
            .expect("anchors")
            .iter()
            .map(|a| a.render())
            .collect::<Vec<_>>()
            .join("\n");
        let timeline = self
            .fabric
            .timeline("s1", usize::MAX, &self.counter, false)
            .await
            .expect("timeline");
        PromptParts::new()
            .with_system("you are a local agent")
            .with_anchors(anchors)
            .with_timeline(timeline.rendered)
    }
}

#[tokio::test]
async fn a_compaction_preserves_a_prefix_the_cache_can_reuse() {
    let r = rig().await;
    let live = r.seed_session(120).await;
    // Guard the fixture itself: a session that never reaches the threshold would
    // make every assertion below vacuous.
    assert!(
        live > r.engine.threshold_for(WINDOW),
        "the fixture must exceed the compaction threshold; it reached {live} tokens"
    );
    let parts = r.parts().await;

    let plan = r
        .engine
        .plan("s1", "0", WINDOW, &parts)
        .await
        .expect("plan");
    assert_eq!(plan.pressure, Pressure::Compacting);
    assert!(!plan.is_empty(), "a full window must produce evictions");

    // The claim: something survives at the head, and it is reported as reusable.
    assert!(
        plan.retained_prefix_tokens > 0,
        "an eviction that starts at token 0 can never reuse a prefix"
    );
    let coherence = plan.coherence.as_ref().expect("cache verdict");
    assert_eq!(
        coherence.status,
        CacheStatus::PartialReuse,
        "the boundary must resolve as reusable; reason: {}",
        coherence.reason
    );
    assert!(
        coherence.aligned_cut.unwrap_or(0) <= plan.retained_prefix_tokens as i64,
        "the reported boundary ({:?}) must not claim more prefix than is preserved ({})",
        coherence.aligned_cut,
        plan.retained_prefix_tokens
    );

    // And the promise that makes it meaningful: the preserved text is the head of
    // what the server will actually be sent. `retained_prefix_text` includes the
    // system and anchor blocks, because those precede the timeline in the real
    // prompt and the cache holds them too — so the comparison is against the
    // concatenation in emission order, not against the timeline alone.
    let retained = parts.retained_prefix_text(plan.retained_prefix_tokens, &r.counter);
    let mut emitted = String::new();
    for (part, _, text) in parts.sections() {
        if part.order() > sakur4_core::prompt::RenderedPart::Timeline.order() {
            break;
        }
        emitted.push_str(text);
    }
    assert!(
        emitted.starts_with(&retained),
        "the preserved prefix must be a byte-prefix of what the server is sent"
    );
    assert!(
        retained.contains("<user> Turn 0"),
        "the preserved prefix must contain session content, not only scaffolding"
    );
    assert!(
        !retained.contains("Turn 119"),
        "the preserved prefix must stop before the end of the session"
    );
}

#[tokio::test]
async fn the_receipt_reports_reuse_only_after_the_plan_is_applied() {
    let r = rig().await;
    r.seed_session(120).await;
    let parts = r.parts().await;

    // Before any compaction the slot has no recorded state for this boundary, so
    // the honest answer is a cold slot — not a flattering guess.
    let before = r
        .coherence
        .observe_prompt("s1", "0", &parts.render(), &r.counter)
        .await
        .expect("observe");
    assert_eq!(before.cache_status, CacheStatus::Cold);

    let plan = r.engine.plan("s1", "0", WINDOW, &parts).await.expect("plan");
    r.engine.apply(&plan, &parts).await.expect("apply");

    let after_parts = r.parts().await;
    let after = r
        .coherence
        .observe_prompt("s1", "0", &after_parts.render(), &r.counter)
        .await
        .expect("observe");
    assert_eq!(
        after.cache_status,
        CacheStatus::PartialReuse,
        "after an aligned compaction the next turn must report reuse; got {}",
        after.detail
    );
    assert!(
        after.reused_tokens > 0,
        "reuse must be a real number, not a label: {}",
        after.detail
    );
    assert_eq!(
        after.reused_tokens + after.prefilled_tokens,
        after.prompt_tokens,
        "reused and prefilled must account for the whole prompt"
    );
}

#[tokio::test]
async fn anchors_and_pinned_constraints_are_never_in_the_eviction_set() {
    let r = rig().await;
    r.seed_session(120).await;
    r.fabric
        .pin(sakur4_core::memory::anchor::PinRequest::new(
            sakur4_core::memory::anchor::AnchorKind::SafetyConstraint,
            "never force-push to main",
        ))
        .await
        .expect("pin");

    let parts = r.parts().await;
    let plan = r.engine.plan("s1", "0", WINDOW, &parts).await.expect("plan");
    assert!(
        plan.anchor_tokens > 0,
        "the pinned anchor must be accounted for in the budget"
    );

    // G4: zero silent loss, under arbitrary compaction pressure.
    r.engine.apply(&plan, &parts).await.expect("apply");
    let anchors = r.fabric.anchors(Some("s1")).await.expect("anchors");
    assert_eq!(anchors.len(), 1, "the anchor set must be untouched");

    let rendered = r.parts().await.render();
    assert!(
        rendered.contains("never force-push to main"),
        "the anchor must still be in every assembled prompt"
    );
}

#[tokio::test]
async fn an_evicted_episode_recalls_byte_identically() {
    let r = rig().await;
    r.seed_session(120).await;
    let parts = r.parts().await;
    let plan = r.engine.plan("s1", "0", WINDOW, &parts).await.expect("plan");

    let before: Vec<(String, String)> = r
        .fabric
        .session_episodes("s1")
        .await
        .expect("episodes")
        .into_iter()
        .map(|e| (e.episode_id, e.content))
        .collect();

    r.engine.apply(&plan, &parts).await.expect("apply");
    assert!(!plan.updates.is_empty(), "the plan should have evicted something");

    for update in &plan.updates {
        let original = before
            .iter()
            .find(|(id, _)| id == &update.episode_id)
            .map(|(_, c)| c.clone())
            .expect("the evicted episode existed");
        let episode = r
            .fabric
            .episode(&update.episode_id)
            .await
            .expect("episode still exists");
        assert_eq!(
            episode.content, original,
            "FR-5 round-trip integrity: eviction must not alter stored content"
        );
        assert_eq!(
            episode.eviction_tier, update.to,
            "the tier must be what the plan said"
        );
    }
}

#[tokio::test]
async fn a_backend_with_no_checkpoints_falls_back_and_says_so() {
    let db = Db::open_in_memory().await.expect("store");
    let backend: Arc<dyn InferenceBackend> =
        Arc::new(sakur4_core::llama::embedded::NullBackend::new());
    let counter = counter();
    let fabric = MemoryFabric::new(db.clone());
    let coherence = Coherence::new(db.clone(), backend, CoherenceConfig::default());
    let engine = EvictionEngine::new(
        fabric.clone(),
        coherence,
        EvictionPolicy::default(),
        counter.clone(),
    );

    for i in 0..120 {
        fabric
            .commit_episode(
                NewEpisode::user(
                    "s1",
                    format!(
                        "Turn {i}: refactor the CacheCoherenceLayer so that eviction boundaries \
                         snap onto checkpoints and the surviving prefix stays an LCP match. {}",
                        "Incident detail worth several hundred tokens per turn, with enough surrounding narrative to make the turn substantial. ".repeat(24)
                    ),
                )
                .with_slot("0"),
                &counter,
                false,
                false,
            )
            .await
            .expect("commit");
    }

    let anchors = fabric.anchors(Some("s1")).await.expect("anchors");
    let timeline = fabric
        .timeline("s1", usize::MAX, &counter, false)
        .await
        .expect("timeline");
    let parts = PromptParts::new()
        .with_system("system")
        .with_anchors(
            anchors
                .iter()
                .map(|a| a.render())
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .with_timeline(timeline.rendered);

    let plan = engine
        .plan("s1", "0", WINDOW, &parts)
        .await
        .expect("plan");

    // Guard the fixture: a session that never reaches the threshold would make the
    // fallback assertions below vacuous.
    assert_eq!(
        plan.pressure,
        Pressure::Compacting,
        "the fixture must exceed the compaction threshold; live is {} of {}",
        plan.live_tokens,
        plan.threshold
    );
    assert!(!plan.is_empty(), "eviction must still happen without cache support");
    let coherence = plan.coherence.as_ref().expect("verdict");
    assert_eq!(
        coherence.status,
        CacheStatus::FullRePrefill,
        "with no checkpoint source the honest verdict is a full re-prefill"
    );
    assert!(!coherence.is_reuse());

    let outcome = engine.apply(&plan, &parts).await.expect("apply");
    assert!(outcome.applied > 0);
    assert!(outcome.tokens_reclaimed > 0);
}

#[tokio::test]
async fn repeated_compactions_do_not_oscillate_across_the_boundary() {
    // Two compactions in a row must agree about where the boundary is. An engine
    // that cuts to one side of a checkpoint on one turn and the other side on the
    // next re-prefills on every turn, which is worse than never compacting.
    let r = rig().await;
    r.seed_session(120).await;

    let parts = r.parts().await;
    let first = r.engine.plan("s1", "0", WINDOW, &parts).await.expect("plan 1");
    r.engine.apply(&first, &parts).await.expect("apply 1");
    let first_boundary = first.retained_prefix_tokens;

    // Grow the session again, as a real one would.
    for i in 0..80 {
        r.fabric
            .commit_episode(
                NewEpisode::user(
                    "s1",
                    format!("Later turn {i}: {}", "more work in this session. ".repeat(14)),
                )
                .with_slot("0"),
                &r.counter,
                false,
                false,
            )
            .await
            .expect("commit");
    }
    let live = r
        .fabric
        .session_live_tokens("s1", &r.counter)
        .await
        .expect("live");
    r.backend.advance_to(live as i64);

    let parts2 = r.parts().await;
    let second = r.engine.plan("s1", "0", WINDOW, &parts2).await.expect("plan 2");

    assert_eq!(second.pressure, Pressure::Compacting);
    assert!(
        second.retained_prefix_tokens > 0,
        "the second compaction must also preserve a prefix"
    );
    // The second boundary must be at least as far in as the first: the prefix is
    // already cached, so cutting behind it would discard exactly the cached part.
    assert!(
        second.retained_prefix_tokens >= first_boundary,
        "the second boundary ({}) moved behind the first ({}), which would discard the cached \
         prefix and re-prefill it",
        second.retained_prefix_tokens,
        first_boundary
    );
    let coherence = second.coherence.as_ref().expect("verdict");
    assert_eq!(
        coherence.status,
        CacheStatus::PartialReuse,
        "the second compaction must still resolve as reusable; {}",
        coherence.reason
    );
}
