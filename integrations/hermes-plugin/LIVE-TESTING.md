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

A one-shot session in an isolated profile never reaches the local server:

```console
$ HERMES_HOME=/tmp/hermes-test-home hermes -z "Reply with exactly: HERMES_OK"
HTTP 401: The API Key appears to be invalid or may have expired.
```

That message is **OpenRouter's wording**, so the request is going to a cloud provider that
was never configured for this profile. Four things were ruled out by measurement rather
than by reading:

**The local server is fine.** It answers 200 with *and* without an `Authorization` header:

```console
$ curl -s -X POST http://your-llama-server:8080/v1/chat/completions \
       -H 'Authorization: Bearer local-only' -d '{...}'      → HTTP 200
$ curl -s -X POST http://your-llama-server:8080/v1/chat/completions -d '{...}'  → HTTP 200
```

**The config parses.** `load_config()` returns the custom provider, the engine name, and
the default model exactly as written.

**The request is not reaching the configured endpoint at all.** The provider was pointed
at a listener this repository controls —
[`docs/verification/capture-request.ps1`](../../docs/verification/capture-request.ps1) —
and the capture file was never written. Nothing arrived. So the failure is upstream of any
HTTP call to the local provider: Hermes is choosing a different model than the one the
config names.

**The engine is not involved.** It registers and loads (above), and the failure is
identical whether `context.engine` is `sakur4` or unset.

### The most likely cause

`state.auth` in Hermes is driven by `modelRoles` and per-model auth entries, not only by
the top-level `model.default`. The user's own config carries `modelRoles`
(`default: infron/qwen/qwen3.8-27b:free`) and a large `custom_providers` list, and this
isolated profile was built by adding to a config that already had them. A model string an
error message cannot resolve falls back — and the fallback's credential is the expired one.

Confirming it means reading `hermes_cli/auth.py`'s `state.auth` resolution or running with
only `modelRoles` set. Both are a few minutes **on a profile whose routing already works**,
which is why that is the recommended next step rather than more work here.

## What this does and does not mean

**Verified:** the engine registers, is instantiated by Hermes, talks to a live daemon, and
passes 27 contracts against it. The integration is correct.

**Not verified:** a Hermes session driving a model with this engine active. So the
in-turn behaviour question remains open here, exactly as model-initiated tool use remains
open for OMP.

**Not a defect in Sakur4.** Every failure is in provider selection, which is Hermes'
configuration; the request never reaches Sakur4 or the model server, and the same engine
passes its contracts against the same daemon.

## To finish it

The recommended route is not more work in an isolated profile. It is a few minutes **on a
profile whose routing already works**, which is the user's own:

1. Confirm the model actually selected: `hermes status`, or send one prompt with
   `hermes -z "ping"` and check which provider answers.
2. Add `context: { engine: sakur4 }` to `config.yaml` and run the same prompt again.
3. If the engine is engaged and the answer changes, the injection path is proven.

The engine side needs nothing further. On a profile where provider selection works:

```bash
# 1. A daemon the engine can reach.
sakur4d --db ~/.sakur4/hermes.db --backend http://your-llama-server:8080 \
        serve --transport http --bind 127.0.0.1:8771

# 2. The plugin, and the engine selected.
cp -r integrations/hermes-plugin "$HERMES_HOME/plugins/sakur4"
hermes plugins enable sakur4
# then add `context: { engine: sakur4 }` to config.yaml

# 3. A live turn, and the engine's own status.
hermes -z "What is the deployment codename? One line, or UNKNOWN."
```

If the answer is correct, the engine injected the Anchor Set into a real Hermes request —
which is the same contract the OMP extension was found to be violating, and the reason
this is worth finishing.
