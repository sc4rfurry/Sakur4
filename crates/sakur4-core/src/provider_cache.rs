//! Provider-cache accounting.
//!
//! # Why this exists next to the Cache-Coherence Layer
//!
//! The Cache-Coherence Layer talks to a local `llama.cpp` slot: it asks the
//! server where its KV cache can be rewound to, and aligns the eviction boundary
//! to it. A hosted provider offers no such API — but it does report, in every
//! completion response, how many prompt tokens it served from its prompt cache and
//! how many it had to process fresh.
//!
//! That is the same signal by a different route, and it measures the same failure:
//!
//! > A compaction that rewrites already-sent history invalidates the cached
//! > prefix from the rewrite point onward, so the next request pays full price for
//! > tokens that were previously discounted.
//!
//! Hermes' own documentation calls this "the strongest argument against"
//! per-turn compaction, and notes the trade depends on numbers specific to the
//! user. Sakur4 can supply those numbers, because it decides where the rewrite
//! happens.
//!
//! # What Sakur4 does and does not do here
//!
//! It does not call a provider (NG1: Sakur4 is a subsystem, not a harness). The
//! harness reports usage through the `context.record_usage` tool — Hermes already
//! has these numbers in `update_from_response`, and every OpenAI-compatible client
//! receives them — and Sakur4 turns them into a per-turn cache verdict with the
//! arithmetic attached.

use crate::error::Result;
use crate::ids::{new_id, now_rfc3339};
use crate::store::Db;

/// One turn's provider-reported usage.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct ProviderUsage {
    /// Prompt tokens the request contained, as the provider counted them.
    pub prompt_tokens: usize,
    pub completion_tokens: usize,
    /// Total billed tokens, when the provider reports it separately.
    pub total_tokens: Option<usize>,
    /// Prompt tokens served from the provider's prompt cache.
    ///
    /// Field names differ by provider: OpenAI reports
    /// `prompt_tokens_details.cached_tokens`, Anthropic `cache_read_input_tokens`,
    /// DeepSeek `prompt_cache_hit_tokens`, Gemini `cachedContentTokenCount`. The
    /// harness normalises whichever it sees into this field.
    pub cache_read_tokens: Option<usize>,
    /// Prompt tokens written *into* the cache this turn, which some providers bill
    /// at a premium (Anthropic's cache write).
    pub cache_write_tokens: Option<usize>,
    pub reasoning_tokens: Option<usize>,
    pub provider: Option<String>,
    pub model: Option<String>,
}

impl ProviderUsage {
    /// Fraction of the prompt that came from cache, in `[0,1]`.
    ///
    /// `None` when the provider did not report cache figures at all — which is
    /// different from zero, and the distinction matters: a provider that does not
    /// report caching cannot be diagnosed by a number it never sent.
    pub fn cache_hit_ratio(&self) -> Option<f64> {
        let read = self.cache_read_tokens?;
        if self.prompt_tokens == 0 {
            return Some(0.0);
        }
        Some((read as f64 / self.prompt_tokens as f64).clamp(0.0, 1.0))
    }

    /// Prompt tokens the provider had to process fresh.
    pub fn uncached_prompt_tokens(&self) -> Option<usize> {
        let read = self.cache_read_tokens?;
        Some(self.prompt_tokens.saturating_sub(read))
    }

    /// Whether the provider reported anything about caching at all.
    pub fn reports_cache(&self) -> bool {
        self.cache_read_tokens.is_some() || self.cache_write_tokens.is_some()
    }
}

/// What changed between two consecutive turns, from the cache's point of view.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderCacheVerdict {
    /// The provider reported nothing about caching.
    NotReported,
    /// No cached prefix was used: a cold start, or a rewrite that broke it.
    CacheMiss,
    /// Some of the prompt came from cache and some was processed fresh. Normal
    /// for an append-only turn: the prefix is reused, the new suffix is not.
    PartialReuse,
    /// Essentially the whole prompt came from cache.
    FullReuse,
    /// The cached prefix shrank relative to the previous turn while the prompt
    /// grew — the signature of a compaction that rewrote already-sent history.
    PrefixBroken,
}

impl ProviderCacheVerdict {
    pub fn as_str(self) -> &'static str {
        match self {
            ProviderCacheVerdict::NotReported => "not-reported",
            ProviderCacheVerdict::CacheMiss => "cache-miss",
            ProviderCacheVerdict::PartialReuse => "partial-reuse",
            ProviderCacheVerdict::FullReuse => "full-reuse",
            ProviderCacheVerdict::PrefixBroken => "PREFIX-BROKEN",
        }
    }

    pub fn headline(self) -> &'static str {
        match self {
            ProviderCacheVerdict::NotReported => {
                "provider did not report prompt-cache usage for this turn"
            }
            ProviderCacheVerdict::CacheMiss => {
                "no cached prefix was used — the whole prompt was processed fresh"
            }
            ProviderCacheVerdict::PartialReuse => {
                "cached prefix reused; only the new suffix was processed fresh"
            }
            ProviderCacheVerdict::FullReuse => "the whole prompt came from the provider's cache",
            ProviderCacheVerdict::PrefixBroken => {
                "the cached prefix SHRANK while the prompt grew — a rewrite invalidated \
                 already-sent history, and this turn paid full price for it"
            }
        }
    }

    /// Whether this verdict describes a compaction that cost money.
    pub fn is_regression(self) -> bool {
        matches!(self, ProviderCacheVerdict::PrefixBroken)
    }
}

/// A recorded turn plus its verdict.
#[derive(Debug, Clone, serde::Serialize)]
pub struct UsageRecord {
    pub turn: i64,
    pub usage: ProviderUsage,
    pub verdict: ProviderCacheVerdict,
    /// Cached tokens on the previous turn, when there was one.
    pub previous_cache_read_tokens: Option<usize>,
    pub detail: String,
}

/// Aggregate provider-cache behaviour over a session.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct ProviderCacheStats {
    pub turns: usize,
    pub turns_reporting: usize,
    pub prefix_breaks: usize,
    pub cache_misses: usize,
    pub total_prompt_tokens: usize,
    pub total_cache_read_tokens: usize,
}

impl ProviderCacheStats {
    /// Share of prompt tokens the provider served from cache across the session.
    pub fn overall_hit_ratio(&self) -> f64 {
        if self.total_prompt_tokens == 0 {
            0.0
        } else {
            self.total_cache_read_tokens as f64 / self.total_prompt_tokens as f64
        }
    }

    pub fn render(&self) -> String {
        if self.turns_reporting == 0 {
            return format!(
                "provider cache: not reported over {} turn(s) — the harness can supply it via \
                 context.record_usage",
                self.turns
            );
        }
        let regressions = if self.prefix_breaks == 0 {
            "no prefix breaks".to_string()
        } else {
            format!(
                "{} PREFIX BREAK(S) — compaction rewrote already-sent history",
                self.prefix_breaks
            )
        };
        format!(
            "provider cache over {} of {} turn(s): {} of {} prompt tokens served from cache \
             ({:.0}%) · {regressions}",
            self.turns_reporting,
            self.turns,
            self.total_cache_read_tokens,
            self.total_prompt_tokens,
            self.overall_hit_ratio() * 100.0
        )
    }
}

/// Decide the verdict for a new turn given the previous turn's numbers.
///
/// # The rule, and why it is this rule
///
/// Append-only growth is the healthy case: the prompt grows, and the cached
/// prefix grows with it, because everything already sent is still a prefix of
/// what is being sent now.
///
/// A compaction inverts that. Rewriting history replaces a long prefix with a
/// shorter one, so the cached prefix *shrinks* even as the prompt stays large —
/// and the next request is billed for the difference. That inversion is the
/// detectable signature, and it is why the comparison is against the previous
/// turn's cached tokens rather than against a fixed ratio.
///
/// The threshold is deliberately generous: a provider's cache has a minimum
/// block size and expires on its own schedule, so a small dip is noise. Only a
/// drop larger than a quarter of the previous cached prefix, on a turn that did
/// not itself get much smaller, counts as a break.
pub fn verdict_for(
    current: &ProviderUsage,
    previous: Option<&ProviderUsage>,
) -> (ProviderCacheVerdict, String) {
    if !current.reports_cache() {
        return (
            ProviderCacheVerdict::NotReported,
            "the provider returned no prompt-cache fields for this turn".into(),
        );
    }
    let read = current.cache_read_tokens.unwrap_or(0);
    let ratio = current.cache_hit_ratio().unwrap_or(0.0);

    if let Some(prev) = previous {
        let prev_read = prev.cache_read_tokens.unwrap_or(0);
        let prev_prompt = prev.prompt_tokens;
        // Only a *rewrite* counts: the prompt must not have shrunk by as much as
        // the cache did, or the drop is simply explained by a smaller request.
        let prompt_shrank = current.prompt_tokens + prev_prompt / 4 < prev_prompt;
        if prev_read > 0 && !prompt_shrank && read + prev_read / 4 < prev_read {
            let lost = prev_read.saturating_sub(read);
            return (
                ProviderCacheVerdict::PrefixBroken,
                format!(
                    "the provider's cached prefix fell from {prev_read} to {read} tokens \
                     ({lost} tokens no longer cached) while the prompt went {prev_prompt} → {}; \
                     this turn was billed for history that had already been paid for",
                    current.prompt_tokens
                ),
            );
        }
    }

    if read == 0 {
        return (
            ProviderCacheVerdict::CacheMiss,
            format!(
                "0 of {} prompt tokens were cached; either a cold prefix or a rewrite with \
                 nothing left in common with the previous request",
                current.prompt_tokens
            ),
        );
    }
    if ratio >= 0.95 {
        return (
            ProviderCacheVerdict::FullReuse,
            format!(
                "{} of {} prompt tokens served from cache ({:.0}%)",
                read,
                current.prompt_tokens,
                ratio * 100.0
            ),
        );
    }
    (
        ProviderCacheVerdict::PartialReuse,
        format!(
            "{} of {} prompt tokens served from cache ({:.0}%); the remaining {} were processed \
             fresh, which is the normal cost of the new suffix",
            read,
            current.prompt_tokens,
            ratio * 100.0,
            current.prompt_tokens.saturating_sub(read)
        ),
    )
}

impl Db {
    /// Append a usage record and return the row plus its verdict.
    pub async fn record_provider_usage(
        &self,
        session_id: &str,
        slot_id: Option<&str>,
        usage: ProviderUsage,
    ) -> Result<UsageRecord> {
        let session = session_id.to_string();
        let slot = slot_id.map(String::from);
        let previous = self.last_provider_usage(session_id).await?;
        let (verdict, detail) = verdict_for(&usage, previous.as_ref());

        let turn: i64 = self
            .with({
                let s = session.clone();
                move |c| {
                    Ok(c.query_row(
                        "SELECT COALESCE(MAX(turn), 0) + 1 FROM provider_usage WHERE session_id = ?1",
                        [s],
                        |r| r.get(0),
                    )
                    .unwrap_or(1))
                }
            })
            .await?;

        let row = usage.clone();
        let id = new_id("usage");
        let now = now_rfc3339();
        self.write(move |tx| {
            tx.execute(
                "INSERT INTO provider_usage
                    (usage_id, session_id, slot_id, turn, prompt_tokens, completion_tokens,
                     total_tokens, cache_read_tokens, cache_write_tokens, reasoning_tokens,
                     provider, model, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
                rusqlite::params![
                    id,
                    session,
                    slot,
                    turn,
                    row.prompt_tokens as i64,
                    row.completion_tokens as i64,
                    row.total_tokens.unwrap_or(row.prompt_tokens + row.completion_tokens) as i64,
                    row.cache_read_tokens.map(|v| v as i64),
                    row.cache_write_tokens.map(|v| v as i64),
                    row.reasoning_tokens.map(|v| v as i64),
                    row.provider,
                    row.model,
                    now,
                ],
            )?;
            Ok(())
        })
        .await?;

        Ok(UsageRecord {
            turn,
            usage,
            verdict,
            previous_cache_read_tokens: previous.and_then(|p| p.cache_read_tokens),
            detail,
        })
    }

    /// The most recent recorded usage for a session.
    pub async fn last_provider_usage(&self, session_id: &str) -> Result<Option<ProviderUsage>> {
        let session = session_id.to_string();
        self.with(move |c| {
            Ok(c.query_row(
                "SELECT prompt_tokens, completion_tokens, total_tokens, cache_read_tokens,
                        cache_write_tokens, reasoning_tokens, provider, model
                 FROM provider_usage WHERE session_id = ?1 ORDER BY turn DESC LIMIT 1",
                [session],
                |r| {
                    Ok(ProviderUsage {
                        prompt_tokens: r.get::<_, i64>(0)?.max(0) as usize,
                        completion_tokens: r.get::<_, i64>(1)?.max(0) as usize,
                        total_tokens: r.get::<_, Option<i64>>(2)?.map(|v| v.max(0) as usize),
                        cache_read_tokens: r.get::<_, Option<i64>>(3)?.map(|v| v.max(0) as usize),
                        cache_write_tokens: r.get::<_, Option<i64>>(4)?.map(|v| v.max(0) as usize),
                        reasoning_tokens: r.get::<_, Option<i64>>(5)?.map(|v| v.max(0) as usize),
                        provider: r.get(6)?,
                        model: r.get(7)?,
                    })
                },
            )
            .ok())
        })
        .await
    }

    /// Aggregate provider-cache statistics for a session (or every session).
    pub async fn provider_cache_stats(&self, session_id: Option<&str>) -> Result<ProviderCacheStats> {
        let session = session_id.map(String::from);
        self.with(move |c| {
            let mut stmt = c.prepare(
                "SELECT prompt_tokens, cache_read_tokens FROM provider_usage
                 WHERE (?1 IS NULL OR session_id = ?1) ORDER BY session_id, turn",
            )?;
            let rows = stmt.query_map(rusqlite::params![session], |r| {
                Ok((
                    r.get::<_, i64>(0)?.max(0) as usize,
                    r.get::<_, Option<i64>>(1)?.map(|v| v.max(0) as usize),
                ))
            })?;

            let mut stats = ProviderCacheStats::default();
            let mut previous: Option<ProviderUsage> = None;
            for row in rows {
                let (prompt, cached) = row?;
                stats.turns += 1;
                if cached.is_some() {
                    stats.turns_reporting += 1;
                }
                stats.total_prompt_tokens += prompt;
                stats.total_cache_read_tokens += cached.unwrap_or(0);
                let current = ProviderUsage {
                    prompt_tokens: prompt,
                    cache_read_tokens: cached,
                    ..Default::default()
                };
                match verdict_for(&current, previous.as_ref()).0 {
                    ProviderCacheVerdict::PrefixBroken => stats.prefix_breaks += 1,
                    ProviderCacheVerdict::CacheMiss => stats.cache_misses += 1,
                    _ => {}
                }
                previous = Some(current);
            }
            Ok(stats)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage_of(prompt: usize, cached: Option<usize>) -> ProviderUsage {
        ProviderUsage {
            prompt_tokens: prompt,
            cache_read_tokens: cached,
            ..Default::default()
        }
    }

    #[test]
    fn a_provider_that_reports_nothing_is_not_reported_not_a_miss() {
        // The distinction matters: a provider that never sends cache fields cannot
        // be diagnosed by a number it never sent.
        let (verdict, detail) = verdict_for(&usage_of(1000, None), None);
        assert_eq!(verdict, ProviderCacheVerdict::NotReported);
        assert!(!verdict.is_regression());
        assert!(detail.contains("no prompt-cache fields"));
        assert_eq!(usage_of(1000, None).cache_hit_ratio(), None);
    }

    #[test]
    fn append_only_growth_is_partial_reuse() {
        // Turn one: 5000 prompt tokens, 4000 cached.
        let first = usage_of(5000, Some(4000));
        // Turn two appends: prompt grows, cached prefix grows with it.
        let second = usage_of(6000, Some(5000));
        let (verdict, detail) = verdict_for(&second, Some(&first));
        assert_eq!(verdict, ProviderCacheVerdict::PartialReuse);
        assert!(!verdict.is_regression());
        assert!(detail.contains("5000 of 6000"), "{detail}");
    }

    #[test]
    fn a_rewrite_that_shrinks_the_prefix_is_a_regression() {
        // The signature: the prompt stays large while the cached prefix collapses.
        let before = usage_of(20000, Some(18000));
        let after = usage_of(19000, Some(2000));
        let (verdict, detail) = verdict_for(&after, Some(&before));
        assert_eq!(verdict, ProviderCacheVerdict::PrefixBroken);
        assert!(verdict.is_regression());
        assert!(detail.contains("18000 to 2000"), "{detail}");
        assert!(detail.contains("already been paid for"), "{detail}");
    }

    #[test]
    fn a_smaller_request_is_not_mistaken_for_a_broken_prefix() {
        // The prompt shrank by more than a quarter, so the cache shrinking with it
        // is explained by the smaller request — not by a rewrite.
        let before = usage_of(20000, Some(18000));
        let after = usage_of(5000, Some(4000));
        let (verdict, _) = verdict_for(&after, Some(&before));
        assert_ne!(
            verdict,
            ProviderCacheVerdict::PrefixBroken,
            "a genuinely smaller request must not be reported as a cache regression"
        );
    }

    #[test]
    fn a_small_dip_is_noise_not_a_break() {
        // Providers evict cache blocks on their own schedule and have a minimum
        // block size; a 10% dip is not a compaction.
        let before = usage_of(20000, Some(18000));
        let after = usage_of(21000, Some(17000));
        let (verdict, _) = verdict_for(&after, Some(&before));
        assert_eq!(verdict, ProviderCacheVerdict::PartialReuse);
    }

    #[test]
    fn a_cold_start_is_a_cache_miss() {
        let (verdict, detail) = verdict_for(&usage_of(3000, Some(0)), None);
        assert_eq!(verdict, ProviderCacheVerdict::CacheMiss);
        assert!(!verdict.is_regression());
        assert!(detail.contains("cold prefix"));
    }

    #[test]
    fn near_total_reuse_is_recognised() {
        let (verdict, _) = verdict_for(&usage_of(10000, Some(9900)), None);
        assert_eq!(verdict, ProviderCacheVerdict::FullReuse);
    }

    #[test]
    fn uncached_tokens_are_derived_consistently() {
        let u = usage_of(6000, Some(5000));
        assert_eq!(u.uncached_prompt_tokens(), Some(1000));
        assert!((u.cache_hit_ratio().unwrap() - 5000.0 / 6000.0).abs() < 1e-9);
        // A provider reporting more cached than prompt tokens is clamped, not
        // allowed to produce a negative or a ratio above one.
        let odd = usage_of(100, Some(150));
        assert_eq!(odd.uncached_prompt_tokens(), Some(0));
        assert_eq!(odd.cache_hit_ratio(), Some(1.0));
    }

    #[tokio::test]
    async fn usage_records_persist_and_aggregate() {
        let db = Db::open_in_memory().await.unwrap();

        let r1 = db
            .record_provider_usage("s1", Some("0"), usage_of(5000, Some(0)))
            .await
            .unwrap();
        assert_eq!(r1.turn, 1);
        assert_eq!(r1.verdict, ProviderCacheVerdict::CacheMiss);

        let r2 = db
            .record_provider_usage("s1", Some("0"), usage_of(6000, Some(5000)))
            .await
            .unwrap();
        assert_eq!(r2.turn, 2);
        assert_eq!(r2.verdict, ProviderCacheVerdict::PartialReuse);
        assert_eq!(r2.previous_cache_read_tokens, Some(0));

        // A rewrite: the prefix collapses while the prompt stays large.
        let r3 = db
            .record_provider_usage("s1", Some("0"), usage_of(6200, Some(300)))
            .await
            .unwrap();
        assert_eq!(r3.verdict, ProviderCacheVerdict::PrefixBroken);
        assert!(r3.detail.contains("no longer cached"));

        let stats = db.provider_cache_stats(Some("s1")).await.unwrap();
        assert_eq!(stats.turns, 3);
        assert_eq!(stats.turns_reporting, 3);
        assert_eq!(stats.prefix_breaks, 1);
        // 0 (cold) + 5000 (warm) + 300 (after the rewrite).
        assert_eq!(stats.total_cache_read_tokens, 5300);
        assert!(stats.render().contains("PREFIX BREAK"));
        assert!(stats.overall_hit_ratio() > 0.2);
    }

    #[tokio::test]
    async fn a_session_with_no_reported_usage_says_so_rather_than_claiming_zero() {
        let db = Db::open_in_memory().await.unwrap();
        db.record_provider_usage("s1", None, usage_of(1000, None))
            .await
            .unwrap();
        let stats = db.provider_cache_stats(Some("s1")).await.unwrap();
        assert_eq!(stats.turns, 1);
        assert_eq!(stats.turns_reporting, 0);
        assert!(stats.render().contains("not reported"));
    }
}
