//! Backends that need no external server.
//!
//! Two live here, and both are load-bearing rather than test scaffolding:
//!
//! * [`EmbeddedBackend`] is what `--backend auto` resolves to when nothing is
//!   listening on the llama.cpp port. It implements the *same* slot model —
//!   including a checkpoint ring at configurable intervals and disk-backed
//!   save/restore — so the Cache-Coherence Layer's logic runs unmodified and is
//!   exercised end to end by developers who have not yet started a model server.
//!   Its simulated behaviour is always reported as simulated: the receipt and
//!   `doctor` both name the backend, so nobody mistakes a mock cache hit for a
//!   real one.
//! * [`NullBackend`] is the explicit "cache coherence is off" path. Every
//!   operation returns a capability miss, which routes the CCL down its
//!   full-re-prefill fallback — the always-correct branch the PRD requires from
//!   day one (NFR-7).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Instant;

use async_trait::async_trait;
use parking_lot::RwLock;

use crate::error::{Error, Result};
use crate::llama::{
    CapabilitySet, CheckpointKind, CheckpointRef, InferenceBackend, RestoreOutcome, SlotState,
    SnapshotOutcome,
};

/// An in-process model of a llama.cpp slot.
#[derive(Debug)]
pub struct EmbeddedBackend {
    n_ctx: i64,
    /// Spacing of the simulated checkpoint ring, matching the semantics of
    /// llama.cpp's `-cms/--checkpoint-min-step`.
    checkpoint_interval: i64,
    /// How many ring entries are retained, matching `-ctxcp/--ctx-checkpoints`.
    ring_capacity: usize,
    n_past: AtomicI64,
    checkpoint_positions: RwLock<Vec<i64>>,
    save_dir: PathBuf,
    /// Set once a caller has asked the backend to simulate a full rewrite, so
    /// the next turn is reported as a miss. Kept for API completeness of the
    /// simulation; the CCL computes the real status itself.
    rewrote: RwLock<bool>,
}

impl Default for EmbeddedBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl EmbeddedBackend {
    pub fn new() -> Self {
        Self {
            n_ctx: 32768,
            // Modelled on a llama.cpp server configured for agentic use:
            // `-cms` spaced so the ring spans the whole window, and `-ctxcp` deep
            // enough that the oldest checkpoint is still early in the session.
            //
            // The span is what matters. A ring holding only the newest few thousand
            // tokens has no checkpoint anywhere near a boundary a compactor would
            // choose, so every compaction reports a full re-prefill and the
            // cache-coherence path is never exercised — which is a realistic default
            // configuration, and exactly why Sakur4 reports the verdict rather than
            // assuming alignment is available.
            checkpoint_interval: 1024,
            ring_capacity: 32,
            n_past: AtomicI64::new(0),
            checkpoint_positions: RwLock::new(Vec::new()),
            save_dir: std::env::temp_dir().join("sakur4-embedded-snapshots"),
            rewrote: RwLock::new(false),
        }
    }

    /// Configure the simulated context window.
    pub fn with_n_ctx(mut self, n_ctx: i64) -> Self {
        self.n_ctx = n_ctx.max(1);
        self
    }

    /// Configure simulated ring spacing and depth.
    pub fn with_ring(mut self, interval: i64, capacity: usize) -> Self {
        self.checkpoint_interval = interval.max(1);
        self.ring_capacity = capacity.max(1);
        self
    }

    /// Simulate the slot having prefilled `tokens` tokens.
    ///
    /// Ring entries are regenerated the way llama.cpp would: at every multiple
    /// of the interval at or below the new position, with only the most recent
    /// `ring_capacity` retained (older entries are evicted as the ring wraps).
    pub fn advance_to(&self, tokens: i64) {
        let tokens = tokens.max(0);
        self.n_past.store(tokens, Ordering::SeqCst);
        let mut ring = Vec::new();
        let mut pos = self.checkpoint_interval;
        while pos <= tokens {
            ring.push(pos);
            pos += self.checkpoint_interval;
        }
        if ring.len() > self.ring_capacity {
            ring.drain(..ring.len() - self.ring_capacity);
        }
        *self.checkpoint_positions.write() = ring;
    }

    /// Advance by `tokens`.
    pub fn advance_by(&self, tokens: i64) {
        self.advance_to(self.n_past.load(Ordering::SeqCst) + tokens);
    }

    /// Simulate a rewrite: the KV state no longer corresponds to the token
    /// stream, so the ring is cleared exactly as a prefix-divergent request would
    /// leave it.
    pub fn simulate_rewrite(&self, retained_tokens: i64) {
        *self.rewrote.write() = true;
        self.advance_to(retained_tokens);
        self.checkpoint_positions.write().clear();
    }

    pub fn was_rewritten(&self) -> bool {
        *self.rewrote.read()
    }
}

#[async_trait]
impl InferenceBackend for EmbeddedBackend {
    fn name(&self) -> &str {
        "embedded"
    }

    fn spec(&self) -> String {
        "sakur4://embedded".into()
    }

    fn capabilities(&self) -> CapabilitySet {
        CapabilitySet {
            reachable: true,
            slots: true,
            slot_save: true,
            slot_restore: true,
            slot_erase: true,
            checkpoint_ring: true,
            tokenize: true,
            props: true,
            metrics: true,
            partial_state_only: false,
            context_shift: false,
        }
    }

    async fn probe(&self) -> Result<CapabilitySet> {
        Ok(self.capabilities())
    }

    async fn slot_state(&self, slot_id: &str) -> Result<SlotState> {
        let n_past = self.n_past.load(Ordering::SeqCst);
        Ok(SlotState {
            slot_id: slot_id.to_string(),
            n_past,
            n_ctx: self.n_ctx,
            is_processing: false,
            checkpoints: self
                .checkpoint_positions
                .read()
                .iter()
                .enumerate()
                .map(|(i, pos)| CheckpointRef {
                    id: format!("embedded-ring{i}"),
                    token_position: *pos,
                    kind: CheckpointKind::Internal,
                    size_bytes: None,
                })
                .collect(),
            prompt_eval_ms: None,
        })
    }

    async fn models(&self) -> Result<Vec<String>> {
        Ok(vec!["sakur4-embedded".into()])
    }

    async fn save_slot(&self, slot_id: &str, path: Option<&Path>) -> Result<SnapshotOutcome> {
        let started = Instant::now();
        let file = match path {
            Some(p) => p.to_path_buf(),
            None => {
                std::fs::create_dir_all(&self.save_dir)?;
                self.save_dir
                    .join(format!("slot{slot_id}-{}.json", crate::ids::uuid_v7()))
            }
        };
        if let Some(parent) = file.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let payload = serde_json::json!({
            "backend": "embedded",
            "slot_id": slot_id,
            "n_past": self.n_past.load(Ordering::SeqCst),
            "n_ctx": self.n_ctx,
            "checkpoints": *self.checkpoint_positions.read(),
            "saved_at": crate::ids::now_rfc3339(),
        });
        let bytes = serde_json::to_vec_pretty(&payload)?;
        std::fs::write(&file, &bytes)?;
        Ok(SnapshotOutcome {
            snapshot_id: crate::ids::new_id("snap"),
            file_path: Some(file.to_string_lossy().to_string()),
            size_bytes: Some(bytes.len() as u64),
            elapsed_ms: started.elapsed().as_millis() as i64,
        })
    }

    async fn restore_slot(&self, slot_id: &str, path: &Path) -> Result<RestoreOutcome> {
        let started = Instant::now();
        let raw = std::fs::read_to_string(path)?;
        let doc: serde_json::Value = serde_json::from_str(&raw)?;
        let n_past = doc.get("n_past").and_then(|v| v.as_i64()).unwrap_or(0);
        self.advance_to(n_past);
        Ok(RestoreOutcome {
            restored: true,
            restore_time_ms: started.elapsed().as_millis() as i64,
            detail: format!(
                "embedded slot {slot_id} restored to {n_past} tokens from {}",
                path.display()
            ),
        })
    }

    async fn erase_slot(&self, _slot_id: &str) -> Result<()> {
        self.advance_to(0);
        Ok(())
    }

    async fn note_compaction(&self, _slot_id: &str, retained_tokens: i64) -> Result<()> {
        // Mirror what a real server does after a prefix-divergent request: keep
        // the matching head, drop the rest, re-base the ring.
        self.advance_to(retained_tokens.max(0));
        Ok(())
    }

    async fn tokenize(&self, text: &str) -> Result<usize> {
        if text.is_empty() {
            return Ok(0);
        }
        Ok(crate::tokens::TokenEstimator::count(
            &crate::tokens::HeuristicTokenizer::default(),
            text,
        ))
    }

    async fn health(&self) -> crate::llama::BackendHealth {
        crate::llama::BackendHealth {
            name: self.name().to_string(),
            spec: self.spec(),
            capabilities: self.capabilities(),
            detail: format!(
                "simulated slot: n_ctx={}, ring every {} tokens ({} deep) — \
                 cache statuses produced against this backend are simulated",
                self.n_ctx, self.checkpoint_interval, self.ring_capacity
            ),
        }
    }
}

/// The explicit no-coherence backend.
#[derive(Debug, Default, Clone, Copy)]
pub struct NullBackend {
    _private: (),
}

impl NullBackend {
    pub fn new() -> Self {
        Self { _private: () }
    }
}

#[async_trait]
impl InferenceBackend for NullBackend {
    fn name(&self) -> &str {
        "none"
    }

    fn spec(&self) -> String {
        "none".into()
    }

    fn capabilities(&self) -> CapabilitySet {
        CapabilitySet::default()
    }

    async fn probe(&self) -> Result<CapabilitySet> {
        Ok(CapabilitySet::default())
    }

    async fn slot_state(&self, slot_id: &str) -> Result<SlotState> {
        Err(Error::BackendUnavailable(format!(
            "slot state is unavailable with --backend none (slot {slot_id})"
        )))
    }

    async fn save_slot(&self, slot_id: &str, _path: Option<&Path>) -> Result<SnapshotOutcome> {
        Err(Error::BackendUnavailable(format!(
            "slot save is unavailable with --backend none (slot {slot_id})"
        )))
    }

    async fn restore_slot(&self, slot_id: &str, _path: &Path) -> Result<RestoreOutcome> {
        Err(Error::BackendUnavailable(format!(
            "slot restore is unavailable with --backend none (slot {slot_id})"
        )))
    }

    async fn erase_slot(&self, _slot_id: &str) -> Result<()> {
        Ok(())
    }

    async fn health(&self) -> crate::llama::BackendHealth {
        crate::llama::BackendHealth {
            name: self.name().to_string(),
            spec: self.spec(),
            capabilities: CapabilitySet::default(),
            detail: "cache coherence disabled; every compaction reports full-re-prefill".into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llama::{snap_to_checkpoint, InferenceBackend};

    #[tokio::test]
    async fn ring_is_regenerated_and_capped() {
        let b = EmbeddedBackend::new().with_ring(100, 3);
        b.advance_to(1000);
        let st = b.slot_state("0").await.unwrap();
        assert_eq!(st.n_past, 1000);
        assert_eq!(
            st.checkpoints
                .iter()
                .map(|c| c.token_position)
                .collect::<Vec<_>>(),
            vec![800, 900, 1000],
            "only the most recent ring entries survive"
        );
    }

    #[tokio::test]
    async fn rewrite_clears_the_ring_like_a_prefix_divergence() {
        let b = EmbeddedBackend::new().with_ring(100, 8);
        b.advance_to(500);
        assert!(!b.slot_state("0").await.unwrap().checkpoints.is_empty());
        b.simulate_rewrite(120);
        let st = b.slot_state("0").await.unwrap();
        assert_eq!(st.n_past, 120);
        assert!(st.checkpoints.is_empty());
        assert!(b.was_rewritten());
    }

    #[tokio::test]
    async fn save_and_restore_roundtrip_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let b = EmbeddedBackend::new().with_ring(100, 8);
        b.advance_to(700);
        let out = b
            .save_slot("0", Some(&dir.path().join("s0.json")))
            .await
            .unwrap();
        assert!(out.size_bytes.unwrap() > 0);

        b.erase_slot("0").await.unwrap();
        assert_eq!(b.slot_state("0").await.unwrap().n_past, 0);

        let restored = b
            .restore_slot("0", Path::new(out.file_path.as_ref().unwrap()))
            .await
            .unwrap();
        assert!(restored.restored);
        assert_eq!(b.slot_state("0").await.unwrap().n_past, 700);
    }

    #[tokio::test]
    async fn embedded_capabilities_support_the_full_ccl_path() {
        let caps = EmbeddedBackend::new().capabilities();
        assert!(caps.can_align_boundaries());
        assert!(caps.can_restore());
        assert!(caps.can_tokenize());
        assert!(!caps.partial_state_only);
        assert!(caps.summary().contains("checkpoint-ring"));
    }

    #[tokio::test]
    async fn snap_works_against_the_embedded_ring() {
        let b = EmbeddedBackend::new().with_ring(100, 8);
        b.advance_to(5000);
        let st = b.slot_state("0").await.unwrap();
        let (aligned, reason) = snap_to_checkpoint(4300, &st.checkpoints, 512).unwrap();
        assert_eq!(aligned, 4300);
        assert_eq!(reason.delta(), 0);
    }

    #[tokio::test]
    async fn null_backend_reports_every_capability_false() {
        let caps = NullBackend::new().capabilities();
        assert!(!caps.reachable);
        assert!(!caps.can_align_boundaries());
        assert!(NullBackend::new().slot_state("0").await.is_err());
    }
}
