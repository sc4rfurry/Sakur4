//! Embeddings for dense retrieval.
//!
//! FR-12 requires the default embedding model to be "a small (<1.5GB) local
//! embedding model" and configurable. Two constraints shape the design:
//!
//! * **NFR-10: no mandatory network egress.** Sakur4 never downloads a model.
//!   If the operator points it at a local embedding endpoint (llama.cpp's
//!   `/v1/embeddings` with an embedding model loaded, Ollama, a local
//!   `text-embeddings-inference`), that is used. If not, Sakur4 does not silently
//!   reach for Hugging Face — it uses [`HashingEmbedder`] and *says so*.
//! * **Degrade, never fail.** Dense retrieval is one of three retrievers in the
//!   Hybrid Recall Engine. An unavailable embedder downgrades recall quality; it
//!   must not downgrade availability. [`FallbackEmbedder`] makes that structural.
//!
//! [`HashingEmbedder`] is a deterministic bag-of-features projection — the
//! "hashing trick" over word unigrams and character trigrams, signed and
//! L2-normalised. It is a legitimate retrieval signal (it is what the classic
//! Vowpal Wabbit / scikit-learn `HashingVectorizer` gives you), it requires no
//! model file, and being deterministic it makes tests reproducible. It is not a
//! semantic model, and the code says so everywhere it could be mistaken for one.

use std::sync::Arc;

use async_trait::async_trait;

use crate::error::{Error, Result};

/// Default dimensionality. 384 matches the common small-model output width, so
/// switching to a real model does not require a schema change.
pub const DEFAULT_DIM: usize = 384;

/// Anything that can embed text.
#[async_trait]
pub trait Embedder: Send + Sync + std::fmt::Debug {
    /// Embed a batch. Implementations should preserve order and length.
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>>;

    /// Embed one string.
    async fn embed_one(&self, text: &str) -> Result<Vec<f32>> {
        let mut v = self.embed(std::slice::from_ref(&text.to_string())).await?;
        v.pop()
            .ok_or_else(|| Error::Other("embedder returned no vector".into()))
    }

    /// Stable identifier for the embedding space. Vectors from different models
    /// are never compared, and this string is how the store tells them apart.
    fn model_id(&self) -> &str;

    fn dim(&self) -> usize;

    /// Whether this is a real learned model or a deterministic fallback.
    fn is_semantic(&self) -> bool;

    /// Human description for `doctor` and the receipt.
    fn describe(&self) -> String {
        format!(
            "{} (dim {}, {})",
            self.model_id(),
            self.dim(),
            if self.is_semantic() {
                "semantic"
            } else {
                "deterministic hashing — lexical only, no semantic generalisation"
            }
        )
    }
}

/// Deterministic hashing embedder: no model, no network, no randomness.
#[derive(Debug, Clone)]
pub struct HashingEmbedder {
    dim: usize,
}

impl Default for HashingEmbedder {
    fn default() -> Self {
        Self { dim: DEFAULT_DIM }
    }
}

impl HashingEmbedder {
    pub fn new(dim: usize) -> Self {
        Self { dim: dim.max(16) }
    }

    fn features(text: &str) -> Vec<String> {
        let mut out = Vec::new();
        let lower = text.to_lowercase();
        for word in lower.split(|c: char| !c.is_alphanumeric() && c != '_') {
            if word.len() >= 2 {
                out.push(format!("w:{word}"));
            }
        }
        // Character trigrams supply the sub-word signal a real BPE model gets
        // from its vocabulary, which is what makes identifier matching work at
        // all (`checkUser` vs `validateUser` share `ser`/`use`).
        let chars: Vec<char> = lower.chars().collect();
        if chars.len() >= 3 {
            for win in chars.windows(3) {
                if win.iter().all(|c| c.is_alphanumeric() || *c == '_') {
                    out.push(format!("t:{}{}{}", win[0], win[1], win[2]));
                }
            }
        }
        out
    }
}

#[async_trait]
impl Embedder for HashingEmbedder {
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        Ok(texts.iter().map(|t| self.hash_embed(t)).collect())
    }

    fn model_id(&self) -> &str {
        "sakur4-hashing-v1"
    }

    fn dim(&self) -> usize {
        self.dim
    }

    fn is_semantic(&self) -> bool {
        false
    }

    fn describe(&self) -> String {
        format!(
            "{} (dim {}, deterministic hashing of word unigrams + character trigrams; \
             local and dependency-free, but lexical rather than semantic — configure a local \
             embedding endpoint for real semantic recall)",
            self.model_id(),
            self.dim
        )
    }
}

impl HashingEmbedder {
    /// The projection itself, exposed for tests.
    pub fn hash_embed(&self, text: &str) -> Vec<f32> {
        let mut vec = vec![0f32; self.dim];
        for feature in Self::features(text) {
            let h = blake3::hash(feature.as_bytes());
            let bytes = h.as_bytes();
            let idx = (u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize)
                % self.dim;
            // Sign from a different byte so collisions do not systematically
            // reinforce: the expected inner product of unrelated texts is then
            // ~0 rather than positive.
            let sign = if bytes[4] & 1 == 0 { 1.0 } else { -1.0 };
            vec[idx] += sign;
        }
        let n = vec.iter().map(|x| x * x).sum::<f32>().sqrt();
        if n > f32::EPSILON {
            for v in &mut vec {
                *v /= n;
            }
        }
        vec
    }
}

/// An embedder backed by an OpenAI-compatible `/v1/embeddings` endpoint.
///
/// Covers llama.cpp with an embedding model loaded, Ollama, and any local
/// OpenAI-compatible server, without three separate integrations.
#[derive(Debug)]
pub struct OpenAiEmbedder {
    client: reqwest::Client,
    base_url: String,
    model: String,
    dim: usize,
    api_key: Option<String>,
}

impl OpenAiEmbedder {
    /// Build without probing; `dim` is confirmed on the first call.
    pub fn new(base_url: &str, model: &str, dim: usize) -> Self {
        Self {
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .expect("reqwest client"),
            base_url: base_url.trim_end_matches('/').to_string(),
            model: model.to_string(),
            dim,
            api_key: std::env::var("SAKUR4_EMBED_API_KEY")
                .ok()
                .filter(|s| !s.trim().is_empty()),
        }
    }

    /// Confirm the endpoint answers and report the real vector width.
    pub async fn probe(base_url: &str, model: &str) -> Result<usize> {
        let e = Self::new(base_url, model, DEFAULT_DIM);
        let v = e.embed_one("sakur4 embedding probe").await?;
        Ok(v.len())
    }
}

#[async_trait]
impl Embedder for OpenAiEmbedder {
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let mut rb = self
            .client
            .post(format!("{}/v1/embeddings", self.base_url))
            .json(&serde_json::json!({ "model": self.model, "input": texts }));
        if let Some(key) = &self.api_key {
            rb = rb.bearer_auth(key);
        }
        let resp = rb.send().await?;
        if !resp.status().is_success() {
            return Err(Error::BackendUnavailable(format!(
                "embedding endpoint returned {}",
                resp.status()
            )));
        }
        let doc: serde_json::Value = resp.json().await?;
        let data = doc
            .get("data")
            .and_then(|d| d.as_array())
            .ok_or_else(|| Error::BackendUnavailable("embedding response had no data array".into()))?;

        let mut out = Vec::with_capacity(data.len());
        for item in data {
            let vec = item
                .get("embedding")
                .and_then(|e| e.as_array())
                .ok_or_else(|| Error::BackendUnavailable("embedding item had no vector".into()))?;
            out.push(
                vec.iter()
                    .map(|v| v.as_f64().unwrap_or(0.0) as f32)
                    .collect::<Vec<f32>>(),
            );
        }
        Ok(out)
    }

    fn model_id(&self) -> &str {
        &self.model
    }

    fn dim(&self) -> usize {
        self.dim
    }

    fn is_semantic(&self) -> bool {
        true
    }
}

/// Try `primary`; on any failure fall back to `secondary` for that call.
///
/// A local embedding server that is restarting should cost recall quality for
/// the duration, not cause `memory.recall` to fail.
#[derive(Debug)]
pub struct FallbackEmbedder {
    primary: Arc<dyn Embedder>,
    secondary: Arc<dyn Embedder>,
    degraded: parking_lot::RwLock<bool>,
}

impl FallbackEmbedder {
    pub fn new(primary: Arc<dyn Embedder>, secondary: Arc<dyn Embedder>) -> Self {
        Self {
            primary,
            secondary,
            degraded: parking_lot::RwLock::new(false),
        }
    }

    /// True when the primary embedder has failed at least once and the fallback
    /// is in use. Surfaced through `doctor` so a quality regression is visible
    /// rather than mysterious.
    pub fn is_degraded(&self) -> bool {
        *self.degraded.read()
    }
}

#[async_trait]
impl Embedder for FallbackEmbedder {
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        match self.primary.embed(texts).await {
            Ok(v) => {
                if *self.degraded.read() {
                    tracing::info!("primary embedder recovered");
                    *self.degraded.write() = false;
                }
                Ok(v)
            }
            Err(e) => {
                if !*self.degraded.read() {
                    tracing::warn!(
                        error = %e,
                        primary = self.primary.model_id(),
                        "primary embedder unavailable; dense recall degraded to {}",
                        self.secondary.model_id()
                    );
                    *self.degraded.write() = true;
                }
                self.secondary.embed(texts).await
            }
        }
    }

    fn model_id(&self) -> &str {
        // The id must reflect what actually produced stored vectors. Because a
        // degraded fallback writes *different* vectors, Sakur4 reports the
        // primary's id only while it is healthy; see `describe`.
        if *self.degraded.read() {
            self.secondary.model_id()
        } else {
            self.primary.model_id()
        }
    }

    fn dim(&self) -> usize {
        self.primary.dim()
    }

    fn is_semantic(&self) -> bool {
        if *self.degraded.read() {
            self.secondary.is_semantic()
        } else {
            self.primary.is_semantic()
        }
    }

    fn describe(&self) -> String {
        if *self.degraded.read() {
            format!(
                "DEGRADED — primary {} unavailable, using {}",
                self.primary.model_id(),
                self.secondary.describe()
            )
        } else {
            self.primary.describe()
        }
    }
}

/// Resolve an embedder from configuration, following the PRD's ordering:
/// an explicitly configured local endpoint first, deterministic hashing last.
pub async fn resolve(url: Option<&str>, model: Option<&str>) -> Arc<dyn Embedder> {
    let url = url
        .map(String::from)
        .or_else(|| std::env::var("SAKUR4_EMBED_URL").ok())
        .filter(|s| !s.trim().is_empty());
    let model = model
        .map(String::from)
        .or_else(|| std::env::var("SAKUR4_EMBED_MODEL").ok())
        .unwrap_or_else(|| "nomic-embed-text".to_string());

    let hashing: Arc<dyn Embedder> = Arc::new(HashingEmbedder::default());

    let Some(url) = url else {
        tracing::info!(
            "no embedding endpoint configured (SAKUR4_EMBED_URL / --embed-url); \
             using the deterministic hashing embedder — lexical recall only"
        );
        return hashing;
    };

    match OpenAiEmbedder::probe(&url, &model).await {
        Ok(dim) => {
            tracing::info!(url = %url, model = %model, dim, "using local embedding endpoint");
            Arc::new(FallbackEmbedder::new(
                Arc::new(OpenAiEmbedder::new(&url, &model, dim)),
                hashing,
            ))
        }
        Err(e) => {
            tracing::warn!(
                url = %url,
                model = %model,
                error = %e,
                "embedding endpoint unreachable; using the deterministic hashing embedder"
            );
            hashing
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn hashing_embedder_is_deterministic_and_normalised() {
        let e = HashingEmbedder::default();
        let a = e.embed_one("the cache coherence layer").await.unwrap();
        let b = e.embed_one("the cache coherence layer").await.unwrap();
        assert_eq!(a, b);
        assert_eq!(a.len(), DEFAULT_DIM);
        let n: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((n - 1.0).abs() < 1e-4, "vectors must be L2-normalised, got {n}");
    }

    #[tokio::test]
    async fn similar_text_scores_higher_than_unrelated_text() {
        let e = HashingEmbedder::default();
        let q = e.embed_one("memory recall staleness").await.unwrap();
        let close = e.embed_one("memory recall and staleness detection").await.unwrap();
        let far = e.embed_one("banana bread recipe with walnuts").await.unwrap();
        let dot = |a: &[f32], b: &[f32]| a.iter().zip(b).map(|(x, y)| x * y).sum::<f32>();
        assert!(
            dot(&q, &close) > dot(&q, &far),
            "lexical overlap must beat unrelated text"
        );
    }

    #[tokio::test]
    async fn empty_text_yields_a_zero_vector_not_an_error() {
        let e = HashingEmbedder::default();
        let v = e.embed_one("").await.unwrap();
        assert_eq!(v.len(), DEFAULT_DIM);
        assert!(v.iter().all(|x| *x == 0.0));
    }

    #[tokio::test]
    async fn fallback_switches_over_and_reports_degradation() {
        #[derive(Debug)]
        struct Broken;
        #[async_trait]
        impl Embedder for Broken {
            async fn embed(&self, _t: &[String]) -> Result<Vec<Vec<f32>>> {
                Err(Error::BackendUnavailable("down".into()))
            }
            fn model_id(&self) -> &str {
                "broken-model"
            }
            fn dim(&self) -> usize {
                8
            }
            fn is_semantic(&self) -> bool {
                true
            }
        }

        let f = FallbackEmbedder::new(
            Arc::new(Broken),
            Arc::new(HashingEmbedder::default()),
        );
        assert!(!f.is_degraded());
        assert_eq!(f.model_id(), "broken-model");

        let v = f.embed_one("hello").await.unwrap();
        assert_eq!(v.len(), DEFAULT_DIM, "fallback vector width wins");
        assert!(f.is_degraded());
        assert_eq!(f.model_id(), "sakur4-hashing-v1");
        assert!(f.describe().contains("DEGRADED"));
    }

    #[tokio::test]
    async fn resolve_without_configuration_returns_the_offline_embedder() {
        // No SAKUR4_EMBED_URL in the test environment, no explicit url.
        let e = resolve(Some(""), None).await;
        assert!(!e.is_semantic());
        assert_eq!(e.model_id(), "sakur4-hashing-v1");
        assert!(e.describe().contains("dependency-free"));
    }

    #[tokio::test]
    async fn resolve_falls_back_when_the_endpoint_is_dead() {
        let e = resolve(Some("http://127.0.0.1:1"), Some("nope")).await;
        assert!(!e.is_semantic());
    }

    #[tokio::test]
    async fn batch_embedding_preserves_order() {
        let e = HashingEmbedder::default();
        let texts = vec!["alpha".to_string(), "beta".to_string(), "gamma".to_string()];
        let vs = e.embed(&texts).await.unwrap();
        assert_eq!(vs.len(), 3);
        assert_eq!(vs[0], e.embed_one("alpha").await.unwrap());
        assert_eq!(vs[2], e.embed_one("gamma").await.unwrap());
    }
}
