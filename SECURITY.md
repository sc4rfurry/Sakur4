# Security Policy

## Reporting a vulnerability

Please do not open a public issue for a security problem. Report it privately
through GitHub's [Security Advisories](../../security/advisories/new) for this
repository, or by contacting the maintainers directly if you cannot use that.

Include what you can: affected version, reproduction steps, and the impact you
believe it has. You will get an acknowledgement, and credit in the fix's release
notes unless you would rather not be named.

## What Sakur4's threat model actually is

Sakur4 runs as a local sidecar for a single developer. That shapes which problems
are real and which are not, and it is worth being explicit rather than implying a
posture the project does not have.

**Sakur4 is not a security boundary.** It stores a verbatim transcript of an agent
session, which in practice can contain anything the agent read: source code, tool
output, credentials that appeared in output, file contents. Treat the Memory Fabric
store (`sakur4.db`) as exactly as sensitive as the sessions it recorded.

**There is no authentication on the MCP transports.** Localhost binding is the
default, and that default is the security control (the PRD's NFR-11). Binding to a
non-loopback address with `--bind 0.0.0.0:...` exposes the entire Memory Fabric,
including the ability to write to it, to anyone who can reach the port. Do not do
it on a network you do not control.

**Optional encryption at rest is implemented, and off by default.** FR-20 asked for it
and it now exists: build with `--features encryption` and open the store through
`Db::open_encrypted`, which issues SQLCipher's `PRAGMA key` before the file is read. The
key must be 64 hex characters — a raw 256-bit key rather than a passphrase, so there is
no PBKDF2 step to attack — and `sakur4d gen-key` generates one. A store written by an
encrypted build is unreadable by a plain SQLite client, which
`crates/sakur4-core/tests/encryption_at_rest.rs` asserts by opening one without the key.

It is not on by default and the default build links plain SQLite, so **a store is
readable by anyone with file access unless you enabled the feature.** If that matters to
you, it matters today, and the switch is a build flag rather than a configuration
option. The feature notes in `crates/sakur4-core/Cargo.toml` record the OpenSSL
requirement on each platform, and CI verifies the feature on Linux.

**Snapshots are as sensitive as the store.** `session.snapshot` writes a slot-save
file — the model's entire KV state for that context — to disk. The size is
60-500 MB per snapshot depending on context length, and the contents are a
representation of everything the session has seen. They live under
`SAKUR4_SNAPSHOT_DIR` (default: a temporary directory), and Sakur4 prunes them by
count, but nothing encrypts them.

**Sakur4 makes no network calls of its own by default.** The PRD requires this
(NFR-10) and the default paths honour it: indexing, embedding, retrieval,
consolidation and MCP serving all function with no outbound access, and the default
embedding path is a local hashing embedder rather than a model download. The
exceptions are explicit: an embedding endpoint you configure, and a llama.cpp
server you point it at — both of which are typically localhost.

## Things that would be security bugs

- A path that reads or writes outside the configured store or snapshot directory.
- Command execution or shell interpolation anywhere in the codebase. There is none
  today; Sakur4 spawns no processes and invokes no shell.
- Prompt injection in stored memory that survives the symbolic/semantic separation.
  Note that Sakur4's dual-track design is partly a *mitigation* here: a tool result
  that contains instructions is stored verbatim in the Episodic Stream and hashed
  into the Symbolic Ledger, but it is never silently rewritten into an
  interpretation, so the raw text stays available for inspection.
- A panic reachable from harness input. The library code has zero
  `unwrap`/`expect`/`panic!` paths outside tests, and a regression that
  reintroduces one on a public path counts as a bug worth reporting.
- Corrupting or losing the Episodic Stream. Writes are transactional and the store
  is opened in WAL mode; a crash must not corrupt it (NFR-5, NFR-6).

## Dependency auditing

Both run in CI. `cargo deny` was wired up in 0.2.1 against `deny.toml`; before that, review
`Cargo.lock` changes in a pull request rather than assuming something else does.
