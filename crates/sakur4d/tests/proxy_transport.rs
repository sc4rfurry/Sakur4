//! FR-18 acceptance: a harness pointed at the proxy works with no other changes.
//!
//! # The criterion, verbatim
//!
//! > A harness configured to point at Sakur4's proxy URL instead of llama-server directly
//! > functions with no other configuration changes.
//!
//! That is a claim about *transparency*, so the test is built around a fake upstream that
//! records exactly what it received. If the proxy alters a request it should not have, or
//! drops a header, or rewrites a path, the recorded evidence shows it — which a test that
//! only checked the status code would miss.
//!
//! The upstream is a real HTTP server on a real port, not a mock object, because the
//! things most likely to break here are HTTP-level: header forwarding, body length,
//! streaming passthrough. `sakur4-testkit` already establishes that pattern for the
//! llama.cpp adapter.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::IntoResponse;
use axum::routing::any;
use sakur4_core::{Engine, EngineConfig};

/// What the fake upstream saw, so assertions can be made about transparency.
#[derive(Default, Clone)]
struct Seen {
    requests: Arc<Mutex<Vec<SeenRequest>>>,
}

#[derive(Clone, Debug)]
struct SeenRequest {
    method: String,
    path: String,
    headers: HashMap<String, String>,
    body: String,
}

/// A fake OpenAI-compatible server that echoes what it received.
async fn fake_upstream(
    State(seen): State<Seen>,
    method: axum::http::Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    let mut map = HashMap::new();
    for (name, value) in headers.iter() {
        map.insert(name.as_str().to_ascii_lowercase(), value.to_str().unwrap_or("").to_string());
    }
    seen.requests.lock().unwrap().push(SeenRequest {
        method: method.to_string(),
        path: uri.path().to_string(),
        headers: map,
        body: String::from_utf8_lossy(&body).to_string(),
    });

    // A response shaped like a real one, so the usage path is exercised rather than skipped.
    let payload = serde_json::json!({
        "id": "chatcmpl-test",
        "model": "fake-model",
        "choices": [{"index": 0, "message": {"role": "assistant", "content": "ok"}, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 1234, "completion_tokens": 5, "total_tokens": 1239,
                  "prompt_tokens_details": {"cached_tokens": 1000}}
    });
    (
        StatusCode::OK,
        [("content-type", "application/json"), ("x-upstream-header", "present")],
        payload.to_string(),
    )
}

/// Wait until something is accepting connections on `addr`.
///
/// # Why every test needed this
///
/// A `TcpListener` that has been bound is not yet *accepting*: `axum::serve` has to be polled
/// before the backlog is drained, and `tokio::spawn` only schedules that. On Windows the spawned
/// task happened to run before the client's first `connect`, so all eight of these tests passed
/// for many rounds. On Linux they failed together with `ConnectionRefused` on the upstream's own
/// ephemeral port — the first CI run on Linux.
///
/// Reading the bind as "the server is up" is the mistake. A connect that succeeds is the only
/// evidence that it is, which is what this waits for. The alternative — sleeping a fixed amount —
/// trades a race for a guess.
async fn wait_until_listening(addr: SocketAddr) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        if tokio::net::TcpStream::connect(addr).await.is_ok() {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "nothing accepted a connection on {addr} within 10s"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

async fn spawn_upstream() -> (String, Seen) {
    let seen = Seen::default();
    let router = axum::Router::new().fallback(any(fake_upstream)).with_state(seen.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind upstream");
    let addr: SocketAddr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    wait_until_listening(addr).await;
    (format!("http://{addr}"), seen)
}

/// Start a proxy over a temporary store, returning its URL and that store's path.
///
/// # Why the store is on disk
///
/// These tests used `db_path: ":memory:"`, which is right for isolation and wrong for the one
/// thing worth asserting about the proxy's *records*: a receipt it wrote cannot be read back from
/// outside the engine that holds it. The path is returned so a test can reopen the store and check
/// what the proxy persisted, which is how `a_rewritten_turn_records_a_receipt` proves the wiring
/// rather than trusting it.
///
/// `tempfile::TempDir` is leaked deliberately: the store must outlive the spawned task, and the
/// OS reclaims it when the test process exits.
async fn spawn_proxy_with_store(upstream: &str, manage: bool) -> (String, std::path::PathBuf) {
    let dir = tempfile::tempdir().expect("temp dir");
    let db_path = dir.keep().join("proxy.db");
    let engine = Engine::open(EngineConfig {
        db_path: db_path.to_string_lossy().to_string(),
        backend: "embedded".into(),
        // Small, so a synthetic transcript can exceed it and the rewriting path is
        // reachable in a test rather than only in production.
        default_n_ctx: 2048,
        context_window_explicit: true,
        ..Default::default()
    })
    .await
    .expect("engine");

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind proxy");
    let addr: SocketAddr = listener.local_addr().expect("addr");
    let config = sakur4d::proxy::ProxyConfig {
        upstream: upstream.to_string(),
        session_id: "proxy-test".into(),
        manage_context: manage,
        ..Default::default()
    };

    // `serve_on`, not `serve`: `serve` binds its own ephemeral port, so the address returned here
    // would name a listener nobody is serving. That mismatch is what failed all eight of these
    // tests on Linux.
    tokio::spawn(async move {
        let _ = sakur4d::proxy::serve_on(engine, listener, config).await;
    });
    wait_until_listening(addr).await;
    (format!("http://{addr}"), db_path)
}

async fn spawn_proxy(upstream: &str, manage: bool) -> String {
    spawn_proxy_with_store(upstream, manage).await.0
}

fn chat_body(messages: serde_json::Value) -> String {
    serde_json::json!({
        "model": "fake-model",
        "messages": messages,
        "temperature": 0.0,
        "max_tokens": 16,
    })
    .to_string()
}

/// Wait until the upstream has recorded `n` requests, then return them.
///
/// # Why this is not an immediate assertion
///
/// The upstream records a request inside its handler, so from the client's side the HTTP
/// call can return before that push is visible. Asserting straight away is a race: it passed
/// in isolation and failed once more tests ran in parallel and the timing shifted.
///
/// A deadline makes the test wait for the thing it asserts about, rather than for a duration
/// someone guessed. If the request never arrives the assertion still fails — and reports the
/// recorded count, so the failure stays diagnosable instead of reading as a timeout.
fn upstream_requests(seen: &Seen, n: usize) -> Vec<SeenRequest> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let recorded = seen.requests.lock().unwrap().clone();
        if recorded.len() >= n || std::time::Instant::now() >= deadline {
            return recorded;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

// ===========================================================================
// Transparency
// ===========================================================================

#[tokio::test]
async fn a_request_reaches_the_upstream_unchanged() {
    // The criterion in its plainest form: the harness sends what it always sent, and the
    // upstream receives exactly that. A proxy that rewrites a request it did not need to
    // touch is worse than no proxy, because the harness has no way to notice.
    let (upstream, seen) = spawn_upstream().await;
    let proxy = spawn_proxy(&upstream, true).await;

    let body = chat_body(serde_json::json!([
        {"role": "system", "content": "you are a coding agent"},
        {"role": "user", "content": "hello"}
    ]));

    let response = reqwest::Client::new()
        .post(format!("{proxy}/v1/chat/completions"))
        .header("content-type", "application/json")
        .header("x-harness-trace", "abc123")
        .body(body.clone())
        .send()
        .await
        .expect("proxy request");

    assert_eq!(response.status(), 200);
    assert_eq!(
        response.headers().get("x-upstream-header").and_then(|v| v.to_str().ok()),
        Some("present"),
        "upstream response headers must reach the harness"
    );

    // The upstream's own body must arrive intact, usage included.
    let returned = response.text().await.expect("body");
    assert!(returned.contains("\"prompt_tokens\":1234"), "got {returned}");
    assert!(returned.contains("chatcmpl-test"), "got {returned}");

    let requests = seen.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 1, "the upstream must see exactly one request");
    let received = &requests[0];

    assert_eq!(received.path, "/v1/chat/completions");
    assert_eq!(received.method, "POST");
    assert!(
        received.headers.contains_key("x-harness-trace"),
        "a harness header was dropped: {:?}",
        received.headers.keys().collect::<Vec<_>>()
    );

    // A short transcript is under the window, so nothing should have been rewritten.
    let sent: serde_json::Value = serde_json::from_str(&body).expect("parse");
    let got: serde_json::Value = serde_json::from_str(&received.body).expect("parse");
    assert_eq!(
        got["messages"], sent["messages"],
        "a request that fits the window must pass through untouched"
    );
    assert_eq!(got["temperature"], 0.0);
    assert_eq!(got["max_tokens"], 16);
}

#[tokio::test]
async fn every_other_endpoint_is_forwarded_too() {
    // The rule that makes this a proxy rather than a reimplementation: a route this build
    // has never heard of goes upstream rather than getting a 404 from the proxy. A harness
    // calling `/v1/responses` or a newer revision's route must not be broken by Sakur4
    // being in the path.
    let (upstream, seen) = spawn_upstream().await;
    let proxy = spawn_proxy(&upstream, true).await;
    let client = reqwest::Client::new();

    for (method, path) in [
        ("GET", "/v1/models"),
        ("GET", "/health"),
        ("POST", "/tokenize"),
        ("POST", "/v1/embeddings"),
        ("POST", "/v1/responses"),
        ("GET", "/props"),
    ] {
        let request = match method {
            "GET" => client.get(format!("{proxy}{path}")),
            _ => client.post(format!("{proxy}{path}")).body("{}"),
        };
        let response = request.send().await.expect("forward");
        assert_eq!(response.status(), 200, "{method} {path} was not forwarded");
    }

    let requests = upstream_requests(&seen, 6);
    let paths: Vec<String> = requests.iter().map(|r| r.path.clone()).collect();
    for expected in
        ["/v1/models", "/health", "/tokenize", "/v1/embeddings", "/v1/responses", "/props"]
    {
        assert!(paths.contains(&expected.to_string()), "{expected} never reached the upstream");
    }
}

#[tokio::test]
async fn a_query_string_survives_forwarding() {
    // Reconstruction is the classic proxy bug: rebuild the URL from the path and the query
    // is silently dropped, which a harness using `?trace=` or an Azure-style `?api-version=`
    // would experience as a mysterious upstream error.
    let (upstream, seen) = spawn_upstream().await;
    let proxy = spawn_proxy(&upstream, true).await;

    reqwest::Client::new()
        .get(format!("{proxy}/v1/models?api-version=2024-02-01&trace=1"))
        .send()
        .await
        .expect("forward");

    // The upstream records only the path, so this asserts the request arrived at all; the
    // query is verified in the unit test that covers `path_and_query`.
    assert!(!seen.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn an_unreachable_upstream_is_a_bad_gateway_not_a_hang() {
    // The one case where a non-transparent status is correct, because there is nothing to
    // be transparent about. A harness should see a clear failure rather than a timeout.
    let proxy = spawn_proxy("http://127.0.0.1:1", true).await;
    let response = reqwest::Client::new()
        .post(format!("{proxy}/v1/chat/completions"))
        .body(chat_body(serde_json::json!([{"role": "user", "content": "hi"}])))
        .send()
        .await
        .expect("proxy answers");

    assert_eq!(response.status(), 502);
    let body = response.text().await.expect("body");
    assert!(body.contains("upstream"), "the failure must name the upstream: {body}");
}

// ===========================================================================
// Accounting
// ===========================================================================

#[tokio::test]
async fn a_response_carrying_usage_still_reaches_the_harness_intact() {
    // The accounting path reads the response body, which means it sits between the upstream
    // and the harness. The failure this guards against is the obvious one: a proxy that
    // consumes a body to inspect it and then forwards nothing, or forwards it twice.
    //
    // The usage *parsing* is unit-tested in `proxy::tests`, where a table of response shapes
    // can be fed in directly. What this covers is that a real HTTP round trip carrying a
    // real `usage` object arrives complete.
    let (upstream, _seen) = spawn_upstream().await;
    let proxy = spawn_proxy(&upstream, true).await;

    let response = reqwest::Client::new()
        .post(format!("{proxy}/v1/chat/completions"))
        .body(chat_body(serde_json::json!([{"role": "user", "content": "hello"}])))
        .send()
        .await
        .expect("forward");

    assert_eq!(response.status(), 200);
    let body = response.text().await.expect("body");
    let parsed: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");
    assert_eq!(parsed["usage"]["prompt_tokens"], 1234);
    assert_eq!(parsed["usage"]["prompt_tokens_details"]["cached_tokens"], 1000);
    assert_eq!(parsed["choices"][0]["message"]["content"], "ok");
}

// ===========================================================================
// Observe-only, which is how a user measures before changing anything
// ===========================================================================

#[tokio::test]
async fn observe_only_never_rewrites_even_a_long_transcript() {
    // `--observe-only` exists so a user can put the proxy in front of their traffic, watch
    // what it *would* have done, and change nothing. That makes it the safe way to
    // evaluate Sakur4 on a real workload, so the guarantee has to be absolute.
    let (upstream, seen) = spawn_upstream().await;
    let proxy = spawn_proxy(&upstream, false).await;

    // Well past the 2048-token window the proxy's engine is configured with.
    let mut messages =
        vec![serde_json::json!({"role": "system", "content": "you are a coding agent"})];
    for i in 0..60 {
        messages.push(serde_json::json!({"role": "user", "content": format!("turn {i}: {}", "x".repeat(400))}));
        messages.push(serde_json::json!({"role": "assistant", "content": format!("answer {i}: {}", "y".repeat(400))}));
    }
    let body = chat_body(serde_json::Value::Array(messages.clone()));

    reqwest::Client::new()
        .post(format!("{proxy}/v1/chat/completions"))
        .body(body.clone())
        .send()
        .await
        .expect("forward");

    let requests = seen.requests.lock().unwrap().clone();
    let got: serde_json::Value = serde_json::from_str(&requests[0].body).expect("parse");
    assert_eq!(
        got["messages"].as_array().map(|a| a.len()),
        Some(messages.len()),
        "observe-only must forward the transcript unchanged, however long it is"
    );
}
// ===========================================================================
// The finding from pointing a real harness at this
// ===========================================================================

#[tokio::test]
async fn a_harness_system_prompt_is_never_evicted() {
    // # Found by running OMP through the proxy against a real llama.cpp
    //
    // A single enormous *user* message produced no rewrite, and the reason is worth a test
    // rather than a note: the eviction engine protects system messages, and a harness sends
    // a very large one on every turn — tool schemas, conventions, the lot. So the largest
    // thing in a real request is also the one thing eviction will not touch.
    //
    // That is deliberate. The system prompt is the harness's contract with the model, and
    // removing it produces a request the model cannot answer sensibly — worse than an
    // over-long one. But it means **a transcript has to grow past two turns before the proxy
    // can do anything**, which is surprising enough to pin down.
    let (upstream, seen) = spawn_upstream().await;
    let proxy = spawn_proxy(&upstream, true).await;

    let system = "S".repeat(8_000); // comfortably past the 2048-token window alone
    let body = chat_body(serde_json::json!([
        {"role": "system", "content": system},
        {"role": "user", "content": "hi"}
    ]));

    reqwest::Client::new()
        .post(format!("{proxy}/v1/chat/completions"))
        .body(body)
        .send()
        .await
        .expect("forward");

    let requests = seen.requests.lock().unwrap().clone();
    let got: serde_json::Value = serde_json::from_str(&requests[0].body).expect("parse");
    let messages = got["messages"].as_array().expect("messages");

    assert_eq!(messages[0]["role"], "system");
    assert_eq!(
        messages[0]["content"].as_str().map(str::len),
        Some(system.len()),
        "the harness's system prompt must reach the model verbatim, however large it is"
    );
}

#[tokio::test]
async fn every_turn_of_a_multi_turn_session_is_recorded() {
    // The proxy's value in observe-only mode is that it builds a memory of the session, so a
    // turn going unrecorded is the failure that matters. Three turns are sent, each
    // extending the last, exactly as a stateless client re-sends its whole transcript.
    let (upstream, seen) = spawn_upstream().await;
    let proxy = spawn_proxy(&upstream, true).await;
    let client = reqwest::Client::new();

    let mut history = vec![serde_json::json!({"role": "user", "content": "first turn about auth"})];
    let mut sent = 0;

    // The harness sends what it has, *then* appends the reply — so the first turn goes out
    // alone and each later turn carries everything before it.
    for reply in ["second turn about retries", "third turn about caching"] {
        sent += 1;
        let response = client
            .post(format!("{proxy}/v1/chat/completions"))
            .body(chat_body(serde_json::Value::Array(history.clone())))
            .send()
            .await
            .expect("forward");
        assert_eq!(response.status(), 200, "turn {sent}");

        history.push(serde_json::json!({"role": "assistant", "content": "acknowledged"}));
        history.push(serde_json::json!({"role": "user", "content": reply}));
    }
    sent += 1;
    let response = client
        .post(format!("{proxy}/v1/chat/completions"))
        .body(chat_body(serde_json::Value::Array(history.clone())))
        .send()
        .await
        .expect("forward");
    assert_eq!(response.status(), 200, "turn {sent}");

    // Three requests reached the upstream, each carrying the history the harness re-sent —
    // which is what makes the deduplication in `remember` necessary rather than merely tidy.
    let requests = upstream_requests(&seen, sent);
    assert_eq!(requests.len(), sent, "the upstream must see one request per turn");
    let last: serde_json::Value = serde_json::from_str(&requests[sent - 1].body).expect("parse");
    assert_eq!(
        last["messages"].as_array().map(|a| a.len()),
        Some(5),
        "the final turn carries the whole conversation"
    );
}

#[tokio::test]
async fn a_rewritten_turn_records_a_receipt() {
    // `context.receipt` reads the *latest* receipt for a session and falls back to an assembled
    // preview when there is none. The proxy recorded nothing, so on the one path where a real
    // prompt reaches a real server, the receipt a user saw described a preview instead — and two
    // of its categories (`repo_map`, `folds`) can only ever be zero there, because the preview
    // assembler never fills them. A field that is always zero is indistinguishable from a
    // measurement, which is the opposite of what this receipt is for.
    //
    // This is the check that the wiring exists. Without it, a later refactor that dropped the
    // `record` call would leave every receipt silently describing the wrong prompt.
    let (upstream, _seen) = spawn_upstream().await;
    let (proxy, db_path) = spawn_proxy_with_store(&upstream, true).await;

    let mut messages =
        vec![serde_json::json!({"role": "system", "content": "you are a coding agent"})];
    for i in 0..60 {
        messages.push(serde_json::json!({"role": "user", "content": format!("turn {i}: {}", "x".repeat(400))}));
        messages.push(serde_json::json!({"role": "assistant", "content": format!("answer {i}: {}", "y".repeat(400))}));
    }

    reqwest::Client::new()
        .post(format!("{proxy}/v1/chat/completions"))
        .body(chat_body(serde_json::Value::Array(messages)))
        .send()
        .await
        .expect("forward");

    // Reopen the store the proxy wrote to. The receipt must be there, and it must describe a
    // prompt with content in it — a receipt whose total is zero would mean the wiring stored an
    // empty assembler result and called it the turn.
    let engine = Engine::open(EngineConfig {
        db_path: db_path.to_string_lossy().to_string(),
        backend: "embedded".into(),
        default_n_ctx: 2048,
        context_window_explicit: true,
        ..Default::default()
    })
    .await
    .expect("reopen engine");

    let receipt = engine
        .receipts()
        .latest("proxy-test")
        .await
        .expect("read receipts")
        .expect("the proxy recorded no receipt for a turn it rewrote");

    assert!(
        receipt.total_tokens > 0,
        "the receipt measures zero tokens for a 121-message transcript"
    );
    assert!(
        receipt.breakdown.raw_recent_history > 0,
        "no timeline tokens: the receipt does not describe the transcript as sent"
    );
    assert_eq!(
        receipt.context_window, 2048,
        "the receipt reports a window other than the one the proxy planned against; got {}",
        receipt.context_window
    );
}
