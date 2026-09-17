# Driving the Hermes ContextEngine with a live model

This records how to get FR-16 running against a real model, what was verified, and
exactly where it stopped. The stopping point matters: it is a **Hermes provider-routing
problem**, not a defect in the engine, and saying so saves the next person the search.

## What was verified

**The engine loads in a real Hermes install.** With `context.engine: sakur4` set and the
plugin present in `$HERMES_HOME/plugins/sakur4/`:

```python
>>> from hermes_cli.plugins import discover_plugins, get_plugin_context_engine
>>> discover_plugins()
>>> get_plugin_context_engine()
Sakur4ContextEngine
    name: sakur4
    url: http://127.0.0.1:8771
```

That is the whole registration path, end to end: discovery → `register(ctx)` →
`ctx.register_context_engine` → the class Hermes will instantiate. It works.

**`HERMES_HOME` isolates a run completely.** Pointing it at a scratch directory redirects
both `get_hermes_home()` and config loading, so a test session needs no changes to a real
profile. This is how the check in `verify.mjs` runs without touching anything.

**Three enabling details, each of which cost time to find:**

1. **`context.engine` must not be the default.** `_select_context_engine` returns `None`
   immediately when the engine name is `compressor`, so a plugin is never consulted. Any
   other name takes the plugin path.
2. **A plugin must be *enabled*, not merely discovered.** `hermes plugins list` showed
   `sakur4 … not enabled` and the loader agreed — `get_plugin_context_engine()` returned
   `None` until `hermes plugins enable sakur4` had run. Discovery and activation are
   separate steps.
3. **The ContextEngine loader reads the repo, not `HERMES_HOME`.** `_CONTEXT_ENGINE_PLUGINS_DIR`
   is `…/hermes-agent/plugins/context_engine/`, so `plugins/context_engine/<name>/` cannot
   be redirected by an environment variable. The general plugin system at
   `$HERMES_HOME/plugins/` can, and that is the path this engine uses.

## The custom-provider shape, which is not the obvious one

Worth writing down because three plausible shapes are all wrong:

```yaml
# WRONG — a map; the loader expects a list
custom_providers:
  local: { base_url: ... }

# WRONG — the provider id is not the key, and `default_model` is ignored
model:
  default: local/Qwen3.8-27B

# RIGHT
model:
  default: custom:llama/Qwen3.8-27B   # slug from custom_provider_slug(name)
custom_providers:
  - name: LLaMa                        # → slug `custom:llama`
    base_url: http://host:8080/v1
    api_key: local-only                # `key_env: NAME` also works if NAME is in .env
    model: Qwen3.8-27B
    models:
      Qwen3.8-27B: {}
    models_discovered: true
```

`custom_provider_slug("LLaMa")` returns `custom:llama`, and that — not the name — is the
provider id to use in `model.default` or `--model`. A `.env` in `$HERMES_HOME` is read, and
without one nothing resolves at all.

## Where it stopped

A one-shot session never reaches the intended model. Selecting it four different ways all
produce the same failure:

```console
$ hermes -z "Reply with exactly: HERMES_OK"
HTTP 401: Invalid Authentication          # or: The API Key appears to be invalid…
```

The wording varies because **it comes from whichever fallback provider answers**, not from
the model that was asked for. Six things were ruled out by measurement:

| Ruled out | How |
|---|---|
| The local model server | answers `200` with **and without** an `Authorization` header |
| The engine | the failure is identical whether `context.engine` is `sakur4` or unset |
| The config syntax | `load_config()` returns the provider, engine and model exactly as written |
| The engine's daemon | answers MCP over curl, and reports `anchors: 1` after a pin |
| The plugin | registers and loads (`Sakur4ContextEngine`, `name: sakur4`) |
| Process spawning | `Start-Process cmd /c echo` exits 0 |

The decisive test is a listener this repository controls
([`docs/verification/capture-request.ps1`](../../docs/verification/capture-request.ps1)),
with the local provider's `base_url` pointed at it. **The capture file is never written** —
nothing arrives — so the request does not reach the endpoint that was configured, however
it was selected:

- `--model custom:llama/Qwen3.8-27B` — nothing arrives
- inherited `modelRoles`, with the config's own default — nothing arrives
- a hand-written `custom_providers` entry naming the provider — nothing arrives
- the user's working config copied wholesale plus `context.engine: sakur4` — nothing arrives

## The one Hermes behaviour worth knowing from this

**A `--model` override that does not resolve fails silently.** The config's
`model.default` is a cloud model, so when the override is not understood Hermes uses that
instead — and the error names *its* credentials:

```console
$ hermes -z "Reply with exactly: HERMES_OK" --model custom:llama/Qwen3.8-27B
HTTP 401: Invalid Authentication
```

Nothing in that message says the model was not found, and `hermes doctor` reports
"✓ API key or custom endpoint configured". From outside the harness this cost a round of
work; from inside it is one `hermes status`.

**The local provider is fine.** Pointed at directly, the same server answers `200`. The
problem is entirely in how Hermes resolves a model string for a custom provider.

## What this does and does not mean

**Verified:** the engine registers, is instantiated by Hermes, talks to a live daemon, and
passes 44 contracts against it. The integration is correct.

**Not verified:** a Hermes session driving a model with this engine active. The in-turn
behaviour question stays open here, exactly as model-initiated tool use stays open for OMP.

**Not a defect in Sakur4.** The request never reaches Sakur4 or the model server, and the
same engine passes its contracts against the same daemon.

## To finish it — five minutes, from inside

The remaining step cannot be done from outside the harness, because the failing seam is
Hermes' own model resolution. On a profile where a session already works:

```bash
# 1. Confirm which model a session actually uses. No override — use the configured one.
hermes status
hermes -z "Reply with exactly: OK"        # this must succeed before going further

# 2. Only then add the engine, and change nothing else.
#    context:
#      engine: sakur4
hermes -z "Reply with exactly: OK"        # still succeeds?

# 3. Ask something only memory can answer.
hermes -z "What is the deployment codename? One line, or UNKNOWN."
```

A **correct** codename at step 3 proves the engine injected the Anchor Set into a real
Hermes request — the same contract the OMP extension was found violating, and the reason
this is worth finishing. `UNKNOWN` means injection is not reaching the request, and the
engine needs the treatment the OMP extension got.

If a `--model` override appears to be ignored, that is the failure mode above rather than
the engine: check `hermes status` before blaming anything downstream.
