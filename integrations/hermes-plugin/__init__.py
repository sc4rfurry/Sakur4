"""Sakur4 context engine for Hermes (FR-16).

Hermes normally compacts by summarising: a model reads the transcript and writes a
shorter one. That works, and it invalidates the prompt cache, because the rewritten
prompt shares no prefix with what the provider already holds — Hermes' own
documentation calls this "the strongest argument against" per-turn compaction.

This engine replaces the summariser with Sakur4's Graduated Eviction Engine, which
decides what leaves the context from token counts, recency, graph in-degree and explicit
droppability, and preserves a prefix while doing it. On a provider that reuses prefixes
the preserved head is then not re-billed.

It also closes a loop the MCP-only integration could not: `update_from_response` receives
the provider's own token accounting on every call, including cache read/write counts, so
Sakur4 measures prompt-cache behaviour automatically instead of being told about it.

# What Hermes asks of an engine

Three abstract methods — `name`, `update_from_response`, `should_compress`, `compress` —
plus a handful of documented attributes that `run_agent.py` reads directly
(`last_prompt_tokens`, `threshold_tokens`, `compression_count`, …). `__deepcopy__` is
implemented because Hermes deep-copies the engine for sub-agents, and the default copy
would duplicate the HTTP connection pool.

# Failure policy

Every daemon call is best-effort. If `sakur4d` is not running, or returns something
unexpected, the engine falls back to what Hermes would have done anyway rather than
raising into the agent loop. A context engine that breaks a session when its sidecar is
down is worse than no context engine — the same rule the OMP extension follows.
"""

from __future__ import annotations

import copy
import json
import logging
import os
import shutil
import subprocess
import urllib.error
import urllib.request
from typing import Any, Dict, List, Optional

from agent.context_engine import ContextEngine

logger = logging.getLogger(__name__)

_EXE = "sakur4d.exe" if os.name == "nt" else "sakur4d"
_DEFAULT_TIMEOUT = 20.0


def _find_daemon() -> Optional[str]:
    """Locate ``sakur4d`` the same way the OMP extension does.

    Deliberately the same search order, so a user who has it working in one harness does
    not discover it missing in another. `shutil.which` covers PATH; the explicit paths
    cover `cargo install` before the shell has been restarted with the new PATH entry,
    which is the most common way a correct install looks absent.
    """
    configured = os.environ.get("SAKUR4_BIN")
    if configured and os.path.exists(configured):
        return configured

    home = os.path.expanduser("~")
    for candidate in (
        os.path.join(home, ".cargo", "bin", _EXE),
        os.path.join(home, ".local", "bin", _EXE),
        os.path.join(home, ".sakur4", "bin", _EXE),
        os.path.join(os.getcwd(), "target", "release", _EXE),
        os.path.join(os.getcwd(), "target", "debug", _EXE),
    ):
        if os.path.exists(candidate):
            return candidate

    return shutil.which("sakur4d")


class Sakur4Client:
    """A minimal MCP client over the daemon's streamable-HTTP transport.

    Synchronous on purpose: the `ContextEngine` interface is synchronous, and running an
    event loop inside `compress()` would fight the host's.
    """

    def __init__(self, base_url: str, store: Optional[str] = None, timeout: float = _DEFAULT_TIMEOUT):
        self.base_url = base_url.rstrip("/")
        self.store = store
        self.timeout = timeout
        self._project_id: Optional[str] = None
        self._project_probed = False

    def call(self, tool: str, arguments: Dict[str, Any]) -> Optional[Dict[str, Any]]:
        """Call one MCP tool; ``None`` on any failure, with the reason logged once."""
        body = json.dumps({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": tool,
                "arguments": arguments,
                "_meta": {
                    "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                    "io.modelcontextprotocol/clientCapabilities": {},
                },
            },
        }).encode("utf-8")

        request = urllib.request.Request(
            f"{self.base_url}/",
            data=body,
            headers={
                "content-type": "application/json",
                "accept": "application/json, text/event-stream",
                "MCP-Protocol-Version": "2026-07-28",
                "Mcp-Method": "tools/call",
                "Mcp-Name": tool,
            },
            method="POST",
        )
        try:
            with urllib.request.urlopen(request, timeout=self.timeout) as response:
                raw = response.read().decode("utf-8", "replace")
        except (urllib.error.URLError, OSError, TimeoutError) as exc:
            logger.debug("sakur4 %s unreachable: %s", tool, exc)
            return None

        payload = None
        for line in raw.splitlines():
            if line.startswith("data: "):
                payload = line[6:]
        if payload is None:
            payload = raw
        try:
            parsed = json.loads(payload)
        except json.JSONDecodeError:
            logger.debug("sakur4 %s returned unparsable content", tool)
            return None

        if parsed.get("error"):
            logger.debug("sakur4 %s failed: %s", tool, parsed["error"])
            return None
        result = parsed.get("result") or {}
        if isinstance(result.get("structuredContent"), dict):
            return result["structuredContent"]
        for part in result.get("content") or []:
            if part.get("type") == "text":
                try:
                    return json.loads(part["text"])
                except json.JSONDecodeError:
                    return {"text": part["text"]}
        return None

    def project_id(self) -> Optional[str]:
        """The daemon's project hash, needed to address project-scoped resources.

        Read from `sakur4.status` rather than computed: the daemon hashes the project
        root, and reproducing that hash here would be a second implementation of
        something the daemon already knows.
        """
        if self._project_probed:
            return self._project_id
        self._project_probed = True
        status = self.call("sakur4.status", {})
        if isinstance(status, dict):
            value = status.get("project_id")
            if isinstance(value, str) and value:
                self._project_id = value
        return self._project_id

    def anchors(self) -> str:
        """The Anchor Set as text, or an empty string.

        Anchors are the one category that must reach the model regardless of what the
        user asked — that is what pinning means — so the engine injects them rather than
        leaving them to a retrieval query that a short rule would never match.

        # The `_meta` block is required, and its absence is silent

        A 2026-07-28 request carries the protocol revision and client capabilities in
        per-request `_meta` rather than in a negotiated handshake. Omitting it is not a
        warning: the daemon answers `400 Invalid params: request _meta is missing`, and
        the first version of this method did omit it, so anchors silently never loaded
        and every pinned constraint stayed invisible to the model. The failure looked
        exactly like "there are no anchors" — which is why the body is read on an error
        response rather than discarded.
        """
        project = self.project_id()
        if not project:
            return ""
        body = json.dumps({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "resources/read",
            "params": {
                "uri": f"sakur4://anchors/{project}",
                "_meta": {
                    "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                    "io.modelcontextprotocol/clientCapabilities": {},
                },
            },
        }).encode("utf-8")
        request = urllib.request.Request(
            f"{self.base_url}/",
            data=body,
            headers={
                "content-type": "application/json",
                "accept": "application/json, text/event-stream",
                "MCP-Protocol-Version": "2026-07-28",
                "Mcp-Method": "resources/read",
                # The spec requires Mcp-Name on requests that name a target, and a
                # resource read names a URI.
                "Mcp-Name": f"sakur4://anchors/{project}",
            },
            method="POST",
        )
        try:
            with urllib.request.urlopen(request, timeout=self.timeout) as response:
                raw = response.read().decode("utf-8", "replace")
        except urllib.error.HTTPError as exc:
            # Read the body: it names what was wrong, and discarding it is how a 400
            # becomes indistinguishable from "nothing to report".
            detail = ""
            try:
                detail = exc.read().decode("utf-8", "replace")[:200]
            except Exception:  # noqa: BLE001 - diagnostics must not raise
                detail = str(exc)
            logger.debug("sakur4 anchors read failed (%s): %s", exc.code, detail)
            return ""
        except (urllib.error.URLError, OSError, TimeoutError) as exc:
            logger.debug("sakur4 anchors unreachable: %s", exc)
            return ""

        payload = raw
        for line in raw.splitlines():
            if line.startswith("data: "):
                payload = line[6:]
        try:
            parsed = json.loads(payload)
        except json.JSONDecodeError:
            return ""
        for part in (parsed.get("result") or {}).get("contents") or []:
            text = part.get("text")
            if isinstance(text, str) and text and "is empty" not in text.lower():
                return text
        return ""


class Sakur4ContextEngine(ContextEngine):
    """Hermes' context engine, backed by the Sakur4 daemon."""

    name = "sakur4"

    # Read directly by run_agent.py, so they exist as class attributes as well as
    # being maintained per instance.
    last_prompt_tokens: int = 0
    last_completion_tokens: int = 0
    last_total_tokens: int = 0
    threshold_tokens: int = 0
    context_length: int = 0
    compression_count: int = 0

    def __init__(self, base_url: Optional[str] = None, session_id: Optional[str] = None, **kwargs: Any):
        super().__init__()
        # `**kwargs` on purpose: the host passes configuration it expects engines to
        # tolerate, and raising on an unknown key would make this engine fail to load
        # for a reason unrelated to whether it works.
        self.base_url = (
            base_url
            or os.environ.get("SAKUR4_URL")
            or "http://127.0.0.1:8765"
        )
        self.session_id = session_id or os.environ.get("SAKUR4_SESSION") or "hermes"
        self.client = Sakur4Client(self.base_url, store=os.environ.get("SAKUR4_DB"))

        self.threshold_percent = float(os.environ.get("SAKUR4_THRESHOLD_PERCENT", "0.75"))
        self.protect_first_n = 3
        self.protect_last_n = 6

        self.provider_cache_turns = 0
        self.prefix_breaks = 0
        self._last_anchors = ""

    # -- needed because Hermes deep-copies the engine for sub-agents -------------
    def __deepcopy__(self, memo: Dict[int, Any]) -> "Sakur4ContextEngine":
        """Copy budget state, share nothing that holds a socket.

        Hermes deep-copies the engine so a child's `update_model()` cannot mutate the
        parent's. A `urllib` opener has no state worth copying and copying it is not
        possible, so the budget fields are copied explicitly and a fresh client is
        constructed. Without this, Hermes logs a warning and silently falls back to the
        built-in compressor — the failure looks like "the engine is not working" rather
        than "the engine could not be copied".
        """
        clone = Sakur4ContextEngine.__new__(Sakur4ContextEngine)
        memo[id(self)] = clone

        clone.base_url = self.base_url
        clone.session_id = self.session_id
        clone.client = Sakur4Client(self.base_url, store=self.client.store)
        clone.threshold_percent = self.threshold_percent
        clone.protect_first_n = self.protect_first_n
        clone.protect_last_n = self.protect_last_n
        clone.provider_cache_turns = self.provider_cache_turns
        clone.prefix_breaks = self.prefix_breaks
        clone._last_anchors = self._last_anchors

        for field in (
            "last_prompt_tokens",
            "last_completion_tokens",
            "last_total_tokens",
            "threshold_tokens",
            "context_length",
            "compression_count",
        ):
            setattr(clone, field, copy.copy(getattr(self, field, 0)))
        return clone

    # -- lifecycle --------------------------------------------------------------

    def on_session_start(self, session_id: str, **kwargs: Any) -> None:
        if session_id:
            self.session_id = session_id
        # Pull the Anchor Set once at start so the first turn can inject it; a session
        # that opens with a pinned rule should not spend a turn without it.
        self._last_anchors = self.client.anchors()
        model = kwargs.get("model")
        if model:
            self.update_model(model)

    def on_session_reset(self) -> None:
        self.compression_count = 0
        self.provider_cache_turns = 0
        self.prefix_breaks = 0

    def update_model(self, model: Any = None, **kwargs: Any) -> None:
        """Track the model's context length, which is what `should_compress` tests."""
        if model is None:
            return
        length = None
        if isinstance(model, dict):
            length = model.get("contextWindow") or model.get("context_length") or model.get("n_ctx")
        else:
            length = getattr(model, "context_window", None) or getattr(model, "context_length", None)
        if isinstance(length, int) and length > 0:
            self.context_length = length
            self.threshold_tokens = int(length * self.threshold_percent)

    # -- the accounting loop, which MCP alone could not close -------------------

    def update_from_response(self, usage: Dict[str, Any]) -> None:
        """Record the provider's token accounting with Sakur4.

        This is the hook that makes the cloud half of Sakur4 automatic. The provider
        already reported how many prompt tokens came from its prompt cache; forwarding
        that here is what lets Sakur4 detect a compaction that invalidated the cache —
        a bill, not just a delay — without anyone asking it to look.
        """
        if not isinstance(usage, dict):
            return

        prompt = _as_int(usage.get("prompt_tokens") or usage.get("input_tokens"))
        completion = _as_int(usage.get("completion_tokens") or usage.get("output_tokens"))
        if prompt:
            self.last_prompt_tokens = prompt
        if completion:
            self.last_completion_tokens = completion
        self.last_total_tokens = _as_int(usage.get("total_tokens")) or (
            self.last_prompt_tokens + self.last_completion_tokens
        )
        if not self.context_length:
            self.update_model(usage.get("model"))

        cache_read = _as_int(usage.get("cache_read_tokens"))
        cache_write = _as_int(usage.get("cache_write_tokens"))
        if prompt <= 0 or (cache_read is None and cache_write is None):
            # Nothing to report. Sending a zero would assert a cache miss the provider
            # never reported, and Sakur4 says so rather than blaming a cache it cannot
            # see — so absence is passed through as absence.
            return

        payload: Dict[str, Any] = {"prompt_tokens": prompt, "session_id": self.session_id}
        if completion:
            payload["completion_tokens"] = completion
        if cache_read is not None:
            payload["cache_read_tokens"] = cache_read
        if cache_write is not None:
            payload["cache_write_tokens"] = cache_write
        model = usage.get("model")
        if isinstance(model, str) and model:
            payload["model"] = model
        provider = usage.get("provider")
        if isinstance(provider, str) and provider:
            payload["provider"] = provider

        verdict = self.client.call("context.record_usage", payload)
        if not isinstance(verdict, dict):
            return
        self.provider_cache_turns += 1
        if verdict.get("regression"):
            self.prefix_breaks += 1
            logger.warning(
                "Sakur4: this turn was billed for history that had already been paid for — %s",
                verdict.get("detail", "the cached prefix shrank"),
            )

    # -- compaction -------------------------------------------------------------

    def should_compress(self, prompt_tokens: Optional[int] = None) -> bool:
        """Fire at the same threshold Hermes would, so behaviour is predictable."""
        tokens = prompt_tokens if prompt_tokens is not None else self.last_prompt_tokens
        if not self.threshold_tokens:
            return False
        return tokens >= self.threshold_tokens

    def has_content_to_compress(self, messages: List[Dict[str, Any]]) -> bool:
        # Needs at least the protected head and tail plus something in between, or the
        # plan has nothing to evict and Hermes would run a pass for no reason.
        return len(messages) > (self.protect_first_n + self.protect_last_n + 1)

    def compress(
        self,
        messages: List[Dict[str, Any]],
        current_tokens: Optional[int] = None,
        focus_topic: Optional[str] = None,
        force: bool = False,
        memory_context: str = "",
    ) -> List[Dict[str, Any]]:
        """Compact `messages` by asking Sakur4 what to evict.

        # What this does, and what it refuses to do

        Every message is committed to Sakur4's append-only stream first, so nothing is
        lost: eviction changes what is in the window, never what is stored, and a fact
        from a dropped turn stays retrievable with `memory.recall`. Then the daemon is
        asked to plan an eviction and the messages it decided to evict are replaced by a
        short marker.

        If the daemon is unreachable, or the plan reclaims nothing, the messages are
        returned unchanged. Returning them unchanged is the honest failure: Hermes will
        see the context is still over budget and fall back, whereas inventing a summary
        here would silently discard a conversation on the strength of an empty plan.
        """
        if not messages:
            return messages

        # # Commit, and remember which episode each message became
        #
        # The plan identifies what to evict by `episode_id`, so the engine has to know
        # which message produced which episode. An earlier version committed without
        # keeping the mapping and then tried to match on the plan's `reason` text — which
        # carries no excerpts — so nothing ever matched, `evicted` was always zero, and
        # compaction silently did nothing while reporting success to the caller. That is
        # the worst failure mode available here: the host believes the context shrank.
        committed: List[Optional[str]] = []
        for message in messages:
            text = _message_text(message)
            if not text:
                committed.append(None)
                continue
            role = _role_of(message)
            args: Dict[str, Any] = {
                "role": role,
                "content": text,
                "session_id": self.session_id,
            }
            if role == "tool" and message.get("name"):
                args["tool_name"] = message["name"]
            result = self.client.call("memory.commit_episode", args)
            committed.append(result.get("episode_id") if isinstance(result, dict) else None)

        plan = self.client.call("context.plan_eviction", {
            "session_id": self.session_id,
            "slot_id": "0",
            "apply": True,
        })
        if not isinstance(plan, dict):
            logger.debug("Sakur4 unreachable; leaving context unchanged for Hermes to handle")
            return messages

        reclaimed = _as_int(plan.get("planned_savings")) or 0
        if reclaimed <= 0:
            logger.debug("Sakur4 plan reclaimed nothing (%s); leaving context unchanged",
                         plan.get("summary", "no summary"))
            return messages

        # Anything the plan moves out of `live` leaves the window. The tier names come
        # from the engine's own four-tier ladder, so this compares against `live` rather
        # than enumerating the other three — a new tier would otherwise be silently
        # treated as still present.
        evicted_ids = set()
        for update in plan.get("updates") or []:
            if not isinstance(update, dict):
                continue
            tier = update.get("to_tier") or update.get("to")
            episode_id = update.get("episode_id")
            if isinstance(episode_id, str) and tier and tier != "live":
                evicted_ids.add(episode_id)

        if not evicted_ids:
            # A plan that reclaimed tokens but named nothing is one this engine cannot
            # act on, and guessing which messages to drop would be worse than deferring.
            logger.debug("Sakur4 plan reclaimed %s tokens but named no episodes; deferring", reclaimed)
            return messages

        head = messages[: self.protect_first_n]
        tail = messages[-self.protect_last_n :] if self.protect_last_n else []
        middle_end = len(messages) - len(tail)

        kept: List[Dict[str, Any]] = []
        evicted = 0
        for index, message in enumerate(messages):
            # The protected head and tail stay regardless of what the plan said: they are
            # what the host decided it needs verbatim, and honouring that is the contract.
            if index < self.protect_first_n or index >= middle_end:
                kept.append(message)
                continue
            episode_id = committed[index] if index < len(committed) else None
            if episode_id and episode_id in evicted_ids:
                evicted += 1
                continue
            kept.append(message)

        if evicted == 0:
            logger.debug("Sakur4 named %s episode(s) but none mapped to a middle message",
                         len(evicted_ids))
            return messages

        anchors = self._last_anchors or self.client.anchors()
        self._last_anchors = anchors

        marker_parts = [
            f"[Sakur4 evicted {evicted} earlier message(s), reclaiming {reclaimed} tokens]",
            "Their full text is still in the Memory Fabric and can be retrieved with "
            "memory.recall — nothing was destroyed, only unwindowed.",
        ]
        if plan.get("cache_status"):
            marker_parts.append(f"Cache verdict for the new boundary: {plan['cache_status']}.")
        if anchors:
            marker_parts.append(
                "Pinned constraints, which are never evicted and remain in force:\n" + anchors
            )
        if focus_topic:
            marker_parts.append(f"Focus for this compaction: {focus_topic}")

        marker = {"role": "user", "content": "\n\n".join(marker_parts)}
        self.compression_count += 1
        logger.info("Sakur4 evicted %s message(s), reclaimed %s tokens", evicted, reclaimed)

        # The marker goes where the evicted messages were, not at the end: the tail is
        # the part the model is being asked about, and inserting before it keeps the
        # recent turns adjacent to the request.
        result_messages = head + [marker] + kept[self.protect_first_n :]
        return result_messages

    def prune_tool_results_only(
        self, messages: List[Dict[str, Any]], current_tokens: Optional[int] = None,
    ) -> tuple[List[Dict[str, Any]], int]:
        """No-op beyond committing, deliberately.

        Hermes calls this on a low, cost-oriented trigger to reclaim re-sent tool output.
        Sakur4's plan already treats re-runnable tool results as the first eviction
        candidates (`droppable`), so doing it here as well would evict twice for one
        saving — and a tool result dropped on a cheap trigger cannot be recovered by a
        later plan, because it is already gone from the window.

        The commit still happens, so the content is retrievable either way.
        """
        for message in messages:
            if _role_of(message) == "tool":
                text = _message_text(message)
                if text:
                    self.client.call("memory.commit_episode", {
                        "role": "tool",
                        "content": text,
                        "tool_name": message.get("name"),
                        "session_id": self.session_id,
                    })
        return messages, 0

    # -- what the model can see and do ------------------------------------------

    def select_context(
        self,
        request_messages: List[Dict[str, Any]],
        *,
        conversation_messages: Optional[List[Dict[str, Any]]] = None,
        incoming_message: Optional[Dict[str, Any]] = None,
        budget_tokens: int = 0,
    ) -> Optional[List[Dict[str, Any]]]:
        """Prepend the Anchor Set, so a pinned rule is in front of the model every turn.

        Hermes says the returned list is request-only and must not be treated as
        persisted state — which is exactly right for this: the anchors are re-derived
        each turn, so a rule pinned mid-session appears on the next request without
        anything being written back into the transcript.

        Returns `None` when there is nothing to inject, which the host treats as "leave
        the request unchanged". That matters for caching: returning a copy with no
        change would be a different list object for no benefit.
        """
        anchors = self._last_anchors or self.client.anchors()
        if not anchors:
            self._last_anchors = ""
            return None
        self._last_anchors = anchors

        block = {
            "role": "user",
            "content": (
                "## Pinned constraints — stated earlier in this session, still in force\n\n"
                f"{anchors}\n\n"
                "These were pinned by the user and are exempt from eviction. "
                "Do not contradict them."
            ),
        }
        # Placed after the system prompt and before history: the head of the prompt is
        # what a provider cache keys on, so inserting at the front would invalidate it
        # every turn, which is the failure this whole project is about.
        if request_messages and request_messages[0].get("role") == "system":
            return [request_messages[0], block] + list(request_messages[1:])
        return [block] + list(request_messages)

    def get_tool_schemas(self) -> List[Dict[str, Any]]:
        """Expose recall to the model, so it can look something up rather than guess."""
        return [{
            "name": "sakur4_recall",
            "description": (
                "Search this session's Sakur4 memory for earlier work, decisions or file "
                "contents. Use it when you cannot remember something instead of "
                "reconstructing it — compaction removes text from the window, not from "
                "memory. A result marked STALE is a summary whose source has changed; it "
                "comes with the source's CURRENT VALUE, which supersedes the summary."
            ),
            "parameters": {
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "What to search for."},
                    "k": {"type": "integer", "description": "Maximum results. Default 5."},
                },
                "required": ["query"],
            },
        }]

    def handle_tool_call(self, name: str, args: Dict[str, Any], **kwargs: Any) -> str:
        if name != "sakur4_recall":
            return f"unknown Sakur4 tool: {name}"
        query = (args or {}).get("query")
        if not isinstance(query, str) or not query.strip():
            return "sakur4_recall needs a query"
        limit = _as_int((args or {}).get("k")) or 5
        result = self.client.call("memory.recall", {
            "query": query,
            "k": limit,
            "session_id": self.session_id,
        })
        if not isinstance(result, dict):
            return "Sakur4 is unavailable; could not search memory."
        rendered = result.get("rendered")
        if isinstance(rendered, str) and rendered.strip():
            return rendered
        return "No matching memory."

    # -- status -----------------------------------------------------------------

    def get_status(self) -> Dict[str, Any]:
        """Reported by `/status`; also the fastest way to see whether this is wired up."""
        return {
            "engine": self.name,
            "daemon": self.base_url,
            "session_id": self.session_id,
            "threshold_tokens": self.threshold_tokens,
            "last_prompt_tokens": self.last_prompt_tokens,
            "compressions": self.compression_count,
            "provider_cache_turns_reported": self.provider_cache_turns,
            "prefix_breaks": self.prefix_breaks,
            "anchors_cached": bool(self._last_anchors),
            "context_length": self.context_length,
        }


# ===========================================================================
# Helpers
# ===========================================================================

def _as_int(value: Any) -> Optional[int]:
    if value is None or isinstance(value, bool):
        return None
    try:
        return int(value)
    except (TypeError, ValueError):
        return None


def _role_of(message: Dict[str, Any]) -> str:
    role = message.get("role")
    if role in ("system", "user", "assistant", "tool"):
        return role
    # Hermes has its own roles; anything unrecognised is recorded as an assistant turn
    # rather than dropped, because a dropped turn is one the model cannot recall.
    return "assistant"


def _message_text(message: Dict[str, Any]) -> str:
    content = message.get("content")
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        parts = []
        for part in content:
            if isinstance(part, dict) and isinstance(part.get("text"), str):
                parts.append(part["text"])
            elif isinstance(part, str):
                parts.append(part)
        return "\n".join(parts)
    return ""


# ===========================================================================
# Plugin entry point
# ===========================================================================

def register(ctx) -> None:
    """Register the engine with Hermes' plugin system."""
    engine = Sakur4ContextEngine()
    ctx.register_context_engine(engine)
    logger.debug("Sakur4 context engine registered (%s, session %s)",
                 engine.base_url, engine.session_id)
