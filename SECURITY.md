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

**Optional encryption at rest is not implemented.** FR-20 in the PRD asks for it
and the `sqlcipher` feature of `libsqlite3-sys` is available, but it is not wired
up. On a shared or untrusted machine, the store is readable by anyone with file
access to it. If that matters to you, it matters today, and there is no
configuration flag that fixes it yet.

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

`cargo deny` and `cargo audit` are not wired into CI yet. Until they are, review
`Cargo.lock` changes in a pull request rather than assuming something else does.
