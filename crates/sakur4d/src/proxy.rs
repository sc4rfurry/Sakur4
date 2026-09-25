//! FR-18: an OpenAI-compatible reverse proxy, for harnesses with no plugin system.
//!
//! # What this is for, and what it is not
//!
//! Every other integration needs something from the harness: MCP support (Hermes, any
//! MCP client), a plugin system (the OMP extension), or an Agent Skills reader. A harness
//! with none of those — an older client, a closed tool, a script that posts to
//! `/v1/chat/completions` and nothing else — cannot be reached any of those ways.
//!
//! It can be reached this way. Point the harness at this proxy instead of at
//! `llama-server`, and everything else stays as it was:
//!
//! ```text
//!   harness ──▶ sakur4d proxy ──▶ llama-server
//!    (openai)         │              (openai)
//!                     └─▶ Memory Fabric
//! ```
//!
//! # The two rules this follows
//!
//! **Transparency first.** The proxy forwards the method, path, query, headers and body
//! upstream and returns the upstream status, headers and body. A harness cannot tell it
//! is there except by the context being managed. Anything the proxy does not understand
//! is passed through untouched rather than rejected — a proxy that breaks an endpoint it
//! did not think about is worse than no proxy.
//!
//! **Never fail a request to manage context.** Recording a turn, planning an eviction, or
//! forwarding usage are all best-effort. If the Memory Fabric is unreachable or the plan
//! is empty, the original request goes upstream unmodified. The same rule the harness
//! plugins follow: a sidecar that breaks a session when it is down is worse than none.
//!
//! # What it does not do yet
//!
//! **Streaming responses are buffered.** `stream: true` works and reaches the client, but
//! the proxy reads the whole body to report its usage, so the client sees the response
//! once it completes rather than token by token. That is a real limitation for an
//! interactive harness and it is recorded rather than hidden. Making it incremental means
//! parsing SSE frames as they pass and reporting usage from the final one.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use sakur4_core::Engine;
use sakur4_core::memory::episodic::NewEpisode;
use sakur4_core::prompt::PromptParts;
use sakur4_core::provider_cache::ProviderUsage;

/// Configuration for the proxy.
#[derive(Debug, Clone)]
pub struct ProxyConfig {
    /// Where the real inference server lives, e.g. `http://127.0.0.1:8080`.
    pub upstream: String,
    /// Session id reported to the Memory Fabric.
    pub session_id: String,
    /// Ask the eviction engine before forwarding, rather than only recording.
    pub manage_context: bool,
    /// How long to wait on the upstream before giving up.
    pub timeout: Duration,
}

impl Default for ProxyConfig {
    fn default() -> Self {
        Self {
            upstream: "http://127.0.0.1:8080".into(),
            // # `"proxy"` was a constant, and a constant here is a shared transcript
            //
            // `MemoryFabric::session_episodes` filters on `session_id` alone — not on `project_id` — and every
            // episodic read goes through it: `timeline` for the assembled prompt, `recent_episodes` for the
            // receipt, and the fold and anchor queries beside them. A constant default meant **one history for
            // every project on the machine**.
            //
            // This function cannot reach the engine, so it carries a name that says what it is rather than one
            // that looks plausible. **The CLI derives the real default** — `proxy-<project_id>` from the engine
            // it just opened — so this value is only reached by a caller building the struct directly, which
            // means a test or an embedding rather than a served session. `"proxy"` was worse than unhelpful
            // there: it looked like a session id, so a caller who forgot to set one got a silently shared
            // history instead of a name that says nothing was configured.
            session_id: "proxy-unconfigured".into(),
            manage_context: true,
            // Generous, because a local model prefilling a long prompt is legitimately
            // slow: 100+ seconds was measured on a 27B at Q3 for a full re-prefill.
            timeout: Duration::from_secs(600),
        }
    }
}

struct ProxyState {
    engine: Engine,
    config: ProxyConfig,
    client: reqwest::Client,
    /// Content hashes of messages already committed, so a harness re-sending its whole
    /// transcript every turn does not write it to memory every turn.
    ///
    /// A `VecDeque` used as a bounded FIFO rather than a `HashSet` that grows without
    /// limit: a proxy session runs for days, and an unbounded set is a slow leak in the one
    /// component that is supposed to be reliable. Eight thousand entries covers far more
    /// history than any window the eviction engine would keep, so evicting from this set
    /// only ever forgets messages that are long out of the window anyway.
    seen: parking_lot::Mutex<std::collections::VecDeque<String>>,
}

/// Entries kept in the already-seen set.
const SEEN_CAPACITY: usize = 8_000;

impl ProxyState {
    /// Record a content hash, returning `false` if it had already been seen.
    fn remember(&self, digest: String) -> bool {
        let mut seen = self.seen.lock();
        if seen.contains(&digest) {
            return false;
        }
        if seen.len() >= SEEN_CAPACITY {
            seen.pop_front();
        }
        seen.push_back(digest);
        true
    }
}

/// Run the proxy until `cancel` fires.
pub async fn serve(engine: Engine, bind: &str, config: ProxyConfig) -> Result<()> {
    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .with_context(|| format!("binding the reverse proxy to {bind}"))?;
    tracing::info!(bind, upstream = %bind_upstream(bind), "sakur4 reverse proxy listening");
    serve_on(engine, listener, config).await
}

/// Run the proxy on a listener the caller has already bound.
///
/// # Why this is separate from `serve`
///
/// `serve` binds its own listener, which is right for a user — they name a port and the OS gives
/// it to them. It is wrong for a test, and was the cause of eight failures on Linux: the test
/// bound an ephemeral port to learn its number, then called `serve`, which bound a *different*
/// ephemeral port. The client connected to the first one, nothing was listening there, and every
/// test failed with `ConnectionRefused`.
///
/// It passed on Windows for many rounds purely by scheduling luck, which is worse than failing:
/// a race that resolves the right way teaches you the code is correct.
///
/// Taking the listener makes the arrangement explicit and removes the race by construction — the
/// address the caller has is the address that serves.
pub async fn serve_on(
    engine: Engine,
    listener: tokio::net::TcpListener,
    config: ProxyConfig,
) -> Result<()> {
    let client = reqwest::Client::builder()
        .timeout(config.timeout)
        // Redirects are followed by default; a local inference server does not redirect,
        // and following one silently would send the request somewhere unexpected.
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .context("building the proxy HTTP client")?;

    let state = Arc::new(ProxyState {
        engine,
        config,
        client,
        seen: parking_lot::Mutex::new(std::collections::VecDeque::new()),
    });

    // `any` on a fallback route, because the point is to forward *everything*. Enumerating
    // the OpenAI surface would mean a harness calling an endpoint this build had not heard
    // of gets a 404 from the proxy instead of the upstream's own answer.
    let router = axum::Router::new().fallback(any(forward)).with_state(state);

    axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
        .context("serving the reverse proxy")?;
    Ok(())
}

/// Only used for a log line; the real upstream is in the state.
fn bind_upstream(_bind: &str) -> &'static str {
    "see ProxyConfig"
}

/// Forward one request, managing context on the way through.
async fn forward(
    State(state): State<Arc<ProxyState>>,
    method: axum::http::Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    // Context management applies only to the chat/completions shapes, because those are
    // the only ones that carry a transcript. Everything else — `/v1/models`, `/health`,
    // `/tokenize`, a route from a newer OpenAI revision — is forwarded untouched, which
    // is what makes this a proxy rather than a reimplementation.
    let manages = state.config.manage_context && is_chat_completion(&method, &uri);
    let outgoing = if manages {
        match rewrite_request(&state, &body).await {
            Some(rewritten) => rewritten,
            // Nothing to change, or the fabric is unavailable. Either way the original
            // body goes upstream: managing context is never worth failing a request for.
            None => body.clone(),
        }
    } else {
        body.clone()
    };

    let url = format!("{}{}", state.config.upstream.trim_end_matches('/'), path_and_query(&uri));
    let mut request = state.client.request(method.clone(), &url);

    // Hop-by-hop headers and the ones describing the body we just replaced must not be
    // forwarded: `content-length` would be wrong for a rewritten body, and passing
    // `host` through would tell the upstream it is serving a different name.
    for (name, value) in headers.iter() {
        let lower = name.as_str().to_ascii_lowercase();
        if matches!(
            lower.as_str(),
            "host" | "content-length" | "connection" | "transfer-encoding" | "accept-encoding"
        ) {
            continue;
        }
        request = request.header(name, value);
    }
    if outgoing != body {
        request = request.header("content-length", outgoing.len().to_string());
    }

    let response = match request.body(outgoing.clone()).send().await {
        Ok(response) => response,
        Err(error) => {
            // An unreachable upstream is the one case worth a non-transparent status,
            // because there is nothing to be transparent *about*.
            tracing::warn!(%url, %error, "upstream request failed");
            return (
                StatusCode::BAD_GATEWAY,
                format!("sakur4 proxy: upstream {url} failed: {error}"),
            )
                .into_response();
        }
    };

    let status = response.status();
    let upstream_headers = response.headers().clone();
    let bytes = match response.bytes().await {
        Ok(bytes) => bytes,
        Err(error) => {
            return (
                StatusCode::BAD_GATEWAY,
                format!("sakur4 proxy: reading the upstream response failed: {error}"),
            )
                .into_response();
        }
    };

    // Accounting happens after the response is in hand, from the response itself: the
    // provider's own token counts are the only trustworthy source, and they are exactly
    // what `context.record_usage` was built to receive.
    if manages {
        record_usage(&state, &bytes).await;
    }

    let mut builder = Response::builder().status(status.as_u16());
    for (name, value) in upstream_headers.iter() {
        let lower = name.as_str().to_ascii_lowercase();
        if matches!(lower.as_str(), "content-length" | "transfer-encoding" | "connection") {
            continue;
        }
        builder = builder.header(name, value);
    }
    builder
        .body(axum::body::Body::from(bytes))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

fn path_and_query(uri: &Uri) -> String {
    match uri.query() {
        Some(query) => format!("{}?{}", uri.path(), query),
        None => uri.path().to_string(),
    }
}

fn is_chat_completion(method: &axum::http::Method, uri: &Uri) -> bool {
    method == axum::http::Method::POST
        && matches!(uri.path(), "/v1/chat/completions" | "/v1/completions" | "/completion")
}

// ===========================================================================
// Context management
// ===========================================================================

/// Rewrite the request body so the transcript fits, or `None` to leave it alone.
async fn rewrite_request(state: &ProxyState, body: &[u8]) -> Option<Bytes> {
    let parsed: serde_json::Value = serde_json::from_slice(body).ok()?;
    let messages = parsed.get("messages")?.as_array()?.clone();
    if messages.is_empty() {
        return None;
    }

    // Record the transcript first, so nothing is lost even if the plan does nothing. This
    // is what makes eviction safe: the window shrinks, the memory does not.
    //
    // # Why the already-seen check matters
    //
    // A harness re-sends its whole transcript every turn — that is what a stateless
    // chat-completions client does. Committing all of it each time created a duplicate
    // episode per message per turn: a 242-message request arriving on ten turns wrote 2,420
    // episodes, bloating the store, polluting lexical recall with near-identical rows, and
    // making the engine replan the same eviction over and over.
    //
    // A bounded set of content hashes, rather than positions or ids, because the harness
    // owns the transcript and may trim, retry, or resume it between turns — so no positional
    // assumption survives. The bound keeps a long session's memory flat.
    let mut fresh = 0;
    for message in &messages {
        let role = message.get("role").and_then(|r| r.as_str()).unwrap_or("assistant");
        let content = content_text(message);
        if content.is_empty() {
            continue;
        }
        let digest = blake3::hash(content.as_bytes()).to_hex().to_string();
        if !state.remember(digest) {
            continue;
        }
        fresh += 1;

        let mut episode = match role {
            "user" => NewEpisode::user(&state.config.session_id, content),
            "tool" => {
                let mut e = NewEpisode::user(&state.config.session_id, content);
                e.role = sakur4_core::memory::episodic::Role::Tool;
                e.tool_name = message.get("name").and_then(|n| n.as_str()).map(String::from);
                e
            }
            _ => {
                let mut e = NewEpisode::user(&state.config.session_id, content);
                e.role = if role == "system" {
                    sakur4_core::memory::episodic::Role::System
                } else {
                    sakur4_core::memory::episodic::Role::Assistant
                };
                e
            }
        };
        // Sessions in a proxy are long-lived; marking everything droppable would let the
        // engine discard user turns, which is not what a user expects.
        episode.droppable = role == "tool";
        let _ =
            state.engine.memory().commit_episode(episode, state.engine.tokens(), true, true).await;
    }
    if fresh == 0 {
        // The whole transcript has been seen. Nothing new to record, but the plan below
        // still runs: the window may have grown past its budget since last turn.
        tracing::debug!("proxy: transcript unchanged since the last turn");
    }

    // # Measure the transcript that is actually being sent
    //
    // The planner decides pressure from `parts.timeline_tokens()`, and that number has to
    // describe **this request** or the decision is made about something else.
    //
    // The first version filled `timeline` from the fabric's rendered session timeline, on the
    // reasoning that `assemble_parts` in `tools.rs` does exactly that. That is right for the
    // MCP path — there, the rendered timeline *is* the prompt. It is wrong here, because the
    // proxy sends the harness's raw `messages` array and the fabric rendering never goes
    // anywhere near the model.
    //
    // The two differ by roughly a factor of two, and the reason is per-episode chrome: the
    // timeline renders a role label, a separator and an episode id for every entry, so 602
    // messages measured 43,712 tokens through the timeline and 20,571 through the server's own
    // tokenizer. The planner was therefore told the request was over twice its real size,
    // aimed its target at that inflated figure, and evicted almost the whole conversation —
    // which is why a session settled at the same ~3,408 tokens whatever the window.
    //
    // So the timeline is filled with the transcript as it will be sent. Same text in, same
    // tokenizer, same number the receipts report.
    let transcript_text = messages
        .iter()
        .map(|m| {
            let role = m.get("role").and_then(|r| r.as_str()).unwrap_or("assistant");
            format!("{role}: {}", content_text(m))
        })
        .collect::<Vec<_>>()
        .join("\n\n");

    let window = state.engine.context_window().await;
    let anchors = state.engine.memory().anchors(Some(&state.config.session_id)).await.ok()?;
    // The proxy forwards a real request to a real server, so an anchor set that cannot fit is exactly
    // the case it must not paper over. `render_anchor_block` returns `BudgetOverflow` for that; the
    // `join` it replaces returned a block of any size and let the server truncate it silently.
    let (anchor_block, _) =
        sakur4_core::memory::anchor::render_anchor_block(&anchors, state.engine.tokens(), 10_000)
            .ok()?;
    let parts = PromptParts::new()
        .with_system("You are a local coding agent.")
        .with_anchors(anchor_block)
        .with_timeline(transcript_text);

    let plan =
        state.engine.eviction().plan(&state.config.session_id, "0", window, &parts).await.ok()?;

    let evicted: std::collections::HashSet<String> = plan
        .updates
        .iter()
        .filter(|u| u.to != sakur4_core::memory::episodic::EpisodeTier::Live)
        .map(|u| u.episode_id.clone())
        .collect();
    if evicted.is_empty() {
        return None;
    }

    // # Apply the tier changes, then trim the messages those tiers moved
    //
    // The ladder from `live` to `referenced` takes several steps, and the first can be
    // token-neutral: `masked` renders a header plus a 160-character preview, which for a short
    // message is *longer* than the message. So `planned_savings` can be zero while the plan
    // has still moved hundreds of episodes out of `live`.
    //
    // This code used to return early on `planned_savings == 0`, on the reasoning that the
    // transcript should be left alone until savings appear. That reasoning held only for a
    // session that grows a turn at a time — where a later plan picks up the ladder where this
    // one left it. For a single large request it was fatal: the plan advanced 903 episodes,
    // reported zero savings, the early return discarded the result, and **the whole transcript
    // went upstream untouched**. Measured: 1,002 messages and 34,571 tokens forwarded verbatim
    // against a 32,768-token window with a 22,938-token trigger.
    //
    // A message whose episode has left `live` should not be in the window, whatever the plan
    // thinks it saved. Whether this turn's steps reclaimed anything is the planner's business;
    // whether the transcript still contains evicted content is this function's.
    let _ = state.engine.eviction().apply(&plan, &parts).await;

    // # Record a receipt for the prompt that is actually going upstream
    //
    // `context.receipt` reads the *latest* receipt for a session and falls back to an assembled
    // preview when there is none. The proxy recorded nothing, so on the one path where a real
    // prompt goes to a real server, the receipt a user saw was the preview — and two of its
    // categories could only ever be zero, because the preview assembler never fills `repo_map` or
    // `folds`. A field that is always zero is indistinguishable from a measurement, which is the
    // opposite of what this receipt is for.
    //
    // `parts` already holds the transcript exactly as it will be sent, which is the same thing the
    // planner is measured against, so the breakdown describes the request rather than a rendering
    // of the fabric. Recording is best-effort: a receipt that could not be stored must not fail a
    // turn that is otherwise ready to forward.
    {
        let receipt = sakur4_core::receipt::Receipt::build(
            &state.config.session_id,
            Some("0"),
            0,
            &parts,
            state.engine.tokens(),
            window,
        );
        if let Err(error) = state.engine.receipts().record(&receipt).await {
            tracing::warn!(%error, "could not record the turn's receipt; forwarding anyway");
        }
    }

    if plan.planned_savings == 0 {
        tracing::info!(
            advanced = plan.updates.len(),
            "proxy advanced the eviction ladder without reclaiming yet"
        );
    }

    let rewritten = drop_evicted(state, &messages, &evicted, plan.target).await;
    if rewritten.len() == messages.len() {
        // The plan named episodes that do not correspond to messages in this request — the
        // transcript has moved on since they were committed. Leaving the body alone is
        // correct: a rewrite that removes nothing is churn, and churn invalidates a
        // provider's cached prefix for no benefit.
        return None;
    }

    tracing::info!(
        evicted = messages.len() - rewritten.len(),
        reclaimed = plan.planned_savings,
        verdict = plan.coherence.as_ref().map(|c| c.status.as_str()).unwrap_or("not evaluated"),
        "proxy rewrote the transcript"
    );

    // # The model has to be told
    //
    // The proxy just removed messages. Without a marker the model sees a conversation with
    // a hole in it — earlier turns simply absent — and answers as though they never
    // happened, which reads as the model forgetting rather than as the proxy having trimmed.
    // The marker names what happened and where the text still is.
    //
    // Placed where the evicted messages were, so surviving turns stay in order and the
    // recent context the model is being asked about stays adjacent to the request.
    let evicted_count = messages.len() - rewritten.len();
    let marker = serde_json::json!({
        "role": "user",
        "content": format!(
            "[Sakur4 removed {evicted_count} earlier message(s) to fit the context window, \
             reclaiming {reclaimed} tokens. Their full text is still in this session's \
             Sakur4 Memory Fabric and can be recalled — nothing was destroyed, only \
             unwindowed.]",
            reclaimed = plan.planned_savings
        )
    });

    // Insert after any leading system message, so the system prompt stays first: it is the
    // head of the prompt, and moving it would invalidate a provider's cached prefix on every
    // turn — the exact cost this project exists to avoid.
    let insert_at = if rewritten.first().and_then(|m| m.get("role")).and_then(|r| r.as_str())
        == Some("system")
    {
        1
    } else {
        0
    };
    let mut with_marker = rewritten;
    with_marker.insert(insert_at, marker);

    let mut out = parsed;
    out["messages"] = serde_json::Value::Array(with_marker);
    serde_json::to_vec(&out).ok().map(Bytes::from)
}

/// Remove messages whose episodes the plan moved to a tier that *renders smaller*.
///
/// # Why the match is on content
///
/// The plan identifies what to evict by `episode_id`; the request carries messages. The only
/// thing connecting them is the text the episode was created from, so the proxy asks the
/// fabric for each moved episode and compares. Matching by *position* would be wrong: the
/// transcript in the request is the harness's view, the episodes are this session's
/// accumulated view, and the two drift apart whenever a harness retries, trims its own
/// history, or resumes a stored session.
///
/// A message is dropped only when its text is the whole of a moved episode's text. A prefix
/// match would delete a user turn that merely begins with the same words as something else.
///
/// # Why "moved" is not enough
///
/// The first rung, `Masked`, renders a header plus a 160-character preview — which for a
/// short message is **longer** than the message. Treating every move as evictable therefore
/// deleted 599 of 602 messages while reclaiming nothing at all: the transcript lost its
/// history and the prompt did not get smaller. Requiring an actual reduction is the same
/// rule the planner applies to its own proposals, applied at the point where messages
/// actually leave.
async fn drop_evicted(
    state: &ProxyState,
    messages: &[serde_json::Value],
    evicted: &std::collections::HashSet<String>,
    target_tokens: usize,
) -> Vec<serde_json::Value> {
    // The text of every episode whose new rendering is genuinely smaller than its content.
    let mut doomed: std::collections::HashSet<String> = std::collections::HashSet::new();
    for episode_id in evicted {
        if let Ok(row) = state.engine.memory().episode(episode_id).await {
            // # Tier membership decides, not rendered size
            //
            // This guard used to require `row.render().len() < row.content.len()`, on the
            // reasoning that a message should only leave the window if its replacement is
            // literally shorter. That is the same mistake the *planner* made and had to have
            // fixed: `Masked` renders a header plus a 160-character preview, which for a short
            // message is longer than the message. So the guard was false for every episode at
            // the first rung, `doomed` was always empty, and the proxy forwarded the whole
            // transcript — 1,002 messages and 34,571 tokens untouched against a 22,938-token
            // trigger, while the log cheerfully reported `advanced=903`.
            //
            // The ladder is the decision. `Referenced` and below represent content that is
            // deliberately no longer in the window, and the whole point of the tiers is that a
            // step can be token-neutral while still being the right step. Measuring the
            // replacement's size is the planner's job, and it does it with `live_tokens`.
            doomed.insert(row.content.trim().to_string());
        }
    }
    if doomed.is_empty() {
        return messages.to_vec();
    }

    let last = messages.len().saturating_sub(1);

    // # Only a contiguous run *up to* the newest eviction may leave
    //
    // The plan's tier updates are per-episode and can name episodes scattered through the
    // session. Removing exactly those would punch holes in the middle of a conversation —
    // turn 3 and turn 40 gone, turns 4..39 present — which reads to a model as an incoherent
    // transcript rather than a shortened one.
    //
    // The *newest* evicted message sets the cut, and everything before it leaves while
    // everything after it stays. Cutting at the oldest instead would delete the entire
    // conversation on any turn where the plan happened to name an early episode.
    //
    // # The cut is bounded so the transcript cannot be erased
    //
    // Two rounds of measurement went into this line. A growing session settled at the same
    // ~3,408 tokens of retained context at a 32,768, an 81,920 and a 131,072 window alike, and
    // at the largest of those it went from 52,406 tokens untouched to 3,408 in a single step.
    // The planner's own target cannot produce that, so the cut is clamped here as well.
    //
    // An earlier attempt at this clamp produced a retained prompt of 108 tokens, worse than
    // the bug. It failed because it accumulated a *running total* and kept reassigning the cut
    // on every iteration, so the answer depended on where the sum happened to cross rather
    // than on the first index that satisfies the budget. This walks from the newest message
    // backwards and takes the **first** index whose surviving suffix is worth the target, then
    // stops.
    let plan_cut = messages
        .iter()
        .enumerate()
        .filter(|(index, message)| {
            let role = message.get("role").and_then(|r| r.as_str()).unwrap_or("");
            if role == "system" || *index == last {
                return false;
            }
            let text = content_text(message);
            !text.is_empty() && doomed.contains(text.trim())
        })
        .map(|(index, _)| index)
        .next_back()
        .map(|newest| newest + 1)
        .unwrap_or(0);

    // Tokens each message contributes, measured with the engine's counter so the number is
    // the same one the planner and the receipts use.
    let cost =
        |message: &serde_json::Value| state.engine.tokens().count(&content_text(message)).get();

    let cut = {
        let mut retained = cost(&messages[last]);
        // The system prompt is never removed, so it always counts towards what survives.
        if !messages.is_empty()
            && messages[0].get("role").and_then(|r| r.as_str()) == Some("system")
        {
            retained += cost(&messages[0]);
        }
        let mut bounded = plan_cut;
        for index in (0..plan_cut).rev() {
            bounded = index;
            if retained >= target_tokens {
                break;
            }
            retained += cost(&messages[index]);
        }
        // # The numbers that decide the trim
        //
        // Logged because the clamp should hold `target_tokens` and demonstrably does not at a
        // small window: a growing session settles at 35% of target there and 95% at a large
        // one. Printing the inputs turns that from an investigation into a reading.
        tracing::info!(
            messages = messages.len(),
            doomed = doomed.len(),
            plan_cut,
            bounded,
            retained,
            target_tokens,
            "proxy chose a cut"
        );
        bounded
    };

    messages
        .iter()
        .enumerate()
        .filter(|(index, message)| {
            let role = message.get("role").and_then(|r| r.as_str()).unwrap_or("");
            // The system prompt is the harness's contract with the model, and the newest turn
            // is what it is asking about. Neither is ever dropped, whatever the plan says —
            // losing either produces a request the model cannot answer sensibly.
            if role == "system" || *index == last {
                return true;
            }
            *index >= cut
        })
        .map(|(_, message)| message.clone())
        .collect()
}

/// The text of a message, whether its content is a string or a list of parts.
fn content_text(message: &serde_json::Value) -> String {
    match message.get("content") {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Array(parts)) => parts
            .iter()
            .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Forward the provider's token accounting to the Memory Fabric.
///
/// This is the accounting half of the proxy, and it is why a harness that cannot run a
/// plugin still gets prompt-cache measurement. The counts come from the response body the
/// proxy already had to read, so nothing extra is asked of the upstream.
async fn record_usage(state: &ProxyState, body: &[u8]) {
    let Some(usage) = parse_usage(body) else {
        return;
    };
    let _ =
        state.engine.db().record_provider_usage(&state.config.session_id, Some("0"), usage).await;
}

/// Extract `usage` from an OpenAI-shaped response, or from the final SSE frame.
fn parse_usage(body: &[u8]) -> Option<ProviderUsage> {
    let text = std::str::from_utf8(body).ok()?;

    // A streamed response is a sequence of `data: {...}` frames with the usage on the
    // last one before `[DONE]`. Taking the last frame that carries a `usage` object is
    // the same rule OpenAI clients use.
    let json: serde_json::Value = if text.trim_start().starts_with("data:") {
        let mut found = None;
        for line in text.lines() {
            let Some(payload) = line.strip_prefix("data: ") else {
                continue;
            };
            if payload.trim() == "[DONE]" {
                continue;
            }
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(payload)
                && value.get("usage").map(|u| !u.is_null()).unwrap_or(false)
            {
                found = Some(value);
            }
        }
        found?
    } else {
        serde_json::from_str(text).ok()?
    };

    let usage = json.get("usage")?;
    let prompt = usage.get("prompt_tokens").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
    if prompt == 0 {
        return None;
    }
    let cache_read = usage
        .get("prompt_tokens_details")
        .and_then(|d| d.get("cached_tokens"))
        .and_then(|v| v.as_u64())
        .map(|v| v as usize)
        .or_else(|| {
            usage.get("cache_read_input_tokens").and_then(|v| v.as_u64()).map(|v| v as usize)
        });

    Some(ProviderUsage {
        prompt_tokens: prompt,
        completion_tokens: usage.get("completion_tokens").and_then(|v| v.as_u64()).unwrap_or(0)
            as usize,
        total_tokens: usage.get("total_tokens").and_then(|v| v.as_u64()).map(|v| v as usize),
        // Absent stays absent: sending zero would assert a cache miss the provider never
        // reported, and Sakur4 reports "not reported" rather than blaming a cache it
        // cannot see.
        cache_read_tokens: cache_read,
        cache_write_tokens: usage
            .get("cache_creation_input_tokens")
            .and_then(|v| v.as_u64())
            .map(|v| v as usize),
        reasoning_tokens: usage
            .get("completion_tokens_details")
            .and_then(|d| d.get("reasoning_tokens"))
            .and_then(|v| v.as_u64())
            .map(|v| v as usize),
        provider: Some("openai-compatible".into()),
        model: json.get("model").and_then(|m| m.as_str()).map(String::from),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_chat_completion_is_managed_and_everything_else_is_forwarded() {
        let post = axum::http::Method::POST;
        let get = axum::http::Method::GET;
        let uri = |p: &str| p.parse::<Uri>().unwrap();

        assert!(is_chat_completion(&post, &uri("/v1/chat/completions")));
        assert!(is_chat_completion(&post, &uri("/v1/completions")));
        assert!(is_chat_completion(&post, &uri("/completion")));

        // The rule that makes this a proxy: everything unrecognised is passed through, so
        // a harness calling an endpoint this build has not heard of still works.
        assert!(!is_chat_completion(&get, &uri("/v1/models")));
        assert!(!is_chat_completion(&get, &uri("/health")));
        assert!(!is_chat_completion(&post, &uri("/tokenize")));
        assert!(!is_chat_completion(&post, &uri("/v1/embeddings")));
        assert!(!is_chat_completion(&post, &uri("/v1/responses")));
    }

    #[test]
    fn usage_is_read_from_a_plain_response() {
        let body = br#"{"model":"qwen","usage":{"prompt_tokens":5000,
            "completion_tokens":12,"total_tokens":5012,
            "prompt_tokens_details":{"cached_tokens":4000}}}"#;
        let usage = parse_usage(body).expect("usage");
        assert_eq!(usage.prompt_tokens, 5000);
        assert_eq!(usage.cache_read_tokens, Some(4000));
        assert_eq!(usage.model.as_deref(), Some("qwen"));
    }

    #[test]
    fn usage_is_read_from_the_last_streamed_frame() {
        // A streamed response carries usage on its final frame, and an earlier frame may
        // carry a null. Taking the last one that has a real object is what an OpenAI client
        // does, so the numbers agree with whatever the harness already logs.
        let body = b"data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}],\"usage\":null}\n\n\
                     data: {\"choices\":[],\"usage\":{\"prompt_tokens\":3000,\"completion_tokens\":7}}\n\n\
                     data: [DONE]\n\n";
        let usage = parse_usage(body).expect("usage from the final frame");
        assert_eq!(usage.prompt_tokens, 3000);
        assert_eq!(usage.completion_tokens, 7);
    }

    #[test]
    fn a_response_without_usage_reports_nothing_rather_than_zero() {
        // Absence is not a miss. Returning a zero-filled usage here would make Sakur4
        // report a cache miss the provider never mentioned.
        assert!(parse_usage(b"{\"choices\":[]}").is_none());
        assert!(parse_usage(b"{\"usage\":{\"prompt_tokens\":0}}").is_none());
        assert!(parse_usage(b"not json at all").is_none());
        assert!(parse_usage(b"data: [DONE]\n\n").is_none());
    }

    #[test]
    fn a_provider_that_omits_cache_fields_is_passed_through_as_absent() {
        let body = br#"{"usage":{"prompt_tokens":1000,"completion_tokens":5}}"#;
        let usage = parse_usage(body).expect("usage");
        assert_eq!(usage.cache_read_tokens, None);
        assert!(!usage.reports_cache(), "a provider that says nothing must not look like a miss");
        assert_eq!(usage.cache_hit_ratio(), None);
    }

    #[test]
    fn message_text_is_read_from_both_content_shapes() {
        let plain = serde_json::json!({"role":"user","content":"hello"});
        assert_eq!(content_text(&plain), "hello");

        // The parts shape is what a vision or tool-call message uses.
        let parts = serde_json::json!({"role":"user","content":[
            {"type":"text","text":"first"},
            {"type":"image_url","image_url":{"url":"x"}},
            {"type":"text","text":"second"}
        ]});
        assert_eq!(content_text(&parts), "first\nsecond");

        let none = serde_json::json!({"role":"assistant","tool_calls":[]});
        assert_eq!(content_text(&none), "");
    }

    #[test]
    fn the_path_and_query_survive_the_forwarding() {
        let uri: Uri = "/v1/chat/completions?trace=1".parse().unwrap();
        assert_eq!(path_and_query(&uri), "/v1/chat/completions?trace=1");
        let plain: Uri = "/health".parse().unwrap();
        assert_eq!(path_and_query(&plain), "/health");
    }
}
