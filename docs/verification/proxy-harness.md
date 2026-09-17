# A real harness through the reverse proxy

Status: **resolved.** The proxy works with a real harness. The hang was the OMP extension and
the proxy both managing context at once, and it is now a documented configuration rule rather
than a mystery.

## The finding

```console
$ # proxy with --backend http://100.98.158.87:8080, OMP pointed at it, extension LOADED
$ omp -p "Reply with exactly: HARNESS_VIA_PROXY" ...
[timed out after 600000ms]
$ # the proxy's log shows it never received a single request

$ # the same proxy, the same OMP invocation, extension DISABLED
$ omp -p "Reply with exactly: NO_EXT_OK" ... --no-extensions
NO_EXT_OK
elapsed: 19.9s
```

**`--no-extensions` turns an indefinite hang into a 20-second answer.** That is the whole
diagnosis, and it also explains the confusing half-result: the embedded-backend proxy worked
*with* the extension loaded, so the extension alone was not sufficient to hang it — the
combination of the extension and a proxy probing a real upstream was.

## Three ways to run a long session, and only two of them are safe

**Proxy only.** Point OMP at the proxy and disable the extension:

```bash
sakur4d --db ~/.sakur4/proxy.db --backend http://<host>:8080 \
        proxy --bind 127.0.0.1:8090 --upstream http://<host>:8080
omp -p "..." --model local/Qwen3.8-27B --no-extensions
```

The proxy does the context management; the harness needs no plugin system at all. This is
FR-18's intended shape and it is verified.

**Extension only.** Point OMP at the model server directly and let the extension manage
context. This is the round-11 path, verified end to end, and it is what the default
configuration does.

**Both — do not.** The extension and the proxy each retrieve, commit, and rewrite, so a turn
is managed twice and the harness hangs before its first request. Whether that is a deadlock, a
retry storm, or something in OMP's connection handling is not diagnosed; what is established is
that the combination is the trigger, reproducibly, and that either alone is fine.

That redundancy is not merely a bug. Both layers do the same job, so running both spends the
work twice and the second layer's decisions are made about a transcript the first has already
rewritten.

## What is verified

| Configuration | Result |
|---|---|
| OMP → proxy (`--backend embedded`), extension loaded | completes, 94s |
| OMP → proxy (`--backend http://…`), extension loaded | **hangs** |
| OMP → proxy (`--backend http://…`), `--no-extensions` | completes, 19.9s |
| OMP → model server directly | completes, 31s |
| Proxy contracts, real transcript, real server | 10 of 10 |

The proxy's own behaviour is verified independently of any harness, which is what the last row
covers: a harness hanging before it sends a request cannot say anything about what the proxy
does with requests.

## What would have caught this sooner

Nothing in the suite, and that is the point worth recording. Every check drove the proxy with a
scripted client; none drove it with a harness that has its *own* context management running.
The failure needed two correctly-working components and a user's realistic configuration — which
is the fourth time this project has found a bug in exactly that seam:

- Anchors never reached the model in OMP until the `context` hook was changed.
- Hermes' `resources/read` needed `_meta`, and the error body was discarded.
- The Hermes engine matched plan fields that do not exist, so compaction did nothing.
- This one.

A check for it would mean running a real harness in CI, which is not available. What is
available is the configuration rule above, written down where a user will hit it.

## Reproducing

```bash
# Works — proxy only
sakur4d --db /tmp/a.db --backend http://100.98.158.87:8080 --context-window 81920 \
        proxy --bind 127.0.0.1:8162 --upstream http://100.98.158.87:8080 &
# OMP baseUrl → http://127.0.0.1:8162/v1
omp -p "Reply with exactly: OK" --model local/Qwen3.8-27B \
    --thinking off --no-session --no-lsp --no-skills --no-extensions

# Hangs — both layers managing context
omp -p "Reply with exactly: OK" --model local/Qwen3.8-27B \
    --thinking off --no-session --no-lsp --no-skills
```
