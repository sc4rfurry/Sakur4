//! An in-process HTTP server that speaks enough of llama.cpp's server API to
//! exercise the Cache-Coherence Layer over the wire.
//!
//! # Why this exists in addition to the embedded backend
//!
//! `sakur4-core`'s embedded backend implements the same *trait*, which is the
//! right level for unit tests of the coherence logic. But the production
//! `LlamaCppBackend` is a separate thing — it parses
//! JSON shapes, tolerates renamed fields, tries alternate payload keys, and maps
//! HTTP failures onto capability misses. None of that is exercised by a trait
//! mock, and all of it is exactly what the PRD's top-rated risk warns will break.
//!
//! So this server answers real HTTP on a real port, and the tests that use it run
//! the *production* client against it.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};

use axum::extract::{Path as AxumPath, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use parking_lot::RwLock;
use serde_json::{Value, json};

/// How the fake server should behave.
#[derive(Debug, Clone)]
pub struct FakeServerConfig {
    /// Context window reported by `/props` and `/slots`.
    pub n_ctx: i64,
    /// Spacing of the checkpoint ring, matching llama.cpp's `-cms`.
    pub checkpoint_interval: i64,
    /// Ring depth, matching `-ctxcp`.
    pub ring_capacity: usize,
    /// Report a sliding-window architecture in the model name, so the client's
    /// `partial_state_only` detection fires.
    pub swa_model: bool,
    /// Omit the checkpoint ring from `/slots`, as older builds do.
    pub hide_checkpoints: bool,
    /// Omit the ring but accept `/slots/{id}/save`, as builds with only disk
    /// checkpoints do.
    pub disk_checkpoints_only: bool,
    /// Return 404 for `/slots` entirely, simulating a minimal build.
    pub no_slots: bool,
    /// Report `is_processing: true`, to test the consolidator's idle gate.
    pub busy: bool,
    /// Report a prompt-eval time on `/slots`.
    pub prompt_eval_ms: Option<i64>,
    /// Fail `/slots/{id}/save` with a 500.
    pub save_fails: bool,
}

impl Default for FakeServerConfig {
    fn default() -> Self {
        Self {
            n_ctx: 32_768,
            checkpoint_interval: 512,
            ring_capacity: 8,
            swa_model: false,
            hide_checkpoints: false,
            disk_checkpoints_only: false,
            no_slots: false,
            busy: false,
            prompt_eval_ms: Some(120),
            save_fails: false,
        }
    }
}

impl FakeServerConfig {
    /// A current, fully capable build.
    pub fn modern() -> Self {
        Self::default()
    }

    /// A build that reports no checkpoint ring at all.
    pub fn no_ring() -> Self {
        Self { hide_checkpoints: true, ..Default::default() }
    }

    /// A build with no slot-management endpoints.
    pub fn no_slots_at_all() -> Self {
        Self { no_slots: true, ..Default::default() }
    }

    /// A sliding-window/hybrid model, where checkpoints carry partial state.
    pub fn sliding_window() -> Self {
        Self { swa_model: true, ..Default::default() }
    }
}

#[derive(Debug)]
struct State_ {
    config: FakeServerConfig,
    n_past: AtomicI64,
    ring: RwLock<Vec<i64>>,
    saves: RwLock<Vec<String>>,
    saves_dir: std::path::PathBuf,
    kill_switch: AtomicBool,
}

/// A running fake llama.cpp server.
pub struct FakeLlamaServer {
    base_url: String,
    shared: Arc<State_>,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    handle: Option<tokio::task::JoinHandle<()>>,
}

impl std::fmt::Debug for FakeLlamaServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FakeLlamaServer").field("base_url", &self.base_url).finish()
    }
}

impl FakeLlamaServer {
    /// Start a server on an ephemeral port.
    pub async fn start(config: FakeServerConfig) -> std::io::Result<Self> {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await?;
        let addr = listener.local_addr()?;
        let base_url = format!("http://{addr}");

        let saves_dir =
            std::env::temp_dir().join(format!("sakur4-fake-{}", crate::unique_suffix()));
        std::fs::create_dir_all(&saves_dir).ok();

        let shared = Arc::new(State_ {
            config,
            n_past: AtomicI64::new(0),
            ring: RwLock::new(Vec::new()),
            saves: RwLock::new(Vec::new()),
            saves_dir,
            kill_switch: AtomicBool::new(false),
        });

        let app = router(shared.clone());
        let (tx, rx) = tokio::sync::oneshot::channel();
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = rx.await;
                })
                .await;
        });

        Ok(Self { base_url, shared, shutdown: Some(tx), handle: Some(handle) })
    }

    /// The base URL to hand to `LlamaCppBackend::connect`.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Simulate the model having prefilled `tokens` tokens.
    pub fn advance_to(&self, tokens: i64) {
        self.shared.n_past.store(tokens, Ordering::SeqCst);
        let mut ring = Vec::new();
        let mut pos = self.shared.config.checkpoint_interval;
        while pos <= tokens {
            ring.push(pos);
            pos += self.shared.config.checkpoint_interval;
        }
        let cap = self.shared.config.ring_capacity;
        if ring.len() > cap {
            ring.drain(..ring.len() - cap);
        }
        *self.shared.ring.write() = ring;
    }

    /// Simulate a prefix divergence: state survives, the ring does not.
    pub fn simulate_rewrite(&self, retained: i64) {
        self.shared.n_past.store(retained, Ordering::SeqCst);
        self.shared.ring.write().clear();
    }

    /// Currently retained ring positions.
    pub fn ring(&self) -> Vec<i64> {
        self.shared.ring.read().clone()
    }

    /// Files this server has written.
    pub fn saved_files(&self) -> Vec<String> {
        self.shared.saves.read().clone()
    }

    /// Make every subsequent request fail, to test transport-error handling.
    pub fn break_transport(&self) {
        self.shared.kill_switch.store(true, Ordering::SeqCst);
    }
}

impl Drop for FakeLlamaServer {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        if let Some(h) = self.handle.take() {
            h.abort();
        }
        let _ = std::fs::remove_dir_all(&self.shared.saves_dir);
    }
}

fn router(shared: Arc<State_>) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/props", get(props))
        .route("/metrics", get(metrics))
        .route("/tokenize", post(tokenize))
        .route("/slots", get(slots))
        .route("/slots/{id}/save", post(save))
        .route("/slots/{id}/restore", post(restore))
        .route("/slots/{id}/erase", post(erase))
        .route("/v1/models", get(models))
        .with_state(shared)
}

fn guard(shared: &State_) -> Result<(), (axum::http::StatusCode, String)> {
    if shared.kill_switch.load(Ordering::SeqCst) {
        return Err((
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            "server has been broken on purpose".into(),
        ));
    }
    Ok(())
}

async fn health(
    State(s): State<Arc<State_>>,
) -> Result<Json<Value>, (axum::http::StatusCode, String)> {
    guard(&s)?;
    Ok(Json(json!({"status": "ok"})))
}

async fn props(
    State(s): State<Arc<State_>>,
) -> Result<Json<Value>, (axum::http::StatusCode, String)> {
    guard(&s)?;
    let model = if s.config.swa_model {
        "/models/gemma-3-27b-it-Q4_K_M.gguf"
    } else {
        "/models/qwen3-32b-Q4_K_M.gguf"
    };
    Ok(Json(json!({
        "model_path": model,
        "n_ctx": s.config.n_ctx,
        "total_slots": 1,
    })))
}

async fn metrics(State(s): State<Arc<State_>>) -> Result<String, (axum::http::StatusCode, String)> {
    guard(&s)?;
    Ok(format!(
        "# HELP llamacpp:prompt_tokens_total prompt tokens\nllamacpp:prompt_tokens_total {}\n",
        s.n_past.load(Ordering::SeqCst)
    ))
}

async fn models(
    State(s): State<Arc<State_>>,
) -> Result<Json<Value>, (axum::http::StatusCode, String)> {
    guard(&s)?;
    Ok(Json(json!({"data": [{"id": "fake-model"}]})))
}

async fn tokenize(
    State(s): State<Arc<State_>>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, (axum::http::StatusCode, String)> {
    guard(&s)?;
    let content = body.get("content").and_then(|c| c.as_str()).unwrap_or("");
    // Deterministic 4-characters-per-token, matching the testkit tokenizer so
    // cross-checks are exact.
    let n = content.chars().count().div_ceil(4);
    Ok(Json(json!({
        "tokens": (0..n).map(|i| i as i64).collect::<Vec<_>>()
    })))
}

async fn slots(
    State(s): State<Arc<State_>>,
) -> Result<Json<Value>, (axum::http::StatusCode, String)> {
    guard(&s)?;
    if s.config.no_slots {
        return Err((axum::http::StatusCode::NOT_FOUND, "no /slots on this build".into()));
    }
    let ring = s.ring.read().clone();
    let mut slot = json!({
        "id": 0,
        "id_task": -1,
        "n_ctx": s.config.n_ctx,
        "n_past": s.n_past.load(Ordering::SeqCst),
        "is_processing": s.config.busy,
    });
    if let Some(ms) = s.config.prompt_eval_ms {
        slot["prompt_eval_ms"] = json!(ms);
    }
    if !s.config.hide_checkpoints && !s.config.disk_checkpoints_only {
        // Two shapes are emitted in the wild; the client must handle both, and
        // this server emits the object form, which is the newer one.
        slot["checkpoints"] = json!(
            ring.iter()
                .enumerate()
                .map(|(i, p)| json!({"pos": p, "size": 1024 * (i as i64 + 1)}))
                .collect::<Vec<_>>()
        );
    } else if s.config.disk_checkpoints_only {
        slot["checkpoints"] = json!([]);
    }
    Ok(Json(json!([slot])))
}

async fn save(
    State(s): State<Arc<State_>>,
    AxumPath(id): AxumPath<String>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, (axum::http::StatusCode, String)> {
    guard(&s)?;
    if s.config.save_fails {
        return Err((axum::http::StatusCode::INTERNAL_SERVER_ERROR, "save disabled".into()));
    }
    let filename =
        body.get("filename").or_else(|| body.get("path")).and_then(|v| v.as_str()).unwrap_or("");
    if filename.is_empty() {
        return Err((axum::http::StatusCode::BAD_REQUEST, "no filename".into()));
    }
    let path = if std::path::Path::new(filename).is_absolute() {
        std::path::PathBuf::from(filename)
    } else {
        s.saves_dir.join(filename)
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(internal)?;
    }
    let payload = json!({
        "slot_id": id,
        "n_past": s.n_past.load(Ordering::SeqCst),
        "checkpoints": *s.ring.read(),
    });
    let bytes = serde_json::to_vec(&payload).map_err(internal)?;
    std::fs::write(&path, &bytes).map_err(internal)?;
    s.saves.write().push(path.to_string_lossy().to_string());
    Ok(Json(json!({
        "filename": path.to_string_lossy(),
        "size": bytes.len(),
    })))
}

async fn restore(
    State(s): State<Arc<State_>>,
    AxumPath(id): AxumPath<String>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, (axum::http::StatusCode, String)> {
    guard(&s)?;
    let filename =
        body.get("filename").or_else(|| body.get("path")).and_then(|v| v.as_str()).unwrap_or("");
    let path = if std::path::Path::new(filename).is_absolute() {
        std::path::PathBuf::from(filename)
    } else {
        s.saves_dir.join(filename)
    };
    let raw = std::fs::read_to_string(&path)
        .map_err(|e| (axum::http::StatusCode::NOT_FOUND, e.to_string()))?;
    let doc: Value = serde_json::from_str(&raw).map_err(internal)?;
    let n_past = doc.get("n_past").and_then(|v| v.as_i64()).unwrap_or(0);
    s.n_past.store(n_past, Ordering::SeqCst);
    if let Some(cps) = doc.get("checkpoints").and_then(|c| c.as_array()) {
        let ring: Vec<i64> = cps.iter().filter_map(|v| v.as_i64()).collect();
        *s.ring.write() = ring;
    }
    Ok(Json(json!({"slot_id": id, "n_past": n_past})))
}

async fn erase(
    State(s): State<Arc<State_>>,
    AxumPath(_id): AxumPath<String>,
) -> Result<Json<Value>, (axum::http::StatusCode, String)> {
    guard(&s)?;
    s.n_past.store(0, Ordering::SeqCst);
    s.ring.write().clear();
    Ok(Json(json!({"ok": true})))
}

fn internal<E: std::fmt::Display>(e: E) -> (axum::http::StatusCode, String) {
    (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}
