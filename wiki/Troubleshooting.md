# Troubleshooting

> **Most problems here are one of four things:** the binary is not on `PATH`, the store is not where you
> think, the backend cannot do what was asked, or a tool was called in a way its arguments do not allow.
> This page is organised by symptom.

---

## First: ask Sakur4 what it thinks

```bash
sakur4d doctor
```

This reports the schema version, the store path and its size, the journal mode, the backend and **what
capabilities it actually detected**, the tokenizer, and the embedder. If you are reporting a bug, this
output is the single most useful thing to attach.

```bash
sakur4d doctor            # the resolved configuration, and what it detected
sakur4d config claude     # the configuration it would print for a harness
```

**There is no `sakur4d status` subcommand** — this page said there was. Counts and coherence come from
`doctor`, or from the MCP tool `sakur4.status`, or from `/sakur4 status` in the OMP extension.

---

## Nothing can find the binary

**Symptom:** `Sakur4: no sakur4d binary found`, or the plugin installs and its tools never appear.

```bash
which sakur4d          # or: where.exe sakur4d    (Windows)
```

If that prints nothing, it is not on `PATH`. Either move it there — `~/.cargo/bin` is the usual place —
or point at it directly:

```bash
export SAKUR4_BIN=/full/path/to/sakur4d     # $env:SAKUR4_BIN on Windows
```

The extension's warning lists **every path it searched**, so you can see where your install actually
landed rather than guessing.

---

## The daemon starts and immediately stops

**Symptom:** a harness reports the server exited, or a session ends without an answer.

Run it by hand, with the banner, and read stderr:

```bash
sakur4d --db ./test.db serve --transport stdio --verbose
```

Common causes:

| Cause | Fix |
|---|---|
| The store path is unwritable | `--db` to a path you own, or check permissions on `~/.sakur4/` |
| Another daemon holds the store | SQLite allows one writer; stop the other process |
| The backend URL is unreachable and the probe blocks | `--backend none` to start without inference, or raise `--probe-timeout-ms` |

---

## "backend unavailable: …" on a save or snapshot

**Not a Sakur4 failure.** The inference server cannot do what was asked — most often it does not
implement `?action=save` for slot checkpoints.

```bash
sakur4d doctor
# capabilities     slots+tokenize
# coherence        no checkpoint source detected — compaction will report full re-prefill
```

That is the correct answer for such a server. Sakur4 **degrades** rather than failing: prefix reuse still
works through longest-common-prefix matching, and the receipt is deliberately **pessimistic** — it
reports `full-re-prefill` where reuse is real but unverifiable.

If you see this on every turn and want it quiet, `SAKUR4_EVICTION_PROFILE=window-first` is the profile
chosen for backends with no checkpoint source.

---

## Recall finds nothing

Work through it in this order:

1. **Is anything stored?** `sakur4d doctor` — `episodes` and `project_id`.
2. **Are you in the same project?** Recall is scoped to the project the daemon was started for. A
   session in a different directory has its own memory, and `store_holds_other_projects` tells you the
   store holds more than this project's.
3. **Was the file indexed?** `sakur4d index` — nothing re-indexes automatically. `sakur4d repo-map`
   dates itself so you can see when it last ran.
4. **Is the embedder lexical?** With no embedding endpoint configured, recall uses a deterministic
   hashing embedder — good at words, poor at paraphrase. `sakur4d doctor` names the embedder in use.

---

## The repository map is out of date

It is a snapshot from the last index, and **nothing watches the filesystem**. Run:

```bash
sakur4d index            # incremental: only files whose content hash changed are re-parsed
```

The map now prints when it was built, or says it has never been indexed, so you can tell before trusting
it. `code.impact_of_change` reasons over the same stored facts.

---

## A tool call is rejected for its arguments

Every tool validates its input and says which field is wrong. The common ones:

| Message | Meaning |
|---|---|
| `request _meta is missing or has malformed required fields` | The 2026-07-28 revision is stateless: every request carries `io.modelcontextprotocol/protocolVersion` and `io.modelcontextprotocol/clientCapabilities` in `_meta`, with **no handshake first** |
| `symbol X is not in the Symbolic Ledger` | The qualified name is wrong or the project is not indexed. `sakur4d repo-map --names` lists the real ones — they are paths like `src::lib::helper`, not bare names |
| `unknown anchor kind: constraint` | Anchor kinds are a fixed set; check the tool's schema for the accepted values |

---

## A count looks wrong — for instance `episodes: 0`

**If you batched the calls, this is the known ordering defect.** MCP permits a server to process a queued
batch in any order, and a read can be served before the write it was sent after. Send a call and await
its answer; see [Limitations](Limitations).

**If you awaited each call**, check whether the write actually landed:

```bash
sqlite3 ~/.sakur4/sakur4.db "SELECT COUNT(*) FROM episodic_stream"
```

If the store has the row and the tool reports zero, that is a real bug worth reporting — attach
`sakur4d doctor`.

---

## Oh My Pi: the plugin is installed but its tools are missing

```bash
SAKUR4_PLUGIN_LOG=/tmp/sakur4.log omp
cat /tmp/sakur4.log
```

The log names the binary paths it searched and why each was rejected. **OMP version matters**: the plugin
is verified against 18.2.x, and its `package.json` deliberately declares no version floor — so an older
OMP will load it without complaint and then behave unpredictably.

---

## The install script refuses to install

**That is the script working.** It fetches `SHA256SUMS.txt` and will not install an archive that is not
listed or does not match it:

```
sakur4d-v0.1.0-x86_64-unknown-linux-gnu.tar.gz is not listed in SHA256SUMS.txt; refusing to install
```

Check that the release you are asking for exists (`SAKUR4_VERSION=v0.1.0`), and that you are not behind a
proxy that rewrites response bodies. **Do not work around it by downloading the archive directly** — a
checksum failure means the download is not what was published, and the reason matters more than the
inconvenience.

---

## The tests fail in a way that looks structural

```bash
cargo clean          # stale incremental artefacts cause odd link errors
node verify.mjs --list   # what would actually run, and what it needs
```

**A skipped check is not a passed check.** The verifier reports `INCOMPLETE` for any group that skipped
and names every skip with its reason. If a group you expect to run reports `INCOMPLETE`, the reason is
printed — it is usually a missing tool rather than a failure.

---

## Still stuck

Open an issue with `sakur4d doctor` output, the exact command, and what you expected. If you have a
reproduction, that is worth more than a description.

---

<sub>[← Back to Home](Home) · [All pages](Home#where-to-go-next)</sub>
