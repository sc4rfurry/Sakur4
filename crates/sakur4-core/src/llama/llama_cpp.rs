//! The llama.cpp backend adapter.
//!
//! Everything llama.cpp-specific lives behind this module, per the PRD's stated
//! mitigation for its highest-rated risk: "isolate all llama.cpp-specific
//! integration behind a single adapter interface with capability detection at
//! connect time".
//!
//! # Probing, not assuming
//!
//! [`LlamaCppBackend::connect`] performs an ordered probe:
//!
//! 1. `GET /health` (newer builds) or `GET /props` (older builds) — is anything
//!    there at all, and what model is it?
//! 2. `GET /slots` — slot state, `n_ctx`, and, on builds that expose it, the
//!    context-checkpoint ring.
//! 3. `GET /props` — `n_ctx` and the model architecture string, from which
//!    Sakur4 infers whether checkpoints can only carry partial state.
//! 4. `POST /tokenize` with a known canary — confirms exact token accounting.
//! 5. `GET /metrics` — prompt-eval telemetry.
//!
//! Each step is independent and tolerant: a 404 marks one capability false
//! rather than failing the connection. That is what lets the same binary drive a
//! current build with a full checkpoint ring and a two-year-old build with only
//! `/slots/{id}/save`, without a configuration flag.

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use parking_lot::RwLock;
use serde_json::Value;

use crate::error::{Error, Result};
use crate::llama::{
    CapabilitySet, CheckpointKind, CheckpointRef, InferenceBackend, RestoreOutcome, SlotState,
    SnapshotOutcome,
};

/// Architecture substrings that imply partial-state-only checkpoints.
///
/// Sliding-window and hybrid (attention + recurrent) models keep state that a
/// checkpoint cannot fully capture: llama.cpp documents that for these the
/// checkpoint holds only part of the context, so a rewind is not guaranteed to
/// reproduce the same outputs. Sakur4 detects them so it can prefer full
/// save/restore over ring rewind (C3's "detect and route around known
/// limitations" responsibility).
const PARTIAL_STATE_ARCH_HINTS: &[&str] = &[
    "swa",
    "sliding",
    "gemma3",
    "gemma-3",
    "recurrent",
    "mamba",
    "hybrid",
    "jamba",
    "qwen3next",
    "lfm2",
    "granite",
];

/// An HTTP client for a llama.cpp `llama-server`.
#[derive(Debug)]
pub struct LlamaCppBackend {
    base_url: String,
    http: reqwest::Client,
    caps: RwLock<CapabilitySet>,
    model_name: RwLock<Option<String>>,
    n_ctx_hint: RwLock<Option<i64>>,
    api_key: Option<String>,
    /// Directory for slot-save files when the caller does not supply one.
    save_dir: std::path::PathBuf,
}

impl LlamaCppBackend {
    /// Probe `base_url` and build a backend. Succeeds only if the server is
    /// reachable; individual capabilities may still be absent.
    pub async fn connect(base_url: &str, timeout: Duration) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(timeout)
            .connect_timeout(timeout.min(Duration::from_secs(3)))
            .user_agent(concat!("sakur4d/", env!("CARGO_PKG_VERSION")))
            .build()?;

        let api_key = std::env::var("SAKUR4_LLAMA_API_KEY").ok().filter(|s| !s.trim().is_empty());

        let save_dir = std::env::var("SAKUR4_SNAPSHOT_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| std::env::temp_dir().join("sakur4-snapshots"));

        let backend = Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            http,
            caps: RwLock::new(CapabilitySet::default()),
            model_name: RwLock::new(None),
            n_ctx_hint: RwLock::new(None),
            api_key,
            save_dir,
        };

        let caps = backend.probe().await?;
        if !caps.reachable {
            return Err(Error::BackendUnavailable(format!(
                "no llama.cpp server responded at {base_url}"
            )));
        }
        Ok(backend)
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// The loaded model's name as reported by the server, when known.
    pub fn model_name(&self) -> Option<String> {
        self.model_name.read().clone()
    }

    /// The context window reported by the server, when known.
    pub fn n_ctx(&self) -> Option<i64> {
        *self.n_ctx_hint.read()
    }

    /// Directory where slot-save files are written when no path is supplied.
    pub fn save_dir(&self) -> &std::path::Path {
        &self.save_dir
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }

    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        let mut rb = self.http.request(method, self.url(path));
        if let Some(key) = &self.api_key {
            rb = rb.bearer_auth(key);
        }
        rb
    }

    /// `GET` returning parsed JSON, mapping every transport problem to a
    /// capability miss rather than an error (the caller decides severity).
    async fn get_json(&self, path: &str) -> Option<Value> {
        let resp = self.request(reqwest::Method::GET, path).send().await.ok()?;
        if !resp.status().is_success() {
            return None;
        }
        resp.json::<Value>().await.ok()
    }

    async fn post_json(&self, path: &str, body: Value) -> Result<(bool, Value)> {
        let resp = self.request(reqwest::Method::POST, path).json(&body).send().await?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        let parsed: Value = serde_json::from_str(&text).unwrap_or(Value::String(text.clone()));
        Ok((status.is_success(), parsed))
    }
}

#[async_trait]
impl InferenceBackend for LlamaCppBackend {
    fn name(&self) -> &str {
        "llama.cpp"
    }

    fn spec(&self) -> String {
        self.base_url.clone()
    }

    fn capabilities(&self) -> CapabilitySet {
        self.caps.read().clone()
    }

    async fn probe(&self) -> Result<CapabilitySet> {
        let mut caps = CapabilitySet::default();

        // --- 1. liveness + identity -----------------------------------------
        let health = self.get_json("/health").await;
        let props = self.get_json("/props").await;
        caps.props = props.is_some();
        let slots_doc = self.get_json("/slots").await;
        caps.slots = slots_doc.as_ref().is_some_and(|v| v.is_array());
        caps.reachable = health.is_some() || props.is_some() || caps.slots;

        if !caps.reachable {
            *self.caps.write() = caps.clone();
            return Ok(caps);
        }

        // --- 2. model identity and architecture ------------------------------
        let arch = props
            .as_ref()
            .and_then(|p| p.get("model_path").or_else(|| p.get("model")).and_then(|v| v.as_str()))
            .map(|s| s.to_lowercase())
            .or_else(|| {
                slots_doc.as_ref().and_then(|s| {
                    s.as_array()
                        .and_then(|a| a.first())
                        .and_then(|s0| s0.get("model"))
                        .and_then(|m| m.as_str())
                        .map(|s| s.to_lowercase())
                })
            })
            .unwrap_or_default();

        if !arch.is_empty() {
            *self.model_name.write() = Some(arch.clone());
            caps.partial_state_only =
                PARTIAL_STATE_ARCH_HINTS.iter().any(|hint| arch.contains(hint));
        }

        if let Some(n_ctx) =
            props.as_ref().and_then(|p| p.get("n_ctx")).and_then(|v| v.as_i64()).or_else(|| {
                slots_doc
                    .as_ref()
                    .and_then(|s| s.as_array())
                    .and_then(|a| a.first())
                    .and_then(|s0| s0.get("n_ctx"))
                    .and_then(|v| v.as_i64())
            })
        {
            *self.n_ctx_hint.write() = Some(n_ctx);
        }

        // --- 3. checkpoint ring ---------------------------------------------
        // Builds that expose the ring report it per slot. The field name has
        // moved between revisions, so accept every spelling seen in the wild.
        caps.checkpoint_ring = slots_doc
            .as_ref()
            .and_then(|s| s.as_array())
            .and_then(|a| a.first())
            .map(|slot| {
                ["checkpoints", "ctx_checkpoints", "context_checkpoints", "ckpt"]
                    .iter()
                    .any(|k| slot.get(*k).is_some_and(|v| v.is_array()))
            })
            .unwrap_or(false);

        // A ring implies the server-side save/restore verbs exist; otherwise
        // treat them as absent until a probe confirms the route, because on some
        // builds the route exists but the handler needs a compiled-in flag.
        caps.slot_save = caps.checkpoint_ring;
        caps.slot_restore = caps.checkpoint_ring;
        caps.slot_erase = caps.slots;

        // --- 4. exact tokenization ------------------------------------------
        if let Ok((ok, body)) =
            self.post_json("/tokenize", serde_json::json!({"content": "Sakur4 probe."})).await
        {
            caps.tokenize = ok && body.get("tokens").is_some_and(|t| t.is_array());
        }

        // --- 5. telemetry ----------------------------------------------------
        caps.metrics = self.get_json("/metrics").await.is_some();

        *self.caps.write() = caps.clone();
        tracing::info!(
            base_url = %self.base_url,
            model = ?self.model_name.read(),
            n_ctx = ?self.n_ctx_hint.read(),
            summary = %caps.summary(),
            "probed llama.cpp backend"
        );
        Ok(caps)
    }

    async fn slot_state(&self, slot_id: &str) -> Result<SlotState> {
        let doc = self
            .get_json("/slots")
            .await
            .ok_or_else(|| Error::BackendUnavailable("GET /slots unavailable".into()))?;
        let array = doc
            .as_array()
            .ok_or_else(|| Error::BackendUnavailable("/slots did not return an array".into()))?;

        let wanted = slot_id.parse::<usize>().ok();
        let slot = array
            .iter()
            .find(|s| {
                wanted
                    .and_then(|w| s.get("id").and_then(|i| i.as_i64()).map(|i| i as usize == w))
                    .unwrap_or(false)
            })
            // A single-slot server may not report ids at all.
            .or_else(|| array.first())
            .ok_or_else(|| Error::NotFound(format!("slot {slot_id}")))?;

        let n_ctx =
            slot.get("n_ctx").and_then(|v| v.as_i64()).or(*self.n_ctx_hint.read()).unwrap_or(0);
        let n_past = slot
            .get("n_past")
            .and_then(|v| v.as_i64())
            .or_else(|| {
                slot.get("prompt_tokens")
                    .and_then(|v| v.as_i64())
                    .map(|p| p + slot.get("n_decoded").and_then(|v| v.as_i64()).unwrap_or(0))
            })
            .unwrap_or(0);

        let checkpoints = parse_checkpoints(slot);

        Ok(SlotState {
            slot_id: slot_id.to_string(),
            n_past,
            n_ctx,
            is_processing: slot.get("is_processing").and_then(|v| v.as_bool()).unwrap_or(false),
            checkpoints,
            prompt_eval_ms: slot
                .get("prompt_eval_ms")
                .or_else(|| slot.get("t_prompt_eval_ms"))
                .and_then(|v| v.as_i64()),
        })
    }

    async fn models(&self) -> Result<Vec<String>> {
        let doc = self
            .get_json("/v1/models")
            .await
            .ok_or_else(|| Error::BackendUnavailable("GET /v1/models unavailable".into()))?;
        Ok(doc
            .get("data")
            .and_then(|d| d.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|m| m.get("id").and_then(|i| i.as_str()).map(String::from))
                    .collect()
            })
            .unwrap_or_default())
    }

    async fn save_slot(
        &self,
        slot_id: &str,
        path: Option<&std::path::Path>,
    ) -> Result<SnapshotOutcome> {
        let caps = self.capabilities();
        if !caps.reachable {
            return Err(Error::BackendUnavailable("backend unreachable".into()));
        }

        let file = match path {
            Some(p) => p.to_path_buf(),
            None => {
                std::fs::create_dir_all(&self.save_dir)?;
                self.save_dir.join(format!("slot{slot_id}-{}.bin", crate::ids::uuid_v7()))
            }
        };
        let file_str = file.to_string_lossy().to_string();

        let started = Instant::now();
        // Revisions have differed on the payload key; try both before failing.
        let mut last_error = String::new();
        for key in ["filename", "path"] {
            match self
                .post_json(&format!("/slots/{slot_id}/save"), serde_json::json!({ key: file_str }))
                .await
            {
                Ok((true, body)) => {
                    let actual = body
                        .get("filename")
                        .or_else(|| body.get("path"))
                        .and_then(|v| v.as_str())
                        .map(String::from)
                        .unwrap_or_else(|| file_str.clone());
                    let size = body
                        .get("size")
                        .or_else(|| body.get("n_bytes"))
                        .and_then(|v| v.as_u64())
                        .or_else(|| std::fs::metadata(&actual).ok().map(|m| m.len()));
                    return Ok(SnapshotOutcome {
                        snapshot_id: crate::ids::new_id("snap"),
                        file_path: Some(actual),
                        size_bytes: size,
                        elapsed_ms: started.elapsed().as_millis() as i64,
                    });
                }
                Ok((false, body)) => {
                    last_error = body.to_string();
                }
                Err(e) => last_error = e.to_string(),
            }
        }
        Err(Error::BackendUnavailable(format!(
            "POST /slots/{slot_id}/save failed on this build: {last_error}"
        )))
    }

    async fn restore_slot(&self, slot_id: &str, path: &std::path::Path) -> Result<RestoreOutcome> {
        let started = Instant::now();
        let file_str = path.to_string_lossy().to_string();
        let mut last_error = String::new();
        for key in ["filename", "path"] {
            match self
                .post_json(
                    &format!("/slots/{slot_id}/restore"),
                    serde_json::json!({ key: file_str }),
                )
                .await
            {
                Ok((true, body)) => {
                    let tokens = body.get("n_past").and_then(|v| v.as_i64());
                    return Ok(RestoreOutcome {
                        restored: true,
                        restore_time_ms: started.elapsed().as_millis() as i64,
                        detail: match tokens {
                            Some(n) => format!("restored {n} tokens of KV state from {file_str}"),
                            None => format!("restored KV state from {file_str}"),
                        },
                    });
                }
                Ok((false, body)) => last_error = body.to_string(),
                Err(e) => last_error = e.to_string(),
            }
        }
        Err(Error::BackendUnavailable(format!(
            "POST /slots/{slot_id}/restore failed on this build: {last_error}"
        )))
    }

    async fn erase_slot(&self, slot_id: &str) -> Result<()> {
        let (ok, body) =
            self.post_json(&format!("/slots/{slot_id}/erase"), serde_json::json!({})).await?;
        if ok {
            Ok(())
        } else {
            Err(Error::BackendUnavailable(format!("POST /slots/{slot_id}/erase failed: {body}")))
        }
    }

    async fn tokenize(&self, text: &str) -> Result<usize> {
        let (ok, body) = self.post_json("/tokenize", serde_json::json!({"content": text})).await?;
        if !ok {
            return Err(Error::BackendUnavailable("POST /tokenize failed".into()));
        }
        body.get("tokens")
            .and_then(|t| t.as_array())
            .map(|a| a.len())
            .ok_or_else(|| Error::BackendUnavailable("/tokenize returned no token array".into()))
    }
}

/// Parse whatever checkpoint representation a build exposes into token positions.
///
/// Accepted shapes:
/// * `[100, 900, 1200]` — bare positions
/// * `[{"pos": 900, "size": 1234}]`
/// * `[{"token_position": 900}]`
/// * `{"0": 900, "1": 1200}` — index-keyed map
fn parse_checkpoints(slot: &Value) -> Vec<CheckpointRef> {
    let raw = ["checkpoints", "ctx_checkpoints", "context_checkpoints", "ckpt"]
        .iter()
        .find_map(|k| slot.get(*k));
    let Some(raw) = raw else {
        return Vec::new();
    };

    let mut out: Vec<CheckpointRef> = Vec::new();
    let mut push = |idx: usize, pos: i64, size: Option<u64>| {
        if pos > 0 {
            out.push(CheckpointRef {
                id: format!("ring{idx}"),
                token_position: pos,
                kind: CheckpointKind::Internal,
                size_bytes: size,
            });
        }
    };

    match raw {
        Value::Array(items) => {
            for (idx, item) in items.iter().enumerate() {
                match item {
                    Value::Number(n) => {
                        if let Some(pos) = n.as_i64() {
                            push(idx, pos, None);
                        }
                    }
                    Value::Object(obj) => {
                        let pos = ["token_position", "pos", "n_past", "position"]
                            .iter()
                            .find_map(|k| obj.get(*k).and_then(|v| v.as_i64()));
                        let size =
                            obj.get("size").or_else(|| obj.get("n_bytes")).and_then(|v| v.as_u64());
                        if let Some(pos) = pos {
                            push(idx, pos, size);
                        }
                    }
                    _ => {}
                }
            }
        }
        Value::Object(map) => {
            for (idx, (_, v)) in map.iter().enumerate() {
                let pos = v
                    .as_i64()
                    .or_else(|| v.get("token_position").and_then(|x| x.as_i64()))
                    .or_else(|| v.get("pos").and_then(|x| x.as_i64()));
                if let Some(pos) = pos {
                    push(idx, pos, v.get("size").and_then(|x| x.as_u64()));
                }
            }
        }
        _ => {}
    }

    // The ring is not guaranteed to arrive ordered; the CCL's snapping logic
    // assumes ascending positions.
    out.sort_by_key(|c| c.token_position);
    out
}

/// Convenience: resolve `SAKUR4_LLAMA_URL` or the given default into a backend,
/// returning `None` instead of an error when nothing is listening.
pub async fn try_connect(
    base_url: Option<&str>,
    timeout: Duration,
) -> Option<Arc<LlamaCppBackend>> {
    let url = base_url
        .map(String::from)
        .or_else(|| std::env::var("SAKUR4_LLAMA_URL").ok())
        .unwrap_or_else(|| "http://127.0.0.1:8080".to_string());
    LlamaCppBackend::connect(&url, timeout)
        .await
        .map(Arc::new)
        .map_err(|e| {
            tracing::debug!(url = %url, error = %e, "llama.cpp not available");
            e
        })
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_bare_position_arrays() {
        let slot = json!({"checkpoints": [100, 900, 1200]});
        let cps = parse_checkpoints(&slot);
        assert_eq!(cps.len(), 3);
        assert_eq!(cps[0].token_position, 100);
        assert_eq!(cps[0].kind, CheckpointKind::Internal);
    }

    #[test]
    fn parses_object_arrays_and_sorts() {
        let slot = json!({"ctx_checkpoints": [
            {"pos": 1200, "size": 4096},
            {"pos": 100},
            {"pos": 900}
        ]});
        let cps = parse_checkpoints(&slot);
        assert_eq!(cps.iter().map(|c| c.token_position).collect::<Vec<_>>(), vec![100, 900, 1200]);
        assert_eq!(cps[2].size_bytes, Some(4096));
    }

    #[test]
    fn parses_index_keyed_maps() {
        let slot = json!({"checkpoints": {"0": 500, "1": 1500}});
        let cps = parse_checkpoints(&slot);
        assert_eq!(cps.iter().map(|c| c.token_position).collect::<Vec<_>>(), vec![500, 1500]);
    }

    #[test]
    fn ignores_zero_and_missing_positions() {
        let slot = json!({"checkpoints": [0, {"pos": 0}, {"other": 1}, 700]});
        let cps = parse_checkpoints(&slot);
        assert_eq!(cps.len(), 1);
        assert_eq!(cps[0].token_position, 700);
    }

    #[test]
    fn tolerates_absent_ring() {
        assert!(parse_checkpoints(&json!({})).is_empty());
        assert!(parse_checkpoints(&json!({"checkpoints": "nonsense"})).is_empty());
    }
}
