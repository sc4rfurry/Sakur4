//! Integration tests for the **stdio** transport.
//!
//! # Why a separate file from `gateway.rs`
//!
//! `gateway.rs` drives the tool surface over HTTP, in-process. This spawns the
//! real `sakur4d` binary as a child process and speaks JSON-RPC over its
//! stdin/stdout — which is how most MCP clients, including Hermes' default
//! `mcp_servers` form, actually connect.
//!
//! The difference is not cosmetic. Over stdio there is no port, no HTTP status
//! code, and no way to distinguish a protocol error from a diagnostic: anything
//! written to stdout that is not a JSON-RPC frame corrupts the channel. So these
//! tests check the things that only matter on that transport — that the handshake
//! completes, that every tool is reachable, and that nothing pollutes stdout.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};

/// A minimal JSON-RPC client over a child process's stdio.
///
/// Hand-rolled rather than using the SDK's client so the test exercises the wire
/// format itself: a bug in framing would be invisible to a client that shares the
/// server's library.
struct StdioClient {
    child: Child,
    stdin: tokio::process::ChildStdin,
    lines: tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
    next_id: u64,
}

impl StdioClient {
    /// Spawn `sakur4d` with a private store and complete the MCP handshake.
    async fn spawn() -> Self {
        Self::spawn_at(&sakur4d_binary(), ":memory:").await
    }

    /// Spawn against a specific binary and store path.
    async fn spawn_at(exe: &Path, db: &str) -> Self {
        let mut child = Command::new(exe)
            .args([
                "--db",
                db,
                "--backend",
                "embedded",
                "serve",
                "--transport",
                "stdio",
                "--no-dream",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // Inherited so a panic in the server is visible in test output rather
            // than swallowed.
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .unwrap_or_else(|e| panic!("spawning {} failed: {e}", exe.display()));

        let stdin = child.stdin.take().expect("child stdin");
        let stdout = child.stdout.take().expect("child stdout");
        let mut client = Self { child, stdin, lines: BufReader::new(stdout).lines(), next_id: 1 };

        // Legacy handshake: still the path a plain stdio client takes.
        let init = client
            .request(
                "initialize",
                json!({
                    "protocolVersion": "2025-11-25",
                    "capabilities": {},
                    "clientInfo": {"name": "sakur4-stdio-test", "version": "1.0"}
                }),
            )
            .await
            .expect("initialize must succeed");
        assert_eq!(init["serverInfo"]["name"], "sakur4", "unexpected server identity: {init}");

        client
            .notify("notifications/initialized", json!({}))
            .await
            .expect("initialized notification");
        client
    }

    /// Send a request and read the matching response, skipping notifications.
    async fn request(&mut self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.next_id;
        self.next_id += 1;
        let frame = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        self.send(&frame).await?;

        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Err(format!("timed out waiting for a reply to {method}"));
            }
            let line = match tokio::time::timeout(remaining, self.lines.next_line()).await {
                Ok(Ok(Some(l))) => l,
                Ok(Ok(None)) => {
                    return Err(format!("server closed stdout while awaiting {method}"));
                }
                Ok(Err(e)) => return Err(format!("read error: {e}")),
                Err(_) => return Err(format!("timed out waiting for a reply to {method}")),
            };
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let parsed: Value = serde_json::from_str(trimmed).map_err(|e| {
                // A non-JSON line on stdout is the specific failure this transport
                // must never have: it means something wrote a diagnostic there.
                format!("stdout carried a non-JSON line ({e}): {trimmed:?}")
            })?;
            if parsed.get("id").and_then(|v| v.as_u64()) == Some(id) {
                if let Some(err) = parsed.get("error") {
                    return Err(format!("{method} returned an error: {err}"));
                }
                return Ok(parsed.get("result").cloned().unwrap_or(Value::Null));
            }
            // A notification or a response to another id: keep reading.
        }
    }

    async fn notify(&mut self, method: &str, params: Value) -> Result<(), String> {
        let frame = json!({"jsonrpc": "2.0", "method": method, "params": params});
        self.send(&frame).await
    }

    async fn send(&mut self, frame: &Value) -> Result<(), String> {
        let mut line = serde_json::to_string(frame).map_err(|e| e.to_string())?;
        line.push('\n');
        self.stdin.write_all(line.as_bytes()).await.map_err(|e| format!("write failed: {e}"))?;
        self.stdin.flush().await.map_err(|e| format!("flush failed: {e}"))
    }

    /// Call a tool and return its `structuredContent`.
    async fn call_tool(&mut self, name: &str, args: Value) -> Result<Value, String> {
        let result = self.request("tools/call", json!({"name": name, "arguments": args})).await?;
        if result.get("isError").and_then(|v| v.as_bool()) == Some(true) {
            return Err(format!("{name} reported an error: {result}"));
        }
        Ok(result.get("structuredContent").cloned().unwrap_or(Value::Null))
    }
}

impl Drop for StdioClient {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}

/// Path to the binary under test, next to the test executable.
fn sakur4d_binary() -> PathBuf {
    // `target/<profile>/deps/<test>.exe` -> `target/<profile>/sakur4d.exe`
    let mut dir = std::env::current_exe().expect("test exe path");
    dir.pop(); // deps
    if dir.ends_with("deps") {
        dir.pop();
    }
    let name = if cfg!(windows) { "sakur4d.exe" } else { "sakur4d" };
    let candidate = dir.join(name);
    assert!(
        candidate.exists(),
        "the sakur4d binary must be built before this test runs; expected {}",
        candidate.display()
    );
    candidate
}

#[tokio::test]
async fn stdio_handshake_and_tool_catalog() {
    let mut client = StdioClient::spawn().await;

    let tools = client.request("tools/list", json!({})).await.expect("tools/list");
    let names: Vec<String> = tools["tools"]
        .as_array()
        .expect("tools array")
        .iter()
        .map(|t| t["name"].as_str().unwrap_or_default().to_string())
        .collect();

    assert_eq!(names.len(), 17, "expected 17 tools, got {names:?}");
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
        "context.record_usage",
        "memory.staleness",
        "sakur4.status",
        "sakur4.dream",
    ] {
        assert!(names.contains(&expected.to_string()), "missing {expected}");
    }

    // stdio is the transport that has no port and no restarts: if a tool whose
    // whole purpose is statelessness works here, the server is genuinely usable
    // as a child process.
    let resources = client.request("resources/list", json!({})).await.expect("resources/list");
    assert!(!resources["resources"].as_array().unwrap().is_empty());

    let prompts = client.request("prompts/list", json!({})).await.expect("prompts/list");
    assert!(
        prompts["prompts"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["name"] == "sakur4_system_preamble")
    );
}

#[tokio::test]
async fn every_tool_is_callable_over_stdio() {
    let mut client = StdioClient::spawn().await;

    // A session that exercises the dual-track write path.
    let committed = client
        .call_tool(
            "memory.commit_episode",
            json!({
                "role": "user",
                "content": "Rule for this repo: never force-push to main, and do not delete the \
                            migrations directory under any circumstances.",
                "session_id": "stdio"
            }),
        )
        .await
        .expect("commit_episode");
    assert_eq!(
        committed["suggested_anchor"]["kind"], "safety_constraint",
        "the deterministic detector must fire: {committed}"
    );

    // Structured tool output becomes deterministic facts.
    let tool = client
        .call_tool(
            "memory.commit_episode",
            json!({
                "role": "tool",
                "tool_name": "read_file",
                "content": "{\"service\":\"sakur4\",\"port\":8765,\"enabled\":true}",
                "session_id": "stdio"
            }),
        )
        .await
        .expect("commit tool result");
    assert_eq!(tool["symbolic_facts"], 3, "got {tool}");

    // Pin, then confirm it is present and accounted for.
    let pinned = client
        .call_tool(
            "memory.pin",
            json!({"content": "never force-push to main", "kind": "safety_constraint", "session_id": "stdio"}),
        )
        .await
        .expect("pin");
    assert!(pinned["token_cost"].as_u64().unwrap() > 0);

    // Recall finds it.
    let recalled = client
        .call_tool("memory.recall", json!({"query": "force-push", "k": 5, "session_id": "stdio"}))
        .await
        .expect("recall");
    assert!(
        recalled["results"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["text"].as_str().unwrap_or_default().contains("force-push")),
        "recall missed the committed turn: {recalled}"
    );

    // Fold, then collapse.
    let folded = client
        .call_tool(
            "memory.fold",
            json!({"description": "trace the call path", "goal": "find callers", "session_id": "stdio"}),
        )
        .await
        .expect("fold");
    let fold_id = folded["fold_id"].as_str().expect("fold_id").to_string();

    let unfolded = client
        .call_tool(
            "memory.unfold",
            json!({"fold_id": fold_id, "result_summary": "validate() is called from login() only", "session_id": "stdio"}),
        )
        .await
        .expect("unfold");
    assert_eq!(unfolded["trace_retrievable"], true);

    let trace = client
        .call_tool("memory.recall_fold", json!({"fold_id": folded["fold_id"]}))
        .await
        .expect("recall_fold");
    assert_eq!(trace["status"], "closed");

    // Planning is inspect-only by default.
    let plan = client
        .call_tool("context.plan_eviction", json!({"session_id": "stdio", "slot_id": "0"}))
        .await
        .expect("plan_eviction");
    assert_eq!(plan["applied"], false, "planning must not apply: {plan}");
    assert!(plan["pressure"].is_string());

    // The receipt accounts for the anchors and the history.
    let receipt = client
        .call_tool("context.receipt", json!({"session_id": "stdio", "assemble": true}))
        .await
        .expect("receipt");
    assert!(receipt["breakdown"]["pinned_anchors"].as_u64().unwrap() > 0);
    assert!(receipt["rendered"].as_str().unwrap().contains("Context Ledger Receipt"));

    // Staleness, status, and the dream cycle are all reachable and non-fatal.
    let stale = client.call_tool("memory.staleness", json!({})).await.expect("staleness");
    assert!(stale["total"].is_number());

    let status = client.call_tool("sakur4.status", json!({})).await.expect("status");
    assert_eq!(status["protocol_version"], "2026-07-28");
    // Two committed turns plus the summary episode that `unfold` contributed.
    assert!(
        status["episodes"].as_u64().unwrap() >= 3,
        "expected the two turns and the fold summary, got {status}"
    );

    let dream = client.call_tool("sakur4.dream", json!({"force": true})).await.expect("dream");
    assert!(dream["ran"].is_boolean(), "got {dream}");

    // The code tools answer cleanly even with nothing indexed.
    let map = client
        .call_tool("code.get_repo_map", json!({"token_budget": 200}))
        .await
        .expect("repo_map");
    assert!(map["map"].is_string());

    let symbol = client
        .call_tool("code.query_symbol", json!({"qualified_name": "nothing::here"}))
        .await
        .expect("query_symbol");
    assert_eq!(symbol["found"], false);

    // Snapshot and restore round-trip on the embedded backend.
    let snap = client
        .call_tool("session.snapshot", json!({"slot_id": "0", "session_id": "stdio"}))
        .await
        .expect("snapshot");
    assert!(snap["snapshot_id"].as_str().unwrap().starts_with("snap_"));
    if let Some(path) = snap["file_path"].as_str() {
        let restored = client
            .call_tool(
                "session.restore",
                json!({"path": path, "slot_id": "0", "session_id": "stdio"}),
            )
            .await
            .expect("restore");
        assert_eq!(restored["restored"], true, "got {restored}");
    }
}

#[tokio::test]
async fn provider_cache_accounting_detects_a_prefix_break_over_stdio() {
    // The cloud-provider analogue of cache-coherent compaction.
    //
    // A local llama.cpp slot reports its cache through its own API, so Sakur4 can
    // ask it where to cut. A hosted provider reports it in the completion response
    // instead — and the failure worth catching is the same one: a compaction that
    // rewrites already-sent history invalidates the provider's cached prefix, so
    // the next turn is billed for tokens that had already been paid for.
    let mut client = StdioClient::spawn().await;

    // Turn 1: a cold prefix. Nothing is cached yet.
    let cold = client
        .call_tool(
            "context.record_usage",
            json!({
                "session_id": "cloud",
                "prompt_tokens": 5000,
                "completion_tokens": 200,
                "cache_read_tokens": 0,
                "provider": "openai-api",
                "model": "gpt-5.2"
            }),
        )
        .await
        .expect("record cold turn");
    assert_eq!(cold["turn"], 1);
    assert_eq!(cold["verdict"], "cache-miss");
    assert_eq!(cold["regression"], false);
    assert_eq!(cold["uncached_tokens"], 5000);

    // Turn 2: appended, so the prefix is reused and only the suffix is fresh.
    let warm = client
        .call_tool(
            "context.record_usage",
            json!({
                "session_id": "cloud",
                "prompt_tokens": 6000,
                "cache_read_tokens": 5000
            }),
        )
        .await
        .expect("record warm turn");
    assert_eq!(warm["verdict"], "partial-reuse");
    assert_eq!(warm["regression"], false);
    assert_eq!(warm["cached_tokens"], 5000);
    assert_eq!(warm["uncached_tokens"], 1000);

    // Turn 3: a compaction rewrote history. The prompt stays large while the
    // cached prefix collapses — the signature, and a real bill.
    let broken = client
        .call_tool(
            "context.record_usage",
            json!({
                "session_id": "cloud",
                "prompt_tokens": 6200,
                "cache_read_tokens": 300
            }),
        )
        .await
        .expect("record post-compaction turn");
    assert_eq!(
        broken["verdict"], "PREFIX-BROKEN",
        "a rewrite that invalidates the cached prefix must be reported: {broken}"
    );
    assert_eq!(broken["regression"], true);
    assert!(
        broken["detail"].as_str().unwrap().contains("already been paid for"),
        "the detail must name the cost: {broken}"
    );
    assert!(
        broken["session_stats"].as_str().unwrap().contains("PREFIX BREAK"),
        "session stats must surface it: {broken}"
    );

    // The receipt carries the accounting, so a slow or expensive turn is
    // explainable from the same place as a locally re-prefilled one.
    let receipt = client
        .call_tool("context.receipt", json!({"session_id": "cloud", "assemble": true}))
        .await
        .expect("receipt");
    let provider = receipt["provider_cache"]
        .as_str()
        .expect("the receipt must report provider-cache accounting when usage was recorded");
    assert!(provider.contains("PREFIX BREAK"), "got {provider}");
    assert!(provider.contains("prompt tokens served from cache"), "got {provider}");
}

#[tokio::test]
async fn a_provider_that_reports_no_cache_fields_is_not_blamed() {
    // Not reported is not the same as zero, and the difference matters: a
    // provider that never sends cache fields cannot be diagnosed by a number it
    // never sent, and reporting a "miss" would be a fabricated finding.
    let mut client = StdioClient::spawn().await;
    let out = client
        .call_tool(
            "context.record_usage",
            json!({"session_id": "silent", "prompt_tokens": 4000, "completion_tokens": 100}),
        )
        .await
        .expect("record usage without cache fields");
    assert_eq!(out["verdict"], "not-reported");
    assert_eq!(out["regression"], false);
    assert_eq!(out["cached_tokens"], Value::Null);
    assert_eq!(out["uncached_tokens"], Value::Null);
    assert!(out["session_stats"].as_str().unwrap().contains("not reported"), "got {out}");
}

#[tokio::test]
async fn malformed_arguments_are_rejected_without_killing_the_session() {
    // A harness under development will send bad arguments. The failure mode must be
    // a clean rejection of that one call, not a dead server — and over stdio there
    // is no reconnect for the client to fall back on.
    //
    // # The observed contract, recorded because it is easy to guess wrong
    //
    // There are two distinct rejection shapes, and an adapter has to handle both:
    //
    // * A call that fails the tool's *JSON schema* comes back as a successful
    //   JSON-RPC response whose result carries `isError: true`, with the message
    //   naming the missing field.
    // * A call that is schema-valid but semantically rejected by Sakur4 comes back
    //   as a JSON-RPC error (`-32603`) naming the offending value.
    //
    // Protocol-level mistakes (an unknown method or tool) are JSON-RPC errors too.
    // Either way the session survives, which is the property that matters.
    //
    // This test also guards the transport itself: while writing it, a WARN-level
    // log line was written to **stdout**, which is the JSON-RPC channel over stdio.
    // The client read it as a frame and failed. `init_tracing` now pins the writer
    // to stderr, and `StdioClient::request` fails loudly on any non-JSON line
    // rather than skipping it — which is how the bug was found.
    let mut client = StdioClient::spawn().await;

    async fn raw_call(client: &mut StdioClient, name: &str, args: Value) -> Result<Value, String> {
        client.request("tools/call", json!({"name": name, "arguments": args})).await
    }

    // Shape one: a schema failure is an `isError` result, naming the field.
    let r = raw_call(&mut client, "memory.commit_episode", json!({"role": "user"}))
        .await
        .expect("a schema failure is still a successful JSON-RPC response");
    assert_eq!(r["isError"], true, "a missing required field must set isError: {r}");
    let text = serde_json::to_string(&r).unwrap();
    assert!(text.contains("content"), "the rejection must name the missing field: {r}");

    // Shape two: schema-valid but semantically rejected is a JSON-RPC error that
    // names the offending value, so an adapter author can see what was wrong.
    let err = raw_call(
        &mut client,
        "memory.commit_episode",
        json!({"role": "not-a-role", "content": "x"}),
    )
    .await
    .expect_err("an invalid role must be rejected");
    assert!(err.contains("unknown role"), "the rejection must say what was wrong: {err}");

    // An unknown tool is a JSON-RPC error, because the tool does not exist.
    let err = raw_call(&mut client, "no.such_tool", json!({}))
        .await
        .expect_err("an unknown tool must be rejected");
    assert!(!err.is_empty());

    // An unknown method is a JSON-RPC error.
    let err = client
        .request("nonsense/method", json!({}))
        .await
        .expect_err("an unknown method must be rejected");
    assert!(!err.is_empty());

    // The essential property: the session still works afterwards.
    let out = client
        .call_tool(
            "memory.commit_episode",
            json!({"role": "user", "content": "still alive", "session_id": "s"}),
        )
        .await
        .expect("the server must survive malformed input");
    assert!(out["episode_id"].as_str().unwrap().starts_with("ep_"));
}

/// A large tool result must not break the framing.
#[tokio::test]
async fn a_large_payload_round_trips_intact() {
    let mut client = StdioClient::spawn().await;
    // ~1.5 MB of structured output: big enough that a framing bug, a line-length
    // assumption, or a stdio buffer limit would show up, and realistic for reading
    // a generated file.
    let big: String = format!(
        "{{\"items\":[{}]}}",
        (0..40_000)
            .map(|i| format!("{{\"id\":{i},\"name\":\"item_{i}\"}}"))
            .collect::<Vec<_>>()
            .join(",")
    );
    assert!(big.len() > 1_000_000, "fixture is {} bytes", big.len());

    let committed = client
        .call_tool(
            "memory.commit_episode",
            json!({"role": "tool", "tool_name": "read_file", "content": big, "session_id": "big"}),
        )
        .await
        .expect("commit a large tool result");
    assert!(
        committed["symbolic_facts"].as_u64().unwrap() > 0,
        "a large structured payload must still yield facts: {committed}"
    );

    let recalled = client
        .call_tool("memory.recall", json!({"query": "item_1999", "k": 3, "session_id": "big"}))
        .await
        .expect("recall the large payload");
    assert!(
        !recalled["results"].as_array().unwrap().is_empty(),
        "the payload must be retrievable at full size: {recalled}"
    );
}

#[tokio::test]
async fn a_restart_preserves_the_store_and_resumes() {
    // NFR-6: a crash and restart mid-session loses at most the last uncommitted
    // turn, never the Memory Fabric. stdio is the right transport to prove it on,
    // because the client owns the process lifetime.
    let store_dir = tempfile::tempdir().expect("temp dir");
    let db = store_dir.path().join("sakur4.db");
    let db_arg = db.display().to_string();
    let exe = sakur4d_binary();

    {
        let mut client = StdioClient::spawn_at(&exe, &db_arg).await;
        for text in ["first turn survives", "second turn survives"] {
            let out = client
                .call_tool(
                    "memory.commit_episode",
                    json!({"role": "user", "content": text, "session_id": "restart"}),
                )
                .await
                .expect("commit before restart");
            assert!(out["episode_id"].as_str().unwrap().starts_with("ep_"));
        }
        // Dropping the client closes stdin, which ends the session.
    }

    assert!(db.exists(), "the store file must persist across a restart");

    let mut client = StdioClient::spawn_at(&exe, &db_arg).await;
    let status = client.call_tool("sakur4.status", json!({})).await.expect("status after restart");
    assert_eq!(
        status["episodes"], 2,
        "episodes committed before the restart must survive it: {status}"
    );

    let recalled = client
        .call_tool("memory.recall", json!({"query": "survives", "k": 5, "session_id": "restart"}))
        .await
        .expect("recall after restart");
    assert_eq!(
        recalled["results"].as_array().unwrap().len(),
        2,
        "both turns must be recallable after a restart: {recalled}"
    );

    client
        .call_tool(
            "memory.commit_episode",
            json!({"role": "user", "content": "third turn, after the restart", "session_id": "restart"}),
        )
        .await
        .expect("commit after restart");
    let status = client.call_tool("sakur4.status", json!({})).await.expect("status");
    assert_eq!(status["episodes"], 3);
}
