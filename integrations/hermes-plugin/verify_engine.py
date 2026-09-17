"""Exercises the Hermes Sakur4 context engine against a live daemon.

Run from the Hermes install directory, or with `HERMES_AGENT_DIR` set:

    SAKUR4_URL=http://127.0.0.1:8770 python verify_engine.py

# Why this is a script and not a unit test

The engine's whole job is to talk to a running `sakur4d` and to satisfy an interface
Hermes owns, neither of which exists in a pytest process. Mocking the daemon would test
the mock. So this imports the real engine, points it at a real daemon, and checks the
contracts that matter — including the one that is easy to get wrong and invisible until
a session is long: that compaction actually removes messages, and that pinned constraints
are in the request whether or not the user's message resembles them.

Exits non-zero on the first failed contract, so it can gate a release.
"""

from __future__ import annotations

import json
import os
import sys
from typing import Any, Dict, List

# The engine imports `agent.context_engine`, which only exists inside a Hermes install.
HERMES_DIR = os.environ.get("HERMES_AGENT_DIR") or os.path.join(
    os.environ.get("LOCALAPPDATA", os.path.expanduser("~")), "hermes", "hermes-agent"
)
PLUGIN_DIR = os.environ.get("SAKUR4_HERMES_PLUGIN") or os.path.join(
    os.environ.get("LOCALAPPDATA", os.path.expanduser("~")), "hermes", "plugins", "sakur4"
)
sys.path.insert(0, HERMES_DIR)
sys.path.insert(0, os.path.dirname(PLUGIN_DIR))

FAILURES: List[str] = []


def check(name: str, condition: bool, detail: str = "") -> None:
    mark = "PASS" if condition else "FAIL"
    print(f"  [{mark}] {name}" + (f"  — {detail}" if detail else ""))
    if not condition:
        FAILURES.append(name)


def load_engine():
    """Import the installed plugin the way Hermes' loader does."""
    import importlib.util

    spec = importlib.util.spec_from_file_location(
        "sakur4_engine", os.path.join(PLUGIN_DIR, "__init__.py")
    )
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def main() -> int:
    print(f"hermes    {HERMES_DIR}")
    print(f"plugin    {PLUGIN_DIR}")

    try:
        module = load_engine()
    except Exception as exc:  # noqa: BLE001 - the point is to report why
        print(f"  [FAIL] import: {exc}")
        return 1

    base_url = os.environ.get("SAKUR4_URL", "http://127.0.0.1:8770")
    engine = module.Sakur4ContextEngine(base_url=base_url, session_id="hermes-verify")
    print(f"daemon    {base_url}\n")

    status = engine.get_status()
    check("engine reports its name", status.get("engine") == "sakur4", str(status.get("engine")))

    # The daemon must actually answer, or every later check is meaningless.
    reachable = engine.client.call("sakur4.status", {})
    check("daemon answers sakur4.status", isinstance(reachable, dict),
          "is sakur4d running on this URL?")
    if not isinstance(reachable, dict):
        return 1

    # -----------------------------------------------------------------------
    print("\nthreshold and budget")
    # -----------------------------------------------------------------------
    engine.update_model({"contextWindow": 32768})
    check("context length recorded", engine.context_length == 32768, str(engine.context_length))
    check("threshold is the configured fraction", engine.threshold_tokens == 24576,
          str(engine.threshold_tokens))
    check("does not fire below the threshold", not engine.should_compress(1000))
    check("fires at the threshold", engine.should_compress(24576))

    # -----------------------------------------------------------------------
    print("\nanchors reach the request")
    # -----------------------------------------------------------------------
    # The bug this exists to catch: the OMP extension injected only retrieval, so a
    # short pinned rule that matched no query never reached the model at all.
    engine.client.call("memory.pin", {
        "kind": "task_contract",
        "content": "the release codename is LANTERN and the window is Thursday 02:00 UTC",
        "session_id": "hermes-verify",
    })
    fresh = module.Sakur4ContextEngine(base_url=base_url, session_id="hermes-verify")
    fresh.on_session_start("hermes-verify")

    request = [
        {"role": "system", "content": "you are a coding agent"},
        {"role": "user", "content": "carry on with whatever you were doing"},
    ]
    selected = fresh.select_context(request, conversation_messages=request, budget_tokens=32768)
    check("select_context returns a list when there is something to inject",
          isinstance(selected, list), type(selected).__name__)
    if isinstance(selected, list):
        joined = " ".join(str(m.get("content", "")) for m in selected)
        check("the pinned rule is in the request", "LANTERN" in joined,
              "a pinned constraint must reach the model regardless of the user's message")
        check("the system prompt stays first, so the cache prefix is not disturbed",
              selected[0].get("role") == "system", str(selected[0].get("role")))
        check("the original messages survive", len(selected) >= len(request),
              f"{len(selected)} vs {len(request)}")

    # -----------------------------------------------------------------------
    print("\ncompaction")
    # -----------------------------------------------------------------------
    # A transcript long enough that a plan has something to evict, with tool results
    # that the engine treats as the first candidates.
    history: List[Dict[str, Any]] = [{"role": "system", "content": "you are a coding agent"}]
    for i in range(30):
        history.append({"role": "user", "content": f"turn {i}: look at module {i}"})
        history.append({
            "role": "tool",
            "name": "read_file",
            "content": f"file {i} contents: " + ("line of code\n" * 120),
        })
        history.append({"role": "assistant", "content": f"reviewed module {i}"})
    tail = {"role": "user", "content": "now summarise what you found"}

    before = len(history)
    compressed = engine.compress(history + [tail], current_tokens=26000)

    check("compaction returns a list", isinstance(compressed, list), type(compressed).__name__)
    check("compaction does not grow the transcript", len(compressed) <= before + 1,
          f"{before} -> {len(compressed)}")
    check("something was evicted", len(compressed) < before + 1,
          "a plan that reclaims nothing means the engine cannot compact")
    check("compression_count advanced", engine.compression_count >= 1,
          str(engine.compression_count))

    if isinstance(compressed, list) and compressed:
        check("the system prompt survives", compressed[0].get("role") == "system")
        check("the newest message survives",
              any("summarise what you found" in str(m.get("content", "")) for m in compressed),
              "the tail is what the model is being asked about")

    # -----------------------------------------------------------------------
    print("\nnothing is destroyed")
    # -----------------------------------------------------------------------
    # Compaction removes text from the window, not from memory. If this fails the
    # engine is a summariser with extra steps.
    recall = engine.handle_tool_call("sakur4_recall", {"query": "module 3", "k": 5})
    check("an evicted turn is still retrievable", isinstance(recall, str) and recall.strip() not in
          ("", "No matching memory."), recall[:80] if isinstance(recall, str) else str(recall))

    # -----------------------------------------------------------------------
    print("\nprovider accounting closes the loop MCP alone could not")
    # -----------------------------------------------------------------------
    engine.update_from_response({
        "prompt_tokens": 5000, "completion_tokens": 200,
        "cache_read_tokens": 0, "model": "gpt-5.2", "provider": "openai",
    })
    engine.update_from_response({"prompt_tokens": 6000, "completion_tokens": 200,
                                 "cache_read_tokens": 5000})
    engine.update_from_response({"prompt_tokens": 6200, "completion_tokens": 200,
                                 "cache_read_tokens": 300})
    check("every reported turn was forwarded", engine.provider_cache_turns >= 3,
          str(engine.provider_cache_turns))
    check("a broken prefix was detected", engine.prefix_breaks >= 1,
          "a rewrite that shrank the cached prefix must be reported")

    # A provider that says nothing about caching must not be blamed for a miss.
    quiet = module.Sakur4ContextEngine(base_url=base_url, session_id="hermes-verify")
    quiet.update_from_response({"prompt_tokens": 1000, "completion_tokens": 10})
    check("an unreported cache is not counted as a turn", quiet.provider_cache_turns == 0,
          "absence is not a miss")

    # -----------------------------------------------------------------------
    print("\ndeepcopy, which Hermes needs for sub-agents")
    # -----------------------------------------------------------------------
    import copy as _copy

    original = module.Sakur4ContextEngine(base_url=base_url, session_id="hermes-verify")
    original.update_from_response({"prompt_tokens": 100, "completion_tokens": 5})
    try:
        clone = _copy.deepcopy(original)
        check("deepcopy succeeds", True)
        check("the clone has its own client", clone.client is not original.client)
        check("budget state is carried over",
              clone.last_prompt_tokens == original.last_prompt_tokens)
        clone.last_prompt_tokens = 9999
        check("mutating the clone does not touch the original",
              original.last_prompt_tokens != 9999)
    except Exception as exc:  # noqa: BLE001
        check("deepcopy succeeds", False, f"{type(exc).__name__}: {exc}")

    # -----------------------------------------------------------------------
    print("\nthe methods Hermes calls around a session, not during one")
    # -----------------------------------------------------------------------
    # These six were implemented and never exercised. A no-op where behaviour was intended
    # fails silently: the session keeps working and one guarantee is simply absent.
    fresh = module.Sakur4ContextEngine(base_url=base_url, session_id="hermes-verify")

    # `on_session_start` must pull the Anchor Set immediately. The implementation's own comment
    # says why — "a session that opens with a pinned rule should not spend a turn without it" —
    # and a turn without it is exactly the bug this engine was written to fix.
    fresh.client.call("memory.pin", {
        "content": "the session-start contract must be armed before the first turn",
        "kind": "task_contract",
        "session_id": "hermes-verify",
    })
    fresh.on_session_start("hermes-verify")
    check("on_session_start arms the anchors before the first turn",
          bool(fresh._last_anchors),
          "otherwise the opening turn runs without a pinned rule")
    check("on_session_start adopts the session id it is given",
          fresh.session_id == "hermes-verify")
    fresh.on_session_start("hermes-verify", model={"contextWindow": 40_000})
    check("on_session_start takes the model's window from a dict",
          fresh.context_length == 40_000 and fresh.threshold_tokens == 30_000,
          f"context_length={fresh.context_length} threshold={fresh.threshold_tokens}")

    # `update_model` accepts either a dict or an object, because Hermes passes both.
    class _Model:
        context_length = 12_000

    fresh.update_model(_Model())
    check("update_model takes the window from an object too",
          fresh.context_length == 12_000 and fresh.threshold_tokens == 9_000)
    before = fresh.context_length
    fresh.update_model(None)
    check("update_model with no model changes nothing", fresh.context_length == before)

    # `on_session_reset` clears the per-session counters. Without it a new session inherits the
    # previous one's compaction count and the status line reports a session that never happened.
    fresh.update_from_response({"prompt_tokens": 500, "completion_tokens": 10})
    fresh.compression_count = 4
    fresh.prefix_breaks = 2
    fresh.on_session_reset()
    check("on_session_reset clears every counter",
          fresh.compression_count == 0 and fresh.provider_cache_turns == 0
          and fresh.prefix_breaks == 0)

    # `has_content_to_compress` is a cheap pre-check, and it has to agree with what `compress`
    # can do: a transcript of only the protected head and tail has no middle to evict.
    minimal = [{"role": "user", "content": f"t{i}"}
               for i in range(fresh.protect_first_n + fresh.protect_last_n)]
    check("has_content_to_compress is False when there is no middle",
          fresh.has_content_to_compress(minimal) is False,
          f"{len(minimal)} messages is exactly the protected head and tail")
    check("has_content_to_compress is True once a middle exists",
          fresh.has_content_to_compress(minimal + [{"role": "user", "content": "middle"}]) is True)

    # `prune_tool_results_only` is deliberately a no-op beyond committing. What matters is that
    # it returns the shape Hermes expects — messages and a reclaimed count — and drops nothing,
    # because a tool result dropped on a cheap trigger cannot be recovered by a later plan.
    transcript = [
        {"role": "system", "content": "you are a coding agent"},
        {"role": "user", "content": "read the config"},
        {"role": "tool", "name": "read_file", "content": "max_attempts = 3"},
        {"role": "assistant", "content": "three attempts"},
        {"role": "user", "content": "and the timeout?"},
    ]
    pruned, reclaimed = fresh.prune_tool_results_only(transcript, current_tokens=9_000)
    check("prune_tool_results_only returns a (messages, count) pair",
          isinstance(pruned, list) and isinstance(reclaimed, int),
          f"got {type(pruned).__name__}, {type(reclaimed).__name__}")
    check("prune_tool_results_only drops nothing",
          pruned == transcript,
          "the plan decides evictions; a cheap trigger must not pre-empt it")
    recalled = fresh.client.call("memory.recall", {
        "query": "max_attempts", "k": 3, "session_id": "hermes-verify",
    })
    check("the tool result it declined to prune is still retrievable",
          isinstance(recalled, dict) and "max_attempts" in json.dumps(recalled),
          "committing is the half that must still happen")

    # The tool schema is a contract with a Pydantic client: one malformed entry makes the client
    # reject the WHOLE catalog. That happened once inside the daemon, with a bare
    # `serde_json::Value` output schema, so the shape is checked rather than assumed.
    schemas = fresh.get_tool_schemas()
    check("get_tool_schemas returns a list", isinstance(schemas, list) and len(schemas) == 1)
    schema = schemas[0] if schemas else {}
    check("the tool schema has the fields a client requires",
          isinstance(schema.get("name"), str)
          and isinstance(schema.get("description"), str)
          and isinstance(schema.get("parameters"), dict),
          "one malformed entry makes a Pydantic client reject the entire catalog")
    params = schema.get("parameters") or {}
    check("the parameters schema declares a type and properties",
          params.get("type") == "object" and isinstance(params.get("properties"), dict),
          f"parameters={params!r}"[:120])

    check("handle_tool_call rejects a name it does not own",
          "unknown" in fresh.handle_tool_call("someone_elses_tool", {}))
    check("handle_tool_call asks for a query rather than guessing",
          "needs a query" in fresh.handle_tool_call("sakur4_recall", {}))
    check("handle_tool_call answers a real query",
          "max_attempts" in fresh.handle_tool_call(
              "sakur4_recall", {"query": "max_attempts", "k": 3}),
          "the tool the model can call has to return the thing it promised")

    # -----------------------------------------------------------------------
    print("\na missing daemon must not break a session")
    # -----------------------------------------------------------------------
    offline = module.Sakur4ContextEngine(base_url="http://127.0.0.1:1", session_id="offline")
    unchanged = offline.compress([{"role": "user", "content": "hello"}], current_tokens=99999)
    check("compress returns the messages unchanged", unchanged == [{"role": "user", "content": "hello"}],
          "deferring is the honest failure; inventing a summary would discard a conversation")
    check("select_context returns None when there is nothing to inject",
          offline.select_context([{"role": "user", "content": "hi"}]) is None)
    check("status still reports", isinstance(offline.get_status(), dict))

    print()
    if FAILURES:
        print(f"{len(FAILURES)} contract(s) failed: {', '.join(FAILURES)}")
        return 1
    print("all contracts pass")
    return 0


if __name__ == "__main__":
    sys.exit(main())
