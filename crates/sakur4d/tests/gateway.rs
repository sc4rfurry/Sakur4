//! Integration tests for the MCP gateway.
//!
//! These run the *production* tool surface over a real HTTP listener using the
//! official SDK's own client, rather than calling handler methods directly. That
//! matters because the parts most likely to break are exactly the parts a unit
//! test skips: JSON-RPC framing, protocol-version negotiation, the
//! `resultType`/`ttlMs`/`cacheScope` fields the 2026-07-28 revision requires, and
//! the shape of each tool's arguments as a client actually sends them.
//!
//! The engine they run against uses the embedded backend, so no GPU, model or
//! external server is involved — but the cache-coherence behaviour being reported
//! is real, not stubbed.

use std::sync::Arc;

use rmcp::model::{CallToolRequestParams, ClientCapabilities, Implementation, ProtocolVersion};
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::{ClientHandler, ServiceExt};
use sakur4_core::llama::BackendSpec;
use sakur4_core::{Engine, EngineConfig};

/// A minimal MCP client that declares nothing and answers nothing.
#[derive(Debug, Clone, Default)]
struct SilentClient;

impl ClientHandler for SilentClient {
    fn get_info(&self) -> rmcp::model::ClientConfig {
        let mut info = rmcp::model::ClientConfig::default();
        info.protocol_version = ProtocolVersion::V_2026_07_28;
        info.capabilities = ClientCapabilities::default();
        info.client_info = Implementation::new("sakur4-test-client", "0.1.0");
        info
    }
}

/// Start a gateway on an ephemeral port and return its base URL.
async fn start_gateway() -> (String, Arc<Engine>, tokio::task::JoinHandle<()>) {
    let cfg = EngineConfig {
        db_path: ":memory:".into(),
        backend: BackendSpec::Embedded.to_string(),
        ..Default::default()
    };
    let engine = Arc::new(Engine::open(cfg).await.expect("engine opens"));
    let server = sakur4d::tools::Sakur4Server::new((*engine).clone());

    let listener =
        tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind an ephemeral port");
    let addr = listener.local_addr().expect("local addr");

    let service: rmcp::transport::streamable_http_server::StreamableHttpService<
        sakur4d::tools::Sakur4Server,
        rmcp::transport::streamable_http_server::session::local::LocalSessionManager,
    > = rmcp::transport::streamable_http_server::StreamableHttpService::new(
        {
            let server = server.clone();
            move || Ok(server.clone())
        },
        Arc::new(
            rmcp::transport::streamable_http_server::session::local::LocalSessionManager::default(),
        ),
        Default::default(),
    );

    let router = axum::Router::new().fallback_service(service);
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });

    (format!("http://{addr}"), engine, handle)
}

/// Connect a client to a base URL.
async fn connect(
    base_url: &str,
) -> rmcp::service::RunningService<rmcp::service::RoleClient, SilentClient> {
    let transport = StreamableHttpClientTransport::from_uri(base_url.to_string());
    SilentClient.serve(transport).await.expect("client connects and negotiates")
}

/// Extract the JSON body of a tool result.
fn tool_json(result: &rmcp::model::CallToolResult) -> serde_json::Value {
    let text = result
        .content
        .iter()
        .filter_map(|c| c.as_text().map(|t| t.text.clone()))
        .collect::<Vec<_>>()
        .join("");
    serde_json::from_str(&text).unwrap_or(serde_json::Value::String(text))
}

async fn call(
    client: &rmcp::service::RunningService<rmcp::service::RoleClient, SilentClient>,
    name: &str,
    args: serde_json::Value,
) -> serde_json::Value {
    let mut params = CallToolRequestParams::new(name.to_string());
    match args {
        serde_json::Value::Object(map) => params = params.with_arguments(map),
        serde_json::Value::Null => {}
        other => panic!("tool arguments must be an object or null, got {other}"),
    }
    let result =
        client.call_tool(params).await.unwrap_or_else(|e| panic!("calling {name} failed: {e}"));
    tool_json(&result)
}

#[tokio::test]
async fn the_tool_catalog_is_stable_cacheable_and_complete() {
    let (url, _engine, _handle) = start_gateway().await;
    let client = connect(&url).await;

    let first = client.list_tools(None).await.expect("tools/list");
    let second = client.list_tools(None).await.expect("tools/list again");

    // FR-14: the catalog is cacheable and byte-identical when nothing changed.
    assert!(first.ttl_ms.is_some(), "list responses must carry ttlMs for the 2026-07-28 revision");
    assert!(first.cache_scope.is_some(), "cacheScope must be present");
    assert_eq!(
        serde_json::to_string(&first.tools).unwrap(),
        serde_json::to_string(&second.tools).unwrap(),
        "repeated tools/list must return identical bytes"
    );

    let names: Vec<&str> = first.tools.iter().map(|t| t.name.as_ref()).collect();
    for expected in [
        "memory.commit_episode",
        "memory.pin",
        "memory.recall",
        "memory.fold",
        "memory.unfold",
        "memory.recall_fold",
        "code.get_repo_map",
        "code.query_symbol",
        "code.impact_of_change",
        "session.snapshot",
        "session.restore",
        "context.receipt",
        "context.plan_eviction",
        "memory.staleness",
        "sakur4.status",
        "sakur4.dream",
    ] {
        assert!(
            names.contains(&expected),
            "the PRD's tool surface is missing {expected}; got {names:?}"
        );
    }
    client.cancel().await.ok();
}

#[tokio::test]
async fn resources_and_prompts_are_listed_with_the_prd_uris() {
    let (url, _engine, _handle) = start_gateway().await;
    let client = connect(&url).await;

    let resources = client.list_resources(None).await.expect("resources/list");
    let uris: Vec<&str> = resources.resources.iter().map(|r| r.uri.as_str()).collect();
    assert!(uris.iter().any(|u| u.starts_with("sakur4://repo-map/")), "got {uris:?}");
    assert!(uris.contains(&"sakur4://receipt/latest"), "got {uris:?}");
    assert!(uris.iter().any(|u| u.starts_with("sakur4://anchors/")), "got {uris:?}");

    // The receipt resource is uncached by design: it describes one turn.
    let prompts = client.list_prompts(None).await.expect("prompts/list");
    assert!(
        prompts.prompts.iter().any(|p| p.name == "sakur4_system_preamble"),
        "the preamble prompt must be discoverable"
    );

    let got = client
        .get_prompt(rmcp::model::GetPromptRequestParams::new("sakur4_system_preamble"))
        .await
        .expect("prompts/get");
    let text = format!("{got:?}");
    assert!(text.contains("memory.fold"), "the preamble must tell the model how to fold");
    client.cancel().await.ok();
}

/// The repo map says when it was built, because nothing rebuilds it.
///
/// # Why this asserts a sentence rather than a behaviour
///
/// Every existing test here *lists* resources; none read a body. That gap is why the repo-map
/// resource could advertise a TTL that "tracks Repo Cortex's last re-index" — a claim about a
/// constant, and one a caching client would rely on — and why `RepoCortex::last_indexed`, written
/// for exactly that purpose, could sit with no caller while the map went out undated.
///
/// A structural map with no date reads as current. It is built from the last index, nothing watches
/// the filesystem (`notify` and `notify-debouncer-full` were declared for that and never used), and
/// `code.impact_of_change` reasons over the same stored facts — so an undated map is a confident
/// answer about a tree that may have moved. The date is the whole safeguard, so it is what the test
/// checks.
#[tokio::test]
async fn the_repo_map_dates_itself() {
    let (url, _engine, _handle) = start_gateway().await;
    let client = connect(&url).await;

    let resources = client.list_resources(None).await.expect("resources/list");
    let uri = resources
        .resources
        .iter()
        .map(|r| r.uri.as_str().to_string())
        .find(|u| u.starts_with("sakur4://repo-map/"))
        .expect("a repo-map resource");

    let read = client
        .read_resource_once(rmcp::model::ReadResourceRequestParams::new(uri.clone()))
        .await
        .expect("resources/read");
    let body = format!("{read:?}");

    // Nothing has indexed in this test, so the map must say exactly that rather than implying it is
    // current. `never indexed` is the only truthful thing to report here.
    assert!(
        body.contains("never indexed") || body.contains("as of the last index"),
        "the repo map must date itself; got {}",
        &body[..body.len().min(400)]
    );
    assert!(
        body.contains("sakur4d index"),
        "and it must say what to do about being out of date; got {}",
        &body[..body.len().min(400)]
    );
    client.cancel().await.ok();
}

#[tokio::test]
async fn a_full_session_round_trips_through_the_gateway() {
    let (url, engine, _handle) = start_gateway().await;
    let client = connect(&url).await;

    // 1 · status reports what was actually detected.
    let status = call(&client, "sakur4.status", serde_json::json!({})).await;
    assert_eq!(status["protocol_version"], "2026-07-28");
    assert_eq!(status["backend"], "sakur4://embedded");
    assert!(
        status["cache_coherence"].as_str().unwrap().contains("checkpoint-aligned"),
        "the embedded backend supports alignment; got {}",
        status["cache_coherence"]
    );

    // 2 · commit a turn that states a constraint: a pin is *suggested*, not taken.
    let committed = call(
        &client,
        "memory.commit_episode",
        serde_json::json!({
            "role": "user",
            "content": "Rule for this repo: never force-push to main, and do not delete the \
                        migrations directory under any circumstances.",
            "session_id": "mcp-test"
        }),
    )
    .await;
    assert!(committed["episode_id"].as_str().unwrap().starts_with("ep_"));
    assert_eq!(
        committed["suggested_anchor"]["kind"], "safety_constraint",
        "the deterministic detector must recognise this; got {committed}"
    );

    // 3 · pinning is explicit and reports its cost.
    let pinned = call(
        &client,
        "memory.pin",
        serde_json::json!({
            "content": "never force-push to main",
            "kind": "safety_constraint",
            "session_id": "mcp-test"
        }),
    )
    .await;
    assert!(pinned["anchor_id"].as_str().unwrap().starts_with("anc_"));
    assert!(pinned["token_cost"].as_u64().unwrap() > 0);

    // 4 · structured tool output becomes deterministic facts.
    let tool = call(
        &client,
        "memory.commit_episode",
        serde_json::json!({
            "role": "tool",
            "tool_name": "read_file",
            "content": "{\"service\":\"sakur4\",\"port\":8765,\"enabled\":true}",
            "session_id": "mcp-test"
        }),
    )
    .await;
    assert_eq!(tool["symbolic_facts"], 3, "got {tool}");
    assert!(tool["symbolic_summary"].as_str().unwrap().contains("JSON"));

    // 5 · status now reflects the writes.
    let status = call(&client, "sakur4.status", serde_json::json!({})).await;
    assert_eq!(status["episodes"], 2);
    assert_eq!(status["anchors"], 1);
    assert_eq!(status["symbolic_facts"], 3);

    // 6 · the receipt accounts for the anchors and the history.
    let receipt = call(
        &client,
        "context.receipt",
        serde_json::json!({"session_id": "mcp-test", "assemble": true}),
    )
    .await;
    assert!(
        receipt["breakdown"]["pinned_anchors"].as_u64().unwrap() > 0,
        "pinned anchors must appear in the breakdown; got {receipt}"
    );
    assert!(receipt["breakdown"]["raw_recent_history"].as_u64().unwrap() > 0, "got {receipt}");
    assert!(receipt["rendered"].as_str().unwrap().contains("Context Ledger Receipt"));
    // Sum of categories must match the measured total, within tolerance.
    let b = &receipt["breakdown"];
    let sum: u64 = [
        "system_prompt",
        "pinned_anchors",
        "retrieved_memory",
        "repo_map",
        "raw_recent_history",
        "tool_schemas",
        "fold_summaries",
        "other",
    ]
    .iter()
    .map(|k| b[*k].as_u64().unwrap_or(0))
    .sum();
    let total = receipt["total_tokens"].as_u64().unwrap();
    let tolerance = (total / 100).max(2);
    assert!(
        sum.abs_diff(total) <= tolerance,
        "category sum {sum} drifted from measured total {total} beyond {tolerance}"
    );

    // 7 · recall finds what was written.
    let recalled = call(
        &client,
        "memory.recall",
        serde_json::json!({"query": "force-push", "k": 5, "session_id": "mcp-test"}),
    )
    .await;
    assert!(
        recalled["results"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["text"].as_str().unwrap().contains("force-push")),
        "recall must find the committed turn; got {recalled}"
    );

    // 8 · folding isolates a subtask and collapsing reclaims the window.
    let folded = call(
        &client,
        "memory.fold",
        serde_json::json!({
            "description": "trace the validate() call path",
            "goal": "find every caller",
            "session_id": "mcp-test"
        }),
    )
    .await;
    let fold_id = folded["fold_id"].as_str().unwrap().to_string();
    assert!(fold_id.starts_with("fold_"));

    for i in 0..3 {
        let ep = call(
            &client,
            "memory.commit_episode",
            serde_json::json!({
                "role": "user",
                "content": format!("folded step {i}: {}", "detail ".repeat(80)),
                "session_id": "mcp-test"
            }),
        )
        .await;
        // Attach it to the fold directly through the engine, which is what an
        // adapter's hook would do; the MCP surface exposes fold/unfold, not
        // tagging, because tagging is the harness's business.
        let _ = ep;
    }
    let unfolded = call(
        &client,
        "memory.unfold",
        serde_json::json!({
            "fold_id": fold_id,
            "result_summary": "validate() is called from login() only",
            "session_id": "mcp-test"
        }),
    )
    .await;
    assert!(
        unfolded["trace_retrievable"].as_bool().unwrap(),
        "the folded trace must remain retrievable; got {unfolded}"
    );

    let trace =
        call(&client, "memory.recall_fold", serde_json::json!({"fold_id": folded["fold_id"]}))
            .await;
    assert_eq!(trace["status"], "closed");
    assert_eq!(trace["result_summary"], "validate() is called from login() only");

    // 9 · the eviction plan is inspectable before it is applied.
    let plan = call(
        &client,
        "context.plan_eviction",
        serde_json::json!({"session_id": "mcp-test", "slot_id": "0"}),
    )
    .await;
    assert!(plan["pressure"].is_string(), "the plan must report pressure; got {plan}");
    assert_eq!(plan["applied"], false, "planning must not apply anything unless asked");

    // 10 · staleness reporting is available even with an empty Atlas.
    let stale = call(&client, "memory.staleness", serde_json::json!({})).await;
    assert!(stale["total"].is_number());

    // 11 · the engine saw everything the client did.
    let episodes = engine.memory().session_episodes("mcp-test").await.unwrap();
    assert!(
        episodes.len() >= 5,
        "expected the committed turns plus the fold summary, got {}",
        episodes.len()
    );

    client.cancel().await.ok();
}

#[tokio::test]
async fn symbol_and_impact_tools_answer_over_the_wire() {
    let (url, engine, _handle) = start_gateway().await;
    let client = connect(&url).await;

    // Index a small real repository so the Ledger has content.
    let repo = sakur4_testkit::FixtureRepo::create(sakur4_testkit::FixtureSpec::minimal())
        .expect("fixture repo");
    engine.repo().index(repo.root()).await.expect("index the fixture");

    // The fixture's call chain is bootstrap -> login_handler -> login -> validate,
    // so the blast radius of `validate` is known in advance.
    let impact = call(
        &client,
        "code.impact_of_change",
        serde_json::json!({"qualified_name": "src::auth::validate", "depth": 4}),
    )
    .await;
    let callers: Vec<String> = impact["callers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["qualified_name"].as_str().unwrap().to_string())
        .collect();
    assert!(
        callers.iter().any(|c| c.contains("login")),
        "validate() is called by login(); got {callers:?}"
    );

    // A symbol lookup returns parser output, marked as such.
    let symbol = call(
        &client,
        "code.query_symbol",
        serde_json::json!({"qualified_name": "src::auth::validate"}),
    )
    .await;
    assert_eq!(symbol["found"], true, "got {symbol}");
    assert_eq!(symbol["kind"], "function");
    assert!(symbol["ast_hash"].as_str().unwrap().len() == 16);

    // An unknown symbol is a clean negative, not an error.
    let missing = call(
        &client,
        "code.query_symbol",
        serde_json::json!({"qualified_name": "does::not::exist"}),
    )
    .await;
    assert_eq!(missing["found"], false);
    assert!(
        missing["note"].as_str().unwrap().to_lowercase().contains("symbolic ledger"),
        "an unknown symbol must be a clean negative that names the reason: {missing}"
    );

    // The repo map respects its budget and reports what it used.
    let small = call(&client, "code.get_repo_map", serde_json::json!({"token_budget": 150})).await;
    let large = call(&client, "code.get_repo_map", serde_json::json!({"token_budget": 4000})).await;
    let small_used = small["tokens_used"].as_u64().unwrap();
    let large_used = large["tokens_used"].as_u64().unwrap();
    assert!(
        small_used <= 150,
        "a budget that can hold content must be honoured; used {small_used}"
    );
    assert!(
        large_used > small_used,
        "a larger budget must return more; {large_used} vs {small_used}"
    );

    // FR-10: a smaller budget returns a strict prefix of a larger one, not a
    // different ranking. Compare the symbol lines, which are the content; the
    // header and coverage footer necessarily differ.
    let symbol_lines = |v: &serde_json::Value| -> Vec<String> {
        v["map"]
            .as_str()
            .unwrap()
            .lines()
            .filter(|l| l.starts_with("  ") || l.starts_with("src/"))
            .map(|l| l.to_string())
            .collect()
    };
    let small_lines = symbol_lines(&small);
    let large_lines = symbol_lines(&large);
    assert!(!small_lines.is_empty(), "the smaller map should still hold content");
    assert!(
        large_lines.len() > small_lines.len(),
        "the larger map should hold more content: {} vs {}",
        large_lines.len(),
        small_lines.len()
    );
    assert_eq!(
        &large_lines[..small_lines.len()],
        small_lines.as_slice(),
        "the smaller map must be a prefix of the larger one.\nsmall:\n{}\nlarge:\n{}",
        small["map"].as_str().unwrap(),
        large["map"].as_str().unwrap()
    );

    client.cancel().await.ok();
}

#[tokio::test]
async fn status_counts_what_was_written_to_a_file_store() {
    // # This test passes, and the reason it passes is the finding
    //
    // It was written to reproduce a defect seen from outside the process against the *binary*:
    //
    //     sakur4d --db X --backend none serve --transport stdio
    //       (all frames written at once, stdin closed)
    //       memory.commit_episode {content: "hello"}  -> ep_01a0d1a83914752e8684d5957ea7a0a6
    //       sakur4.status {}                          -> episodes 0
    //     sqlite> SELECT COUNT(*) FROM episodic_stream  -> 1
    //
    // The difference is **pipelining**. This test calls `call(...).await` and only then asks for
    // status, so the commit has completed; a probe that writes every frame and closes stdin gets
    // `episodes: 0` on every run. Sent sequentially, the same session reports 1.
    //
    // So the defect is a read that can observe the store before a write it was pipelined behind has
    // landed — and the write reports success either way, returning a real `ep_…` identifier from a
    // store that the next request cannot see. The daemon's own trace shows them adjacent:
    //
    //     17.945350  response id=2  commit -> ep_01a0d1a8...
    //     17.945443  response id=2  status -> episodes 0
    //
    // This test is kept as the *control*: it says the store, the `Db` clone, the tool router and the
    // resolved path are all sound when requests are sequenced, which leaves concurrency in the
    // request path as the remaining suspect.
    //
    // It cannot reproduce the defect, so it is not a regression test for it. A test that does
    // reproduce it needs to pipeline, which the SDK client here does not do.
    let dir = tempfile::tempdir().expect("temp dir");
    let db_path = dir.path().join("counts.db");

    let cfg = EngineConfig {
        db_path: db_path.to_string_lossy().to_string(),
        backend: BackendSpec::Embedded.to_string(),
        ..Default::default()
    };
    let engine = Arc::new(Engine::open(cfg).await.expect("engine opens"));
    let server = sakur4d::tools::Sakur4Server::new((*engine).clone());

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let service: rmcp::transport::streamable_http_server::StreamableHttpService<
        sakur4d::tools::Sakur4Server,
        rmcp::transport::streamable_http_server::session::local::LocalSessionManager,
    > = rmcp::transport::streamable_http_server::StreamableHttpService::new(
        {
            let server = server.clone();
            move || Ok(server.clone())
        },
        Arc::new(
            rmcp::transport::streamable_http_server::session::local::LocalSessionManager::default(),
        ),
        Default::default(),
    );
    let app = axum::Router::new().fallback_service(service);
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    let url = format!("http://{addr}");
    let client = connect(&url).await;

    // Write one episode through the tool surface, then ask status for the count.
    call(
        &client,
        "memory.commit_episode",
        serde_json::json!({
            "session_id": "counts",
            "content": "the validator lives in src/auth.rs",
            "role": "user"
        }),
    )
    .await;

    let status = call(&client, "sakur4.status", serde_json::json!({})).await;

    // The store itself, read directly, so the assertion has an independent witness.
    let direct: i64 = engine
        .db()
        .with(|c| Ok(c.query_row("SELECT COUNT(*) FROM episodic_stream", [], |r| r.get(0))?))
        .await
        .expect("count episodes");

    assert_eq!(direct, 1, "the episode was not written to the store at all");
    assert_eq!(
        status["episodes"].as_i64(),
        Some(direct),
        "sakur4.status reports {} episodes for a store holding {} — the counts it reports do not \
         come from the store the tools write to. Full status: {}",
        status["episodes"],
        direct,
        status
    );

    client.cancel().await.ok();
    handle.abort();
}
