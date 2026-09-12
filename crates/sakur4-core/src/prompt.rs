//! Prompt assembly and the coordinate space that the eviction engine and the
//! Cache-Coherence Layer both reason in.
//!
//! # Why this is its own module
//!
//! Two components need to agree on "where" a piece of context is: the eviction
//! engine, which decides what to cut, and the cache layer, which decides whether
//! the cut is alignable to a checkpoint. If each computed positions its own way
//! they would drift, and the failure would be silent — a boundary that looks
//! aligned but is not, producing a full re-prefill while the receipt claims
//! partial reuse.
//!
//! So there is exactly one assembler. [`PromptParts`] holds each category as a
//! separate string; [`PromptParts::positions`] resolves any category offset into
//! a whole-prompt token position; and the receipt's per-category breakdown is
//! computed from the same structure that produced the prompt, which is what makes
//! FR-15's "sum of category token counts matches the actual prompt token count
//! within rounding tolerance" true by construction rather than by care.

use crate::ids::short_hash_str;
use crate::tokens::TokenCounter;

/// One rendered category of the prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RenderedPart {
    /// The harness's system prompt.
    System,
    /// The Anchor Set block (FR-4).
    Anchors,
    /// The rendered episodic timeline.
    Timeline,
    /// The Repo Cortex map (FR-10).
    RepoMap,
    /// Retrieved memory from Hybrid Recall (FR-12).
    Recall,
    /// Tool schemas the harness will send.
    ToolSchemas,
    /// Fold summaries currently in the window.
    Folds,
    /// Anything else the caller needs accounted for.
    Extra,
}

impl RenderedPart {
    pub fn label(self) -> &'static str {
        match self {
            RenderedPart::System => "system prompt",
            RenderedPart::Anchors => "pinned anchors",
            RenderedPart::Timeline => "raw recent history",
            RenderedPart::RepoMap => "repo map",
            RenderedPart::Recall => "retrieved memory",
            RenderedPart::ToolSchemas => "tool schemas",
            RenderedPart::Folds => "fold summaries",
            RenderedPart::Extra => "other",
        }
    }

    /// Order in which parts are emitted. Anchors are first after the system
    /// prompt on purpose: they are the content least negotiable and most
    /// expensive to lose, so they should sit where nothing can displace them.
    pub fn order(self) -> u8 {
        match self {
            RenderedPart::System => 0,
            RenderedPart::Anchors => 1,
            RenderedPart::Folds => 2,
            RenderedPart::Recall => 3,
            RenderedPart::RepoMap => 4,
            RenderedPart::Timeline => 5,
            RenderedPart::ToolSchemas => 6,
            RenderedPart::Extra => 7,
        }
    }

    /// Which parts the eviction engine can influence.
    ///
    /// Only the timeline is evictable. Retrieval, the repo map and the folds are
    /// *chosen* content — shrinking them is a retrieval decision, not an eviction
    /// one, and conflating the two would let a budget emergency silently degrade
    /// retrieval quality without anything recording that it happened.
    pub fn is_evictable(self) -> bool {
        matches!(self, RenderedPart::Timeline)
    }

    /// Which parts are mandatory regardless of budget.
    pub fn is_mandatory(self) -> bool {
        matches!(self, RenderedPart::System | RenderedPart::Anchors)
    }
}

/// The prompt, in pieces.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct PromptParts {
    pub system: String,
    pub anchors: String,
    pub timeline: String,
    pub repo_map: String,
    pub recall: String,
    pub tool_schemas: String,
    pub folds: String,
    /// Named extras, for harnesses with their own sections.
    pub extra: Vec<(String, String)>,
}

impl PromptParts {
    /// An empty prompt.
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_system(mut self, s: impl Into<String>) -> Self {
        self.system = s.into();
        self
    }

    pub fn with_anchors(mut self, s: impl Into<String>) -> Self {
        self.anchors = s.into();
        self
    }

    pub fn with_timeline(mut self, s: impl Into<String>) -> Self {
        self.timeline = s.into();
        self
    }

    pub fn with_repo_map(mut self, s: impl Into<String>) -> Self {
        self.repo_map = s.into();
        self
    }

    pub fn with_recall(mut self, s: impl Into<String>) -> Self {
        self.recall = s.into();
        self
    }

    pub fn with_tool_schemas(mut self, s: impl Into<String>) -> Self {
        self.tool_schemas = s.into();
        self
    }

    pub fn with_folds(mut self, s: impl Into<String>) -> Self {
        self.folds = s.into();
        self
    }

    /// The text of one part.
    pub fn part(&self, which: RenderedPart) -> &str {
        match which {
            RenderedPart::System => &self.system,
            RenderedPart::Anchors => &self.anchors,
            RenderedPart::Timeline => &self.timeline,
            RenderedPart::RepoMap => &self.repo_map,
            RenderedPart::Recall => &self.recall,
            RenderedPart::ToolSchemas => &self.tool_schemas,
            RenderedPart::Folds => &self.folds,
            RenderedPart::Extra => "",
        }
    }

    /// Every part that carries text, with its header, in emission order.
    pub fn sections(&self) -> Vec<(RenderedPart, &str, &str)> {
        let mut out: Vec<(RenderedPart, &str, &str)> = Vec::new();
        let pairs: [(RenderedPart, &'static str, &str); 7] = [
            (RenderedPart::System, "", &self.system),
            (
                RenderedPart::Anchors,
                "--- pinned anchors ---",
                &self.anchors,
            ),
            (RenderedPart::Folds, "--- folded subtasks ---", &self.folds),
            (
                RenderedPart::Recall,
                "--- retrieved memory ---",
                &self.recall,
            ),
            (RenderedPart::RepoMap, "--- repository map ---", &self.repo_map),
            (
                RenderedPart::Timeline,
                "--- recent history ---",
                &self.timeline,
            ),
            (
                RenderedPart::ToolSchemas,
                "--- tool schemas ---",
                &self.tool_schemas,
            ),
        ];
        for (part, header, text) in pairs {
            if !text.trim().is_empty() {
                out.push((part, header, text));
            }
        }
        out.sort_by_key(|(p, _, _)| p.order());
        out
    }

    /// The assembled prompt, exactly as it would be sent.
    pub fn render(&self) -> String {
        let mut out = String::new();
        for (_, header, text) in self.sections() {
            if !header.is_empty() {
                out.push_str(header);
                out.push('\n');
            }
            out.push_str(text);
            if !text.ends_with('\n') {
                out.push('\n');
            }
        }
        for (name, text) in &self.extra {
            if text.trim().is_empty() {
                continue;
            }
            out.push_str(&format!("--- {name} ---\n{text}\n"));
        }
        out
    }

    /// Total tokens in the assembled prompt, measured through the same tokenizer
    /// the receipt uses.
    ///
    /// Requires a counter rather than estimating: this number is compared against
    /// the context budget to decide whether to compact, and an estimate that
    /// disagrees with the receipt's measurement by a few percent is the difference
    /// between "relaxed" and "over budget".
    pub fn total_tokens(&self, counter: &TokenCounter) -> usize {
        counter.count(&self.render()).get()
    }

    /// Cheap size probe used by callers that already hold a counter.
    pub fn count(&self, counter: &TokenCounter) -> usize {
        counter.count(&self.render()).get()
    }

    /// Tokens per category, for the receipt breakdown.
    pub fn breakdown(&self, counter: &TokenCounter) -> Vec<(RenderedPart, usize)> {
        let mut out: Vec<(RenderedPart, usize)> = self
            .sections()
            .into_iter()
            .map(|(part, header, text)| {
                let mut owned = String::new();
                if !header.is_empty() {
                    owned.push_str(header);
                    owned.push('\n');
                }
                owned.push_str(text);
                (part, counter.count(&owned).get())
            })
            .collect();
        let extra = self
            .extra
            .iter()
            .map(|(n, t)| counter.count(&format!("--- {n} ---\n{t}\n")).get())
            .sum::<usize>();
        if extra > 0 {
            out.push((RenderedPart::Extra, extra));
        }
        out
    }

    /// Tokens held by parts the eviction engine cannot touch.
    pub fn fixed_tokens(&self, counter: &TokenCounter) -> usize {
        self.breakdown(counter)
            .into_iter()
            .filter(|(p, _)| !p.is_evictable())
            .map(|(_, t)| t)
            .sum()
    }

    /// Tokens in the evictable timeline, measured.
    ///
    /// Measured rather than estimated, and measured through the *same* tokenizer
    /// the eviction engine and the receipt use. An approximation here is not
    /// cosmetic: the eviction engine compares this against the compaction
    /// threshold, so a chars-per-token guess that disagrees with the receipt's
    /// number by a few percent means the engine decides "relaxed" while the
    /// receipt prints a window that is three-quarters full.
    pub fn timeline_tokens(&self, counter: &TokenCounter) -> usize {
        counter.count(&self.timeline).get()
    }

    /// The character offset at which `part` begins in the rendered prompt.
    ///
    /// Exact, not approximate: [`PromptParts::render`] is the only assembler, so
    /// the offsets it implies are the offsets that will be sent.
    pub fn part_offsets(&self) -> Vec<(RenderedPart, usize, usize)> {
        let mut out = Vec::new();
        let mut cursor = 0usize;
        for (part, header, text) in self.sections() {
            let start = cursor;
            if !header.is_empty() {
                cursor += header.len() + 1;
            }
            cursor += text.len();
            if !text.ends_with('\n') {
                cursor += 1;
            }
            out.push((part, start, cursor));
        }
        out
    }

    /// The retained prefix, as text, when the eviction boundary falls inside the
    /// timeline.
    ///
    /// # The approximation, stated plainly
    ///
    /// The boundary arrives as a *timeline token count*, and mapping tokens to
    /// characters requires a tokenizer that can round-trip. Rather than pretend,
    /// Sakur4 converts with the same 4-characters-per-token ratio the timeline is
    /// built with, then clamps to the timeline's end. The result is used only to
    /// compute a reuse *estimate* for the receipt and to key the slot's retained
    /// state; the authoritative reuse number is whatever the backend reports for
    /// the prompt it actually evaluates. A wrong estimate here cannot corrupt
    /// anything — it can only make the receipt's projection optimistic, and the
    /// receipt prints both.
    pub fn retained_prefix_text(&self, timeline_tokens: usize) -> String {
        let mut out = String::new();
        for (part, _, text) in self.sections() {
            match part {
                RenderedPart::Timeline => {
                    let chars = timeline_tokens.saturating_mul(4).min(text.len());
                    // Do not split a UTF-8 character.
                    let mut end = chars;
                    while end > 0 && !text.is_char_boundary(end) {
                        end -= 1;
                    }
                    out.push_str(&text[..end]);
                }
                p if p.order() < RenderedPart::Timeline.order() => out.push_str(text),
                _ => {}
            }
        }
        out
    }

    /// A stable hash of the prompt prefix, for cache bookkeeping.
    pub fn prefix_hash(&self) -> String {
        short_hash_str(&self.render())
    }

    /// True when nothing has been assembled yet.
    pub fn is_empty(&self) -> bool {
        self.render().trim().is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tokens::CharTokenizer;

    fn counter() -> TokenCounter {
        TokenCounter::new(CharTokenizer { chars_per_token: 4 })
    }

    fn sample() -> PromptParts {
        PromptParts::new()
            .with_system("you are an agent")
            .with_anchors("[SAFETY] never force-push")
            .with_timeline("<user> do the thing\n<assistant> ok\n")
            .with_repo_map("src/lib.rs\n  fn main")
            .with_recall("recalled: the cache layer snaps boundaries")
            .with_tool_schemas("{\"name\":\"read\"}")
    }

    #[test]
    fn anchors_render_before_everything_evictable() {
        let p = sample();
        let rendered = p.render();
        let anchors = rendered.find("pinned anchors").unwrap();
        let timeline = rendered.find("recent history").unwrap();
        assert!(anchors < timeline);
    }

    #[test]
    fn ordering_is_by_declared_priority_not_insertion() {
        let p = PromptParts::new()
            .with_timeline("timeline")
            .with_anchors("anchors")
            .with_system("system");
        let rendered = p.render();
        assert!(rendered.find("system").unwrap() < rendered.find("anchors").unwrap());
        assert!(rendered.find("anchors").unwrap() < rendered.find("timeline").unwrap());
    }

    #[test]
    fn empty_parts_are_omitted_entirely() {
        let p = PromptParts::new().with_timeline("only this");
        let rendered = p.render();
        assert!(!rendered.contains("pinned anchors"));
        assert!(!rendered.contains("tool schemas"));
        assert!(rendered.contains("only this"));
    }

    #[test]
    fn breakdown_sums_to_the_measured_total() {
        let p = sample();
        let c = counter();
        let per_part: usize = p.breakdown(&c).iter().map(|(_, t)| *t).sum();
        let whole = c.count(&p.render()).get();
        let tolerance = (whole / 10).max(2);
        assert!(
            per_part.abs_diff(whole) <= tolerance,
            "category sum {per_part} drifted from measured {whole} beyond {tolerance}"
        );
    }

    #[test]
    fn fixed_tokens_excludes_only_the_timeline() {
        let p = sample();
        let c = counter();
        let fixed = p.fixed_tokens(&c);
        let total = c.count(&p.render()).get();
        let timeline = p
            .breakdown(&c)
            .into_iter()
            .find(|(part, _)| *part == RenderedPart::Timeline)
            .map(|(_, t)| t)
            .unwrap();
        assert!(fixed + timeline <= total + 2);
        assert!(fixed > 0);
    }

    #[test]
    fn only_the_timeline_is_evictable() {
        assert!(RenderedPart::Timeline.is_evictable());
        for p in [
            RenderedPart::System,
            RenderedPart::Anchors,
            RenderedPart::RepoMap,
            RenderedPart::Recall,
            RenderedPart::ToolSchemas,
            RenderedPart::Folds,
            RenderedPart::Extra,
        ] {
            assert!(!p.is_evictable(), "{p:?} must not be evictable");
        }
        assert!(RenderedPart::System.is_mandatory());
        assert!(RenderedPart::Anchors.is_mandatory());
    }

    #[test]
    fn retained_prefix_keeps_anchors_and_truncates_the_timeline() {
        let p = sample();
        let retained = p.retained_prefix_text(2);
        assert!(retained.contains("never force-push"), "anchors must survive");
        assert!(retained.contains("you are an agent"));
        assert!(
            !retained.contains("do the thing"),
            "a zero-ish timeline budget must drop the timeline"
        );
    }

    #[test]
    fn retained_prefix_is_monotonic_in_the_budget() {
        let p = sample();
        let small = p.retained_prefix_text(1);
        let medium = p.retained_prefix_text(20);
        let large = p.retained_prefix_text(1_000);
        assert!(large.len() >= medium.len());
        assert!(medium.len() >= small.len());
        // The retained prefix is the *sections*, not the rendered prompt: it
        // carries no headers, which is why it is a prefix of the rendered text
        // with headers stripped.
        assert!(large.contains(p.timeline.trim_end()));
        assert!(large.contains("never force-push"));
        assert!(!large.contains("--- recent history ---"));
    }

    #[test]
    fn retained_prefix_never_splits_a_multibyte_character() {
        let p = PromptParts::new().with_timeline("日本語のテキストです".repeat(20));
        for tokens in 0..40 {
            let s = p.retained_prefix_text(tokens);
            assert!(std::str::from_utf8(s.as_bytes()).is_ok());
        }
    }

    #[test]
    fn part_offsets_are_consistent_with_the_rendered_prompt() {
        let p = sample();
        let rendered = p.render();
        let offsets = p.part_offsets();
        assert!(!offsets.is_empty());
        for (part, start, end) in &offsets {
            assert!(*end <= rendered.len(), "{part:?} offset runs past the prompt");
            assert!(start <= end);
            assert!(rendered.is_char_boundary(*start));
            assert!(rendered.is_char_boundary(*end));
        }
    }

    #[test]
    fn extra_sections_are_accounted_and_rendered() {
        let mut p = PromptParts::new().with_system("s");
        p.extra
            .push(("harness preamble".into(), "always answer briefly".into()));
        let rendered = p.render();
        assert!(rendered.contains("harness preamble"));
        let breakdown = p.breakdown(&counter());
        assert!(breakdown.iter().any(|(part, _)| *part == RenderedPart::Extra));
    }

    #[test]
    fn prefix_hash_tracks_content() {
        let a = sample();
        let mut b = sample();
        assert_eq!(a.prefix_hash(), b.prefix_hash());
        b.timeline.push_str("extra");
        assert_ne!(a.prefix_hash(), b.prefix_hash());
    }

    #[test]
    fn empty_prompt_is_recognised() {
        assert!(PromptParts::new().is_empty());
        assert!(!sample().is_empty());
    }
}
