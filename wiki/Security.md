# Security

> **Sakur4 is not a security boundary.** This page says what its threat model actually is, rather than
> implying a posture the project does not have.

---

## The one sentence that matters

**Sakur4 stores a verbatim transcript of an agent session**, which in practice contains anything the
agent read: source code, tool output, file contents, and credentials that appeared in output.

**Treat the Memory Fabric store — `sakur4.db` — as exactly as sensitive as the sessions it recorded.**

That may be more sensitive than the code they touched, because the transcript includes *what was read*
and *what was said about it*.

---

## No authentication on either transport

Localhost binding is the default, and **that default is the security control.** There is no token, no
password, no per-client identity.

```bash
sakur4d serve --transport http --bind 0.0.0.0:8765   # ← don't, unless you mean it
```

Binding to a non-loopback address exposes **the entire Memory Fabric — including the ability to write to
it — to anyone who can reach the port.** On a shared network that is every device on it. Do not do it on
a network you do not control.

If you need remote access, put it behind something that authenticates: an SSH tunnel, a reverse proxy
with TLS and client certificates, or a private network you administer.

---

## Encryption at rest — implemented, off by default

| | |
|---|---|
| **Feature** | `encryption` (`rusqlite/bundled-sqlcipher`) |
| **Default** | **off** — the standard build links plain SQLite |
| **Key** | 64 hex characters — a raw 256-bit key, not a passphrase |
| **Generate** | `sakur4d gen-key` |
| **Consequence of off** | a store is readable by anyone with file access |

```bash
cargo build --release -p sakur4d --features encryption
sakur4d gen-key                          # prints a key; store it somewhere safe
```

The key is applied as SQLCipher's `PRAGMA key` **before the file is read**, which is a requirement of
SQLCipher rather than a preference — a pragma issued after the header is read is too late.

Because the key is raw rather than derived, **there is no PBKDF2 step to attack** and also **no recovery
from a lost key.** Back it up deliberately.

A store written by an encrypted build is unreadable by a plain SQLite client; the test
`crates/sakur4-core/tests/encryption_at_rest.rs` asserts exactly that by opening one without the key.

**OpenSSL development files are required to build the feature**, which is why the encryption job is a
separate CI job and why it reports `INCOMPLETE` on a machine that lacks them rather than passing
vacuously.

---

## What Sakur4 does *not* do

| | |
|---|---|
| Redact secrets from transcripts | It is a recorder. If a token appeared in tool output, it is in the store |
| Encrypt in transit | stdio is a pipe; HTTP is plain unless you put TLS in front of it |
| Isolate one project's data from another **on the same root** | Project scoping separates projects; it is not a sandbox |
| Verify the model's summaries | That is the *point* of the Ledger — model output cannot write to it — but it is a correctness property, not a security one |
| Reduce what an agent may read | Sakur4 observes; it does not gate |

---

## Supply chain

| Control | State |
|---|---|
| **Known-vulnerability scan** | `cargo audit` runs in CI on every push. Running it by hand the first time found a **medium-severity `rustls` advisory** (RUSTSEC-2026-0285, TLS 1.3 handshake messages accepted across encryption-level boundaries) that had been published ten days earlier — reached through `reqwest` for the reverse proxy |
| **Checksum-verified installs** | `install.sh` fetches `SHA256SUMS.txt` and **refuses to install** an archive that is not listed or does not match. A missing checksum file is a failure, not a silent downgrade |
| **Signed artifacts** | **Not yet.** A checksum proves a download was not corrupted; it does not prove origin. See [Limitations](Limitations) |
| **Licence and duplicate-version policy** | Runs in CI against `deny.toml` — licences, duplicates, sources |icy question the project has not answered |
| **Unused dependencies** | Checked. Three were declared and referenced nowhere, including file-watching crates that implied a feature which did not exist |

---

## Reporting a vulnerability

**Please do not open a public issue.** Report privately through GitHub's
[Security Advisories](https://github.com/sc4rfurry/Sakur4/security/advisories/new), or contact the
maintainers directly if you cannot use that.

Include what you can: affected version, reproduction steps, and the impact you believe it has. You will
get an acknowledgement, and credit in the fix's release notes unless you would rather not be named.

---

## The short version

- It is a local tool for one developer. **Keep it local.**
- **The store is as sensitive as the session.** Back it up accordingly, or encrypt it.
- **Encryption is a build flag, not a setting.** If it matters to you, it matters today.
- **Binding to a non-loopback address removes the only access control there is.**

---

<sub>[← Back to Home](Home) · [All pages](Home#where-to-go-next)</sub>
