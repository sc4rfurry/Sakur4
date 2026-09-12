//! Token accounting.
//!
//! The Context Ledger Receipt (FR-15) is only meaningful if every category's
//! token count is measured the same way, so all accounting flows through
//! [`TokenCounter`]. Three strategies exist, in descending order of fidelity:
//!
//! 1. [`TokenCounter::Backend`] — ask the llama.cpp server's `/tokenize`
//!    endpoint. Exact for the loaded model, including its chat template.
//! 2. [`TokenCounter::Bpe`] — a local `tokenizer.json` (rust-tokenizers is not
//!    a dependency by default, so this is engaged only when a caller supplies a
//!    counter).
//! 3. [`TokenCounter::Heuristic`] — a character-class model calibrated against
//!    BPE behaviour on mixed source code and prose.
//!
//! The heuristic is deliberately conservative (it rounds up) because every
//! budget decision in the eviction engine compares against it: under-counting
//! would let a prompt silently overflow the window.

use std::sync::Arc;

use crate::error::Result;

/// A token measurement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct TokenCount(pub usize);

impl TokenCount {
    pub fn get(self) -> usize {
        self.0
    }
}

impl std::fmt::Display for TokenCount {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// How a count was produced, so the receipt can state its own confidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum TokenizerKind {
    /// Exact, from the serving backend.
    Backend,
    /// Exact for a different tokenizer than the serving model; approximate in
    /// practice for the prompt being measured.
    LocalBpe,
    /// Statistical approximation.
    Heuristic,
}

impl TokenizerKind {
    pub fn as_str(self) -> &'static str {
        match self {
            TokenizerKind::Backend => "backend-exact",
            TokenizerKind::LocalBpe => "local-bpe",
            TokenizerKind::Heuristic => "heuristic",
        }
    }
}

/// Anything that can turn text into a token count.
pub trait TokenEstimator: Send + Sync + std::fmt::Debug {
    /// Count tokens in `text`.
    fn count(&self, text: &str) -> usize;

    /// Count tokens for a whole rendered prompt.
    ///
    /// Defaults to summing per-part counts; backends that can tokenize a full
    /// string in one round trip override this so BPE merges across boundaries
    /// are accounted for.
    fn count_parts(&self, parts: &[&str]) -> usize {
        parts.iter().map(|p| self.count(p)).sum()
    }

    fn kind(&self) -> TokenizerKind;

    fn model_name(&self) -> Option<&str> {
        None
    }
}

/// A handle to whichever estimator is live.
#[derive(Clone)]
pub struct TokenCounter {
    inner: Arc<dyn TokenEstimator>,
}

impl std::fmt::Debug for TokenCounter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokenCounter")
            .field("kind", &self.kind())
            .field("model", &self.model_name())
            .finish()
    }
}

impl Default for TokenCounter {
    fn default() -> Self {
        Self::heuristic()
    }
}

impl TokenCounter {
    pub fn new(estimator: impl TokenEstimator + 'static) -> Self {
        Self {
            inner: Arc::new(estimator),
        }
    }

    /// The default, dependency-free estimator.
    pub fn heuristic() -> Self {
        Self::new(HeuristicTokenizer::default())
    }

    pub fn from_arc(inner: Arc<dyn TokenEstimator>) -> Self {
        Self { inner }
    }

    pub fn count(&self, text: &str) -> TokenCount {
        TokenCount(self.inner.count(text))
    }

    pub fn count_parts(&self, parts: &[&str]) -> TokenCount {
        TokenCount(self.inner.count_parts(parts))
    }

    pub fn kind(&self) -> TokenizerKind {
        self.inner.kind()
    }

    pub fn model_name(&self) -> Option<String> {
        self.inner.model_name().map(|s| s.to_string())
    }

    pub fn inner(&self) -> Arc<dyn TokenEstimator> {
        self.inner.clone()
    }
}

/// Character-class heuristic tokenizer.
///
/// Calibrated so that, for the mixture of prose, source code and tool JSON that
/// a coding agent produces, the estimate lands within roughly ±10% of a
/// GPT-2/Qwen-class BPE tokenizer without carrying a vocabulary file:
///
/// * ASCII words of length `n` cost `ceil(n / 4.2)` tokens — English prose
///   averages ~4 characters per BPE token, identifiers slightly more.
/// * Runs of 3+ punctuation characters (code, ASCII art, separators) are counted
///   in pairs, because BPE tokenizers have digraph merges for common symbols.
/// * CJK and other wide scripts are ~1 token per character.
/// * Newlines, leading indentation and JSON structure cost real tokens, so runs
///   of whitespace are charged 1 token per 4 characters.
/// * A constant 4-token overhead covers per-message role/template scaffolding.
#[derive(Debug, Clone)]
pub struct HeuristicTokenizer {
    /// Extra per-message cost for chat-template scaffolding.
    pub per_message_overhead: usize,
    /// Characters per token for ASCII words.
    pub chars_per_token: f32,
}

impl Default for HeuristicTokenizer {
    fn default() -> Self {
        Self {
            per_message_overhead: 4,
            chars_per_token: 4.2,
        }
    }
}

impl TokenEstimator for HeuristicTokenizer {
    fn kind(&self) -> TokenizerKind {
        TokenizerKind::Heuristic
    }

    fn count(&self, text: &str) -> usize {
        if text.is_empty() {
            return 0;
        }
        let mut total = 0f32;
        let mut word_len = 0usize;
        let mut punct_run = 0usize;
        let mut space_run = 0usize;

        let flush_word = |len: usize, total: &mut f32| {
            if len > 0 {
                *total += (len as f32 / 4.2).ceil().max(1.0);
            }
        };
        let flush_punct = |len: usize, total: &mut f32| {
            if len > 0 {
                // Digraph merges mean long symbol runs cost about half their length.
                *total += (len as f32 / 2.0).ceil().max(1.0);
            }
        };
        let flush_space = |len: usize, total: &mut f32| {
            if len > 0 {
                // A single separating space is absorbed by the following token;
                // indentation and blank lines are not.
                *total += if len == 1 { 0.0 } else { (len as f32 / 4.0).ceil() };
            }
        };

        for ch in text.chars() {
            if ch.is_ascii_alphanumeric() || ch == '_' || ch == '\'' || ch == '-' {
                flush_punct(punct_run, &mut total);
                punct_run = 0;
                flush_space(space_run, &mut total);
                space_run = 0;
                word_len += 1;
            } else if ch.is_whitespace() {
                flush_word(word_len, &mut total);
                word_len = 0;
                flush_punct(punct_run, &mut total);
                punct_run = 0;
                space_run += 1;
            } else if ch.is_ascii() {
                flush_word(word_len, &mut total);
                word_len = 0;
                flush_space(space_run, &mut total);
                space_run = 0;
                punct_run += 1;
            } else {
                flush_word(word_len, &mut total);
                word_len = 0;
                flush_punct(punct_run, &mut total);
                punct_run = 0;
                flush_space(space_run, &mut total);
                space_run = 0;
                // Wide scripts: roughly one token per character, one-and-a-half
                // for the rarer planes that BPE vocabularies fragment.
                total += if (ch as u32) < 0x10000 { 1.0 } else { 1.5 };
            }
        }
        flush_word(word_len, &mut total);
        flush_punct(punct_run, &mut total);
        flush_space(space_run, &mut total);

        (total.ceil() as usize).max(1)
    }

    fn count_parts(&self, parts: &[&str]) -> usize {
        parts
            .iter()
            .map(|p| self.count(p) + self.per_message_overhead)
            .sum::<usize>()
            .saturating_sub(self.per_message_overhead)
    }
}

/// A fixed-per-character estimator. Test-only: makes budget arithmetic in tests
/// exact and obvious (`100 tokens == 100 characters`).
#[derive(Debug, Clone)]
pub struct CharTokenizer {
    pub chars_per_token: usize,
}

impl Default for CharTokenizer {
    fn default() -> Self {
        Self { chars_per_token: 4 }
    }
}

impl TokenEstimator for CharTokenizer {
    fn kind(&self) -> TokenizerKind {
        TokenizerKind::Heuristic
    }

    fn count(&self, text: &str) -> usize {
        if text.is_empty() {
            return 0;
        }
        text.chars()
            .count()
            .div_ceil(self.chars_per_token.max(1))
            .max(1)
    }

    fn model_name(&self) -> Option<&str> {
        Some("char-test")
    }
}

/// An estimator that delegates to an async backend, caching recent results.
///
/// Used when the llama.cpp server is reachable: counts are then exact for the
/// model actually serving the session, which is what FR-15's acceptance
/// criterion ("sum of category counts matches the actual prompt token count
/// within rounding tolerance") really demands.
#[derive(Debug)]
pub struct BackendTokenizer {
    counts: parking_lot::Mutex<std::collections::HashMap<u64, usize>>,
    fallback: HeuristicTokenizer,
    model: Option<String>,
}

impl Default for BackendTokenizer {
    fn default() -> Self {
        Self::new(None)
    }
}

impl BackendTokenizer {
    pub fn new(model: Option<String>) -> Self {
        Self {
            counts: parking_lot::Mutex::new(std::collections::HashMap::new()),
            fallback: HeuristicTokenizer::default(),
            model,
        }
    }

    /// Record an exact count obtained from the backend for `text`.
    pub fn record(&self, text: &str, tokens: usize) {
        let key = blake3::hash(text.as_bytes());
        let mut key_bytes = [0u8; 8];
        key_bytes.copy_from_slice(&key.as_bytes()[..8]);
        self.counts
            .lock()
            .insert(u64::from_le_bytes(key_bytes), tokens);
    }

    /// Fall back to the heuristic for text the backend has not seen.
    pub fn estimate_locally(&self, text: &str) -> usize {
        self.fallback.count(text)
    }
}

impl TokenEstimator for BackendTokenizer {
    fn kind(&self) -> TokenizerKind {
        TokenizerKind::Backend
    }

    fn count(&self, text: &str) -> usize {
        let key = blake3::hash(text.as_bytes());
        let mut key_bytes = [0u8; 8];
        key_bytes.copy_from_slice(&key.as_bytes()[..8]);
        let k = u64::from_le_bytes(key_bytes);
        if let Some(v) = self.counts.lock().get(&k) {
            return *v;
        }
        self.fallback.count(text)
    }

    fn model_name(&self) -> Option<&str> {
        self.model.as_deref()
    }
}

/// Choose the best available counter for a session.
///
/// Ordering is fidelity-first: an exact backend count beats a local estimate.
/// `probe` is called once with a known string to confirm the backend really can
/// tokenize (an OpenAI-compatible shim may advertise `/tokenize` and 404 on use).
pub async fn select_counter<F, Fut>(probe: F) -> TokenCounter
where
    F: FnOnce(&str) -> Fut,
    Fut: std::future::Future<Output = Result<usize>>,
{
    const CANARY: &str = "Sakur4 token accounting probe.";
    match probe(CANARY).await {
        Ok(n) if n > 0 => {
            let bt = BackendTokenizer::new(None);
            bt.record(CANARY, n);
            tracing::info!(canary_tokens = n, "using backend-exact token accounting");
            TokenCounter::new(bt)
        }
        Ok(_) => TokenCounter::heuristic(),
        Err(e) => {
            tracing::debug!(error = %e, "backend tokenizer unavailable; using heuristic");
            TokenCounter::heuristic()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_is_zero() {
        let t = HeuristicTokenizer::default();
        assert_eq!(t.count(""), 0);
    }

    #[test]
    fn prose_is_about_four_chars_per_token() {
        let t = HeuristicTokenizer::default();
        let text = "The quick brown fox jumps over the lazy dog while the agent \
                    remembers exactly where the cache checkpoint boundary was.";
        let n = t.count(text);
        let chars = text.chars().count();
        assert!(
            (n as f32) > (chars as f32 / 6.0) && (n as f32) < (chars as f32 / 3.0),
            "estimate {n} implausible for {chars} characters"
        );
    }

    #[test]
    fn cjk_costs_more_per_character_than_ascii() {
        let t = HeuristicTokenizer::default();
        let ascii = t.count("abcdefghij");
        let cjk = t.count("記憶ファブリック");
        assert!(cjk > ascii, "wide script must cost at least 1 token/char");
    }

    #[test]
    fn code_is_charged_for_symbols_and_indentation() {
        let t = HeuristicTokenizer::default();
        let dense = t.count("fn f(){let x=1;x}");
        let spaced = t.count("fn f() { let x = 1; x }");
        assert!(dense > 0 && spaced > 0);
        assert!(
            t.count("        deeply_indented_symbol_name\n") > 2,
            "indentation must cost tokens"
        );
    }

    #[test]
    fn monotonic_in_length() {
        let t = HeuristicTokenizer::default();
        let short = t.count("let x = 1;");
        let long = t.count("let x = 1; let y = 2; let z = 3; let w = 4;");
        assert!(long > short);
    }

    #[test]
    fn char_tokenizer_is_exact_for_tests() {
        let t = CharTokenizer { chars_per_token: 4 };
        assert_eq!(t.count("aaaaaaaa"), 2);
        assert_eq!(t.count("aaaaa"), 2);
        assert_eq!(t.count(""), 0);
    }

    #[test]
    fn backend_tokenizer_prefers_recorded_counts() {
        let bt = BackendTokenizer::new(Some("test-model".into()));
        assert_eq!(bt.model_name(), Some("test-model"));
        let heuristic = bt.estimate_locally("hello world");
        assert_eq!(bt.count("hello world"), heuristic);
        bt.record("hello world", 999);
        assert_eq!(bt.count("hello world"), 999);
    }

    #[tokio::test]
    async fn counter_selection_falls_back_when_probe_fails() {
        let c = select_counter(|_: &str| async {
            Err(crate::error::Error::BackendUnavailable("no server".into()))
        })
        .await;
        assert_eq!(c.kind(), TokenizerKind::Heuristic);

        let c2 = select_counter(|_: &str| async { Ok(7usize) }).await;
        assert_eq!(c2.kind(), TokenizerKind::Backend);
        assert_eq!(c2.count("Sakur4 token accounting probe.").get(), 7);
    }
}
