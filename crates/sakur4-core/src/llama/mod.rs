//! The pluggable inference-backend layer (PRD component C3's foundation).
//!
//! # Why this is a trait and not a client
//!
//! The PRD's own risk register calls out that llama.cpp's slot/checkpoint API
//! "is built against a moving, architecture-dependent target": endpoint names
//! have been renamed, checkpoint flags were removed and replaced, and SWA/hybrid
//! models only support partial state. A Cache-Coherence Layer written directly
//! against one build of one server would be correct for exactly that build.
//!
//! Sakur4 therefore never assumes. It **probes** whatever is at the configured
//! base URL, records a [`CapabilitySet`], and routes every cache-related decision
//! through the capability set. The same binary then behaves correctly as:
//!
//! | Configured target | Resolution | Cache coherence |
//! |---|---|---|
//! | `auto` + a llama.cpp server on localhost | [`LlamaCppBackend`] | full |
//! | `auto` + nothing listening | [`EmbeddedBackend`] (in-process mock) | full, simulated |
//! | explicit `sakur4://embedded` | [`EmbeddedBackend`] | full, simulated |
//! | explicit `none` | [`NullBackend`] | disabled; full re-prefill fallback |
//! | a remote host on the LAN | [`LlamaCppBackend`] | full |
//! | an older build with no `/slots` | [`LlamaCppBackend`] with fewer capabilities | degraded, never failed |
//!
//! The last two rows are NFR-7 ("degrades gracefully ... never a hard failure")
//! expressed as a type rather than as a hope.

pub mod embedded;
pub mod llama_cpp;
pub mod mock;

use std::sync::Arc;

use async_trait::async_trait;

use crate::error::{Error, Result};

/// Where the Cache-Coherence Layer should look for an inference backend.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case", untagged)]
#[derive(Default)]
pub enum BackendSpec {
    /// `auto` — probe, then fall back to the embedded backend.
    #[default]
    Auto,
    /// Use the in-process embedded backend without probing anything.
    Embedded,
    /// No backend: every coherence operation degrades to a logged no-op.
    None,
    /// An explicit HTTP base URL, e.g. `http://127.0.0.1:8080`.
    Url(String),
}


impl BackendSpec {
    /// Parse the `--backend` / `SAKUR4_BACKEND` form.
    pub fn parse(raw: &str) -> Self {
        match raw.trim() {
            "" | "auto" => BackendSpec::Auto,
            "embedded" | "sakur4://embedded" | "mock" => BackendSpec::Embedded,
            "none" | "off" | "disabled" => BackendSpec::None,
            other => BackendSpec::Url(other.trim_end_matches('/').to_string()),
        }
    }
}

impl std::fmt::Display for BackendSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BackendSpec::Auto => write!(f, "auto"),
            BackendSpec::Embedded => write!(f, "embedded"),
            BackendSpec::None => write!(f, "none"),
            BackendSpec::Url(u) => write!(f, "{u}"),
        }
    }
}

/// What a backend can actually do, determined by probing rather than assumption.
///
/// Every field defaults to `false`, so a backend that fails detection can never
/// accidentally be treated as capable: the failure mode of a missing probe is
/// "we do a full re-prefill", which is always correct and sometimes slow.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct CapabilitySet {
    /// The backend answered at all.
    pub reachable: bool,
    /// `/slots` (GET) is available — slot list, `n_ctx`, `n_past`, and on newer
    /// builds the checkpoint ring state.
    pub slots: bool,
    /// `POST /slots/{id}/save` persists KV + recurrent state to disk.
    pub slot_save: bool,
    /// `POST /slots/{id}/restore` reloads persisted state.
    pub slot_restore: bool,
    /// `POST /slots/{id}/erase` drops a slot's state.
    pub slot_erase: bool,
    /// The build reports context checkpoints (the `-cms`/`-ctxcp` ring), which
    /// is what makes sub-prefix rewind possible without a disk round trip.
    pub checkpoint_ring: bool,
    /// `POST /tokenize` is available, enabling exact token accounting.
    pub tokenize: bool,
    /// `GET /props` is available, giving `n_ctx` and the model name.
    pub props: bool,
    /// `GET /metrics` is available, giving prompt-eval timings.
    pub metrics: bool,
    /// The loaded model is a sliding-window or hybrid-attention architecture,
    /// where checkpoints capture only partial state.
    pub partial_state_only: bool,
    /// Server switches slots on prefix divergence (SWA/hybrid with
    /// `--swa-full`-style handling), so an LCP mismatch may reset the whole slot.
    pub context_shift: bool,
}

impl CapabilitySet {
    /// The honest one-line summary shown by `doctor` and in receipts.
    pub fn summary(&self) -> String {
        if !self.reachable {
            return "unreachable".into();
        }
        let mut bits = Vec::new();
        if self.slots {
            bits.push("slots");
        }
        if self.slot_save {
            bits.push("save");
        }
        if self.slot_restore {
            bits.push("restore");
        }
        if self.checkpoint_ring {
            bits.push("checkpoint-ring");
        }
        if self.tokenize {
            bits.push("tokenize");
        }
        if self.metrics {
            bits.push("metrics");
        }
        if self.partial_state_only {
            bits.push("PARTIAL-STATE-ONLY");
        }
        if bits.is_empty() {
            "reachable, no coherence endpoints".into()
        } else {
            bits.join("+")
        }
    }

    /// Whether checkpoint-aligned eviction boundaries (FR-7) can be attempted.
    pub fn can_align_boundaries(&self) -> bool {
        self.reachable && self.slots && (self.checkpoint_ring || self.slot_save)
    }

    /// Whether a warm restore (FR-8) is possible.
    pub fn can_restore(&self) -> bool {
        self.reachable && (self.slot_restore || self.slot_save)
    }

    /// Whether exact token accounting is possible.
    pub fn can_tokenize(&self) -> bool {
        self.reachable && self.tokenize
    }
}

/// Live state of one inference slot.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct SlotState {
    pub slot_id: String,
    /// Tokens currently resident in the slot's KV cache.
    pub n_past: i64,
    /// Tokens the slot can hold.
    pub n_ctx: i64,
    /// Slot is mid-generation.
    pub is_processing: bool,
    /// The build's exposed checkpoint ring, oldest first. Empty when the build
    /// does not report it (still usable: the CCL then relies on save points).
    pub checkpoints: Vec<CheckpointRef>,
    /// The server's own record of the last prompt-eval, if exposed.
    pub prompt_eval_ms: Option<i64>,
}

impl SlotState {
    /// The boundary at or before `position` closest to it — the snap target for
    /// FR-7. Returns `None` when no checkpoint is a usable candidate.
    pub fn nearest_checkpoint_at_or_before(&self, position: i64) -> Option<&CheckpointRef> {
        self.checkpoints
            .iter()
            .filter(|c| c.token_position <= position)
            .max_by_key(|c| c.token_position)
    }

    /// Fraction of the slot's window currently occupied.
    pub fn fill_ratio(&self) -> f64 {
        if self.n_ctx <= 0 {
            0.0
        } else {
            (self.n_past as f64 / self.n_ctx as f64).clamp(0.0, 1.0)
        }
    }
}

/// A checkpoint the backend knows about, in the *token* domain.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct CheckpointRef {
    /// Backend-native identifier (may be an index for an internal ring entry or
    /// a filename for a slot-save file).
    pub id: String,
    pub token_position: i64,
    pub kind: CheckpointKind,
    pub size_bytes: Option<u64>,
}

/// Distinguishes the two mechanisms the CCL reasons about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckpointKind {
    /// An in-memory ring entry (`-ctxcp`). Cheap to rewind to, goes away when
    /// the process restarts or the ring wraps.
    Internal,
    /// A file written by `/slots/{id}/save`. Survives restarts, costs disk.
    SlotSaveFile,
    /// A marker Sakur4 itself wrote when opening a fold.
    FoldMarker,
    /// A save taken immediately before an unavoidable rewrite.
    PreRewrite,
}

/// Result of a snapshot (save) request.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SnapshotOutcome {
    pub snapshot_id: String,
    pub file_path: Option<String>,
    pub size_bytes: Option<u64>,
    pub elapsed_ms: i64,
}

/// Result of a restore request.
#[derive(Debug, Clone, serde::Serialize)]
pub struct RestoreOutcome {
    pub restored: bool,
    pub restore_time_ms: i64,
    pub detail: String,
}

/// Backend health, for `doctor` and for the receipt's cache-status field.
#[derive(Debug, Clone, serde::Serialize)]
pub struct BackendHealth {
    pub name: String,
    pub spec: String,
    pub capabilities: CapabilitySet,
    pub detail: String,
}

/// The abstraction every inference backend implements.
///
/// Note what is *not* here: prompt assembly, sampling, chat templating. Sakur4
/// is a subsystem, not a harness (NG1) — it only needs the state-management
/// primitives that make compaction cheap.
#[async_trait]
pub trait InferenceBackend: Send + Sync + std::fmt::Debug {
    /// Stable short name, e.g. `llama.cpp` or `embedded`.
    fn name(&self) -> &str;

    /// The spec this backend was resolved from, for display.
    fn spec(&self) -> String;

    /// What this backend can do. Cached after the first probe.
    fn capabilities(&self) -> CapabilitySet;

    /// Re-run capability detection (used by `doctor --refresh`).
    async fn probe(&self) -> Result<CapabilitySet>;

    /// Live slot state, including the checkpoint ring when the build reports it.
    async fn slot_state(&self, slot_id: &str) -> Result<SlotState>;

    /// OpenAI-compatible model list; empty when unavailable.
    async fn models(&self) -> Result<Vec<String>> {
        Ok(Vec::new())
    }

    /// Persist a slot's KV + recurrent state. `path` is a hint; backends that
    /// manage their own storage return the real path in the outcome.
    async fn save_slot(&self, slot_id: &str, path: Option<&std::path::Path>) -> Result<SnapshotOutcome>;

    /// Reload previously persisted slot state.
    async fn restore_slot(&self, slot_id: &str, path: &std::path::Path) -> Result<RestoreOutcome>;

    /// Discard a slot's state so the next request starts clean.
    async fn erase_slot(&self, slot_id: &str) -> Result<()>;

    /// Take note that a compaction retained only `retained_tokens` of the slot's
    /// context.
    ///
    /// A real llama.cpp slot does this on its own: a request whose prompt diverges
    /// from the cached prefix causes the server to keep the matching head and drop
    /// the rest, and its checkpoint ring is re-based on what remains. The default
    /// implementation is therefore a no-op — there is nothing for a client to tell
    /// a real server. Backends that *simulate* a server use it to stay honest, so
    /// the coherence logic is exercised against the cache state a real server
    /// would actually be in rather than one it never reaches.
    async fn note_compaction(&self, slot_id: &str, retained_tokens: i64) -> Result<()> {
        let _ = (slot_id, retained_tokens);
        Ok(())
    }

    /// Exact token count for the loaded model, when the backend can do it.
    async fn tokenize(&self, text: &str) -> Result<usize> {
        let _ = text;
        Err(Error::BackendUnavailable(format!(
            "{} cannot tokenize",
            self.name()
        )))
    }

    /// Health summary for `doctor`.
    async fn health(&self) -> BackendHealth {
        BackendHealth {
            name: self.name().to_string(),
            spec: self.spec(),
            capabilities: self.capabilities(),
            detail: self.capabilities().summary(),
        }
    }
}

/// A resolved backend plus the provenance of how it was chosen.
#[derive(Debug, Clone)]
pub struct ResolvedBackend {
    pub backend: Arc<dyn InferenceBackend>,
    /// The spec the operator asked for.
    pub requested: BackendSpec,
    /// What actually got used, and why — printed at startup and by `doctor`.
    pub resolution_note: String,
}

/// Resolve a [`BackendSpec`] into a live backend.
///
/// This is the single place where "works dynamically" is implemented. `auto`
/// probes the configured base URL with a short timeout and falls back to the
/// embedded backend, so a developer with no llama.cpp running still gets a fully
/// functional Sakur4 with *observably simulated* cache behaviour rather than a
/// crash or a silently disabled subsystem.
pub async fn resolve(spec: &BackendSpec, timeout: std::time::Duration) -> ResolvedBackend {
    let default_local = "http://127.0.0.1:8080";

    match spec {
        BackendSpec::None => {
            let backend = Arc::new(embedded::NullBackend::new()) as Arc<dyn InferenceBackend>;
            ResolvedBackend {
                backend,
                requested: spec.clone(),
                resolution_note: "cache coherence disabled by configuration; \
                                  every compaction takes the full-re-prefill path (NFR-7)"
                    .into(),
            }
        }
        BackendSpec::Embedded => {
            let backend = Arc::new(embedded::EmbeddedBackend::new());
            ResolvedBackend {
                backend,
                requested: spec.clone(),
                resolution_note: "using the embedded backend (no external server required)".into(),
            }
        }
        BackendSpec::Url(url) => match llama_cpp::LlamaCppBackend::connect(url, timeout).await {
            Ok(b) => {
                let caps = b.capabilities();
                ResolvedBackend {
                    backend: Arc::new(b),
                    requested: spec.clone(),
                    resolution_note: format!("connected to {url} ({})", caps.summary()),
                }
            }
            Err(e) => {
                tracing::warn!(url = %url, error = %e, "explicit backend unreachable");
                let backend = Arc::new(embedded::EmbeddedBackend::new());
                ResolvedBackend {
                    backend,
                    requested: spec.clone(),
                    resolution_note: format!(
                        "requested {url} was unreachable ({e}); degraded to the embedded backend"
                    ),
                }
            }
        },
        BackendSpec::Auto => {
            let candidate = std::env::var("SAKUR4_LLAMA_URL")
                .ok()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| default_local.to_string());
            match llama_cpp::LlamaCppBackend::connect(&candidate, timeout).await {
                Ok(b) => {
                    let caps = b.capabilities();
                    ResolvedBackend {
                        backend: Arc::new(b),
                        requested: spec.clone(),
                        resolution_note: format!("auto-detected llama.cpp at {candidate} ({})", caps.summary()),
                    }
                }
                Err(e) => {
                    tracing::info!(
                        candidate = %candidate,
                        error = %e,
                        "no llama.cpp server detected; using the embedded backend"
                    );
                    let backend = Arc::new(embedded::EmbeddedBackend::new());
                    ResolvedBackend {
                        backend,
                        requested: spec.clone(),
                        resolution_note: format!(
                            "no llama.cpp server at {candidate} ({e}); using the embedded backend"
                        ),
                    }
                }
            }
        }
    }
}

/// Snap a proposed eviction cut point onto an existing checkpoint boundary.
///
/// This is FR-7 in one function. The rule: choose the *latest* checkpoint at or
/// before the requested cut, provided the gap is inside `tolerance`. Choosing
/// "at or before" is what preserves the PRD's core property — the surviving
/// prefix must still be a prefix of what the KV cache holds, so the next turn is
/// an LCP match rather than a full re-prefill. A checkpoint *after* the cut would
/// mean re-prefilling the difference anyway, so it is not a candidate.
///
/// Returns the aligned position and the reason, or `None` when no checkpoint is
/// close enough and the caller should fall back.
pub fn snap_to_checkpoint(
    requested_cut: i64,
    checkpoints: &[CheckpointRef],
    tolerance: i64,
) -> Option<(i64, SnapReason)> {
    if requested_cut <= 0 || checkpoints.is_empty() {
        return None;
    }
    let best = checkpoints
        .iter()
        .filter(|c| c.token_position > 0 && c.token_position <= requested_cut)
        .max_by_key(|c| c.token_position)?;

    let delta = requested_cut - best.token_position;
    if delta > tolerance.max(0) {
        return None;
    }
    let reason = match best.kind {
        CheckpointKind::Internal => SnapReason::InternalCheckpoint { delta },
        CheckpointKind::SlotSaveFile => SnapReason::SlotSaveFile { delta },
        CheckpointKind::FoldMarker => SnapReason::FoldMarker { delta },
        CheckpointKind::PreRewrite => SnapReason::PreRewrite { delta },
    };
    Some((best.token_position, reason))
}

/// Why a cut point moved.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum SnapReason {
    /// Snapped back onto an in-memory ring checkpoint.
    InternalCheckpoint { delta: i64 },
    /// Snapped back onto a disk checkpoint.
    SlotSaveFile { delta: i64 },
    /// Snapped back onto a fold-open marker.
    FoldMarker { delta: i64 },
    /// Snapped back onto a pre-rewrite save.
    PreRewrite { delta: i64 },
}

impl SnapReason {
    pub fn delta(&self) -> i64 {
        match self {
            SnapReason::InternalCheckpoint { delta }
            | SnapReason::SlotSaveFile { delta }
            | SnapReason::FoldMarker { delta }
            | SnapReason::PreRewrite { delta } => *delta,
        }
    }

    pub fn describe(&self) -> String {
        match self {
            SnapReason::InternalCheckpoint { delta } => {
                format!("snapped {delta} tokens back onto an in-memory checkpoint")
            }
            SnapReason::SlotSaveFile { delta } => {
                format!("snapped {delta} tokens back onto a saved slot file")
            }
            SnapReason::FoldMarker { delta } => {
                format!("snapped {delta} tokens back onto a fold marker")
            }
            SnapReason::PreRewrite { delta } => {
                format!("snapped {delta} tokens back onto a pre-rewrite save")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cps(positions: &[(i64, CheckpointKind)]) -> Vec<CheckpointRef> {
        positions
            .iter()
            .enumerate()
            .map(|(i, (p, k))| CheckpointRef {
                id: format!("c{i}"),
                token_position: *p,
                kind: *k,
                size_bytes: None,
            })
            .collect()
    }

    #[test]
    fn spec_parsing_covers_every_documented_form() {
        assert_eq!(BackendSpec::parse("auto"), BackendSpec::Auto);
        assert_eq!(BackendSpec::parse(""), BackendSpec::Auto);
        assert_eq!(BackendSpec::parse("embedded"), BackendSpec::Embedded);
        assert_eq!(BackendSpec::parse("sakur4://embedded"), BackendSpec::Embedded);
        assert_eq!(BackendSpec::parse("none"), BackendSpec::None);
        assert_eq!(
            BackendSpec::parse("http://10.0.0.5:8080/"),
            BackendSpec::Url("http://10.0.0.5:8080".into())
        );
    }

    #[test]
    fn default_capabilities_are_pessimistic() {
        let caps = CapabilitySet::default();
        assert!(!caps.reachable);
        assert!(!caps.can_align_boundaries());
        assert!(!caps.can_restore());
        assert_eq!(caps.summary(), "unreachable");
    }

    #[test]
    fn snap_prefers_latest_checkpoint_at_or_before() {
        let ring = cps(&[
            (100, CheckpointKind::Internal),
            (900, CheckpointKind::Internal),
            (1200, CheckpointKind::Internal),
        ]);
        let (pos, reason) = snap_to_checkpoint(1000, &ring, 512).unwrap();
        assert_eq!(pos, 900);
        assert_eq!(reason.delta(), 100);
        assert_eq!(reason, SnapReason::InternalCheckpoint { delta: 100 });
    }

    #[test]
    fn snap_refuses_when_the_gap_exceeds_tolerance() {
        let ring = cps(&[(100, CheckpointKind::Internal), (900, CheckpointKind::Internal)]);
        // 900 is the latest checkpoint at or before each of these cuts.
        assert!(snap_to_checkpoint(2000, &ring, 512).is_none(), "gap 1100 > 512");
        assert!(snap_to_checkpoint(1413, &ring, 512).is_none(), "gap 513 > 512");
        // Exactly at the tolerance is inside it: the boundary is inclusive, so a
        // 512-token budget buys a full 512-token snap.
        assert!(snap_to_checkpoint(1412, &ring, 512).is_some(), "gap 512 == 512");
        assert!(snap_to_checkpoint(1400, &ring, 512).is_some(), "gap 500 < 512");
    }

    #[test]
    fn snap_never_selects_a_checkpoint_after_the_cut() {
        // A checkpoint beyond the cut would mean re-prefilling the gap, which
        // defeats the purpose; the correct answer is the earlier one.
        let ring = cps(&[(800, CheckpointKind::Internal), (1100, CheckpointKind::Internal)]);
        let (pos, _) = snap_to_checkpoint(1000, &ring, 512).unwrap();
        assert_eq!(pos, 800);
    }

    #[test]
    fn snap_requires_a_usable_candidate() {
        assert!(snap_to_checkpoint(1000, &[], 512).is_none());
        assert!(snap_to_checkpoint(0, &cps(&[(10, CheckpointKind::Internal)]), 512).is_none());
        // Only a zero-position checkpoint exists: nothing to snap back onto.
        assert!(snap_to_checkpoint(100, &cps(&[(0, CheckpointKind::Internal)]), 512).is_none());
    }

    #[test]
    fn slot_fill_ratio_is_clamped() {
        let s = SlotState {
            n_past: 90,
            n_ctx: 100,
            ..Default::default()
        };
        assert!((s.fill_ratio() - 0.9).abs() < 1e-9);
        let over = SlotState {
            n_past: 150,
            n_ctx: 100,
            ..Default::default()
        };
        assert_eq!(over.fill_ratio(), 1.0);
        let zero = SlotState::default();
        assert_eq!(zero.fill_ratio(), 0.0);
    }
}
