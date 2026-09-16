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

A one-shot session in an isolated profile could not be made to reach the local server:

```console
$ HERMES_HOME=/tmp/hermes-test-home hermes -z "Reply with exactly: HERMES_OK"
HTTP 401: The API Key appears to be invalid or may have expired.
```

That message is **OpenRouter's wording** — Hermes fell back to a cloud provider rather
than routing to the configured local one. The local server itself is fine, and this was
checked directly rather than assumed:

```console
$ curl -s -X POST http://your-llama-server:8080/v1/chat/completions \
       -H 'Authorization: Bearer local-only' -d '{...}'
HTTP 200
$ curl -s -X POST http://your-llama-server:8080/v1/chat/completions -d '{...}'
HTTP 200
```

Both with and without an `Authorization` header, so the credential is not the problem.
`hermes doctor` reports "✓ API key or custom endpoint configured" with no complaint about
the custom provider, and `agent.log` records nothing for the failing session. The routing
decision happens somewhere that neither `doctor` nor the log surfaces.

Continuing would have meant either enabling the debug logger and reading Hermes' provider
resolution, or changing a **live** profile — and neither is worth doing blind to someone
else's working install.

## What this does and does not mean

**Verified:** the engine registers, is instantiated by Hermes, talks to a live daemon, and
passes 27 contracts against it. The integration is correct.

**Not verified:** a Hermes session driving a model with this engine active. So the
tool-selection question — does the engine behave well *inside a turn* — remains open here,
exactly as it does for OMP's model-initiated tool use.

**Not a defect in Sakur4.** Every failure above is in provider routing, which is Hermes'
own configuration, and the same engine passes its contracts against the same daemon.

## To finish it

On a profile whose provider routing already works — the live one, or a copy of it:

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
