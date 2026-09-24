# Verification

> **Nothing on this site is a claim you have to take on faith.** This page says what is checked, how to
> run it yourself, and — as importantly — what the checks do *not* establish.

---

## Run the whole thing

```bash
git clone https://github.com/sc4rfurry/Sakur4 && cd Sakur4
cargo build --release -p sakur4d
node verify.mjs
```

`verify.mjs` is a single script with no dependencies beyond Node and a Rust toolchain. It builds what it
needs, spawns a real daemon, speaks real MCP to it, and reports each check by name.

```bash
node verify.mjs --list                    # what would run, and what each needs
node verify.mjs --quick                   # skip the slow benchmarks
node verify.mjs --only rust,fmt           # a subset, by group or short check name
node verify.mjs --require-all             # fail if anything was skipped
node verify.mjs --check-release           # download the published release and test the installer
node verify.mjs --upstream http://host:8080   # include the live-inference checks
```

**A skip is reported, never silent.** `--require-all` turns any skip into a failure, and
`--allow-absent` excuses only those whose reason names a missing tool — so a machine without Oh My Pi
does not fail the build, but a check that quietly stopped running does.

---

## What the groups need

| Group | Needs | What it covers |
|---|---|---|
| `rust` | nothing | fmt, clippy, the test suite, doctests, `cargo doc`, the documentation guards |
| `encryption` | OpenSSL development files | FR-20's acceptance criterion |
| `hermes` | Python + a built daemon | the ContextEngine, 44 contracts against a live daemon |
| `bench` | a repository to index | the scripted A/B, NFR-2 recall at scale |
| `live` | `--upstream <url>` | measurements against a real llama.cpp server |
| `harness` | a built daemon | OMP and Hermes installation, generated configs, the install path |

---

## The documentation guards

Five checks exist only to stop the docs drifting away from the code. Each was written after a real
drift was found, and between them they have caught their own author more than once.

| Guard | Catches |
|---|---|
| **documented test count matches** | the suite's size stated in three places, drifting from reality |
| **documented tool arguments exist** | an argument named in a table that no tool accepts |
| **documented environment variables exist** | an env var documented but never read |
| **documented catalogue size matches** | "17 tools, 4 resources" when it is no longer 17 or 4 |
| **documented commands exist** | a command in a code block that is not a real subcommand or flag |

Plus four structural checks added later, each of which found something:

- **no unused dependencies** — found three crates declared and referenced nowhere, two of them
  file-watching libraries that implied a feature the project did not have.
- **documentation links resolve** — every relative link in every `.md`, resolved against the file
  containing it. (Written after getting one wrong by hand.)
- **anchors go through the budget check** — FR-4's refusal path stayed wired into all four
  prompt-building paths.
- **workflows are structurally sound** — GitHub accepts a malformed workflow and runs a *subset* of it,
  so this checks step indentation per job and that `cancel-in-progress` is the value each workflow needs.

---

## The checks that do not run by default

### Against a real llama.cpp server

```bash
node verify.mjs --upstream http://your-llama-server:8080
```

Point this at **your own** inference server. It measures prefix reuse, tokenisation, and the compaction
path against a real backend rather than a stub. Results against one such server are recorded in
[`docs/verification/README.md`](https://github.com/sc4rfurry/Sakur4/blob/master/docs/verification/README.md) —
including the capability probe returning **501** for `?action=save` and Sakur4 correctly reporting
`no checkpoint source detected`.

### Against the published release

```bash
node verify.mjs --check-release
```

Downloads the actual release archive and its checksum file from GitHub, verifies the checksum, **proves
a tampered archive is refused**, extracts, and places the binary. It runs after publishing in the release
workflow, so a release that cannot be installed fails its own build.

---

## How the guards are written

Every check in this project is expected to **fail when the thing it guards breaks** — and that is
verified rather than assumed, because a check that cannot fail is worse than no check: it reports a clean
result and gets trusted.

Several were wrong in ways that made them look fine:

| Check | How it was wrong |
|---|---|
| **unused dependencies** | First too permissive (`axum::http::` counted as using the `http` crate), then too strict (`use serde::{…}` counted as unused). Both directions, found by grepping names the report claimed were absent |
| **uncalled functions** | Counted a *struct field* named like the function as a call, so it reported zero while a genuinely uncalled function sat in the tree. Four versions before it worked; three looked plausible |
| **documentation links** | Walked into an untracked second checkout and reported a broken link in a file that is not part of this repository |

The rule that came out of it: **assert the known case before believing the tool.** Each of those now
pins answers it must produce, and fails loudly if it does not.

---

## What the checks do not establish

- **A green run is not a conformance result.** Both transports are exercised against a real SDK client,
  which is weaker than a conformance suite.
- **The benchmarks are narrow.** One repository, one model, matched windows. See [Benchmarks](Benchmarks)
  for which number came from where.
- **A skipped check is not a passed check.** The summary says `INCOMPLETE` for any group that skipped,
  and names every skip and its reason.
- **The live-server checks need a server.** Without one they skip, and the site's claims about
  cache-coherent behaviour rest on the scripted backend and on the recordings in `docs/verification/`.

---

<sub>[← Back to Home](Home) · [All pages](Home#where-to-go-next)</sub>
