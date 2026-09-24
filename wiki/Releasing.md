# Releasing

> How a version of Sakur4 gets from a green tree to a download. Mostly automated, with two steps that
> are deliberately manual.

---

## The shape of a release

```text
  bump versions  →  tag v0.1.0  →  GitHub Actions
                                      ├─ verify (fmt, clippy, tests, docs)
                                      ├─ build 5 platform archives
                                      ├─ checksum them
                                      ├─ publish the GitHub release
                                      └─ download the release back and install it
```

That last step is the point: **the release workflow proves the release it just published is
installable** — fetching the archive and its checksum file from the tag, refusing a tampered copy,
extracting, and placing the binary. A release that cannot be installed fails its own build.

---

## Version policy

While the major version is `0`:

| Change | Bump |
|---|---|
| **The MCP tool surface** — tool names, argument shapes, result shapes, resource URIs, the prompt name | **minor**, plus a migration note in `CHANGELOG.md` |
| Anything else | patch |

**The MCP tool surface is the stable contract**, because that is what harnesses depend on. The Rust APIs
are published so the daemon has a home on crates.io, **not as a stability promise**.

`rust-version` is `1.94`, and CI builds against exactly that — so the promise in `Cargo.toml` is
verified rather than assumed.

---

## Cutting a release

### 1 · Make sure the tree is green

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features   # CI uses -D warnings
cargo test --workspace --all-targets
node verify.mjs --require-all
```

### 2 · Update the changelog

Move `[Unreleased]` entries under the new version, with the date. **Say what was wrong, not just what
changed** — the changelog is read by people deciding whether to upgrade.

### 3 · Bump the version

In the workspace `Cargo.toml`, then `cargo check` to refresh `Cargo.lock`.

### 4 · Commit and tag

```bash
git commit -am "release: v0.1.0"
git tag -a v0.1.0 -m "v0.1.0"
git push origin master --follow-tags
```

The tag triggers the release workflow.

### 5 · Publish to crates.io — manual, and in order

**This is one of the two manual steps, and the order is not optional:** `sakur4d` depends on
`sakur4-core` by version, so the library must be published **and indexed** before the daemon.

```bash
cargo publish -p sakur4-core
# wait for the registry to index it — publishing sakur4d too early fails to resolve the dependency
cargo publish -p sakur4d
```

---

## What ships in an archive

Each platform archive contains:

| | |
|---|---|
| `sakur4d` (or `sakur4d.exe`) | **Executable**, `-rwxr-xr-x` in the archive — verified by the install-path check |
| `skills/sakur4/` | The Agent Skill, `SKILL.md` and its scripts |
| `integrations/omp-plugin/` | The Oh My Pi extension |
| `integrations/hermes-plugin/` | The Hermes ContextEngine |
| `LICENSE`, `README.md`, `CHANGELOG.md` | |

`install.sh` installs the binary **and the skill**; the plugins are unpacked alongside because each
harness keeps plugins somewhere different.

---

## If a release goes wrong

| Artifact | What you can do |
|---|---|
| The **GitHub release** | Delete and recreate it from the same tag, or move the tag and re-run the workflow |
| A **crates.io version** | **Cannot be fixed that way.** Yank it and publish a patch that supersedes it |
| A **bad binary** | Re-run the workflow for the tag; the archives are rebuilt and re-checksummed |

---

## Workflow concurrency, and why the two differ

| Workflow | `cancel-in-progress` | Why |
|---|---|---|
| `ci.yml` | **true** | A stale test result is worthless, so a superseded run is cancelled |
| `release.yml` | **false** | Two runs racing to create the same release, or a cancel midway through publishing, can leave an archive on the download page with **no checksum beside it** — a state a user cannot detect |

Both values are checked by `docs/verification/workflow-shape.mjs`, which also asserts that every step
entry in every job sits at the same indent. GitHub accepts a malformed workflow and runs a *subset* of
it, so a step under the wrong key is not a syntax error — it is a step that silently never runs.

---

## What is not automated yet

- **`cargo deny`** — licences and duplicate versions. A policy question the project has not answered.
  (`cargo audit` **does** run in CI, and found a medium-severity `rustls` advisory on its first hand-run.)
- **A signed release.** Archives are checksummed but not GPG- or cosign-signed, so a checksum proves a
  download was not corrupted rather than proving origin.
- **Homebrew, Scoop, or any package-manager formula.**
- **Publishing to crates.io**, which is manual by necessity — the token is not in CI.

---

<sub>[← Back to Home](Home) · [All pages](Home#where-to-go-next)</sub>
