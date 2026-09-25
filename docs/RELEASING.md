# Releasing Sakur4

A release is a tag. Everything else is automated, except the two steps that are
irreversible and therefore belong to a human.

## One-time setup

**The repository URL is real everywhere it appears**, and verified on every run:

```toml
repository = "https://github.com/sc4rfurry/Sakur4"
```

That covers `[workspace.package]` in `Cargo.toml`, the badge and links in `README.md`,
`install.sh`, the plugin and skill manifests, and the compare/release link definitions at the
bottom of `CHANGELOG.md`. `verify.mjs` fails if a placeholder returns, because the failure it
guards against is a published crate whose repository link 404s — a crate nobody can inspect.

CI uses no secrets and the release workflow uses the automatic `GITHUB_TOKEN`, so nothing else
needs configuring.

### The first push

The one step that cannot happen from inside the repository is getting it onto GitHub:

```bash
gh auth login                     # interactive, once
git remote add origin https://github.com/sc4rfurry/Sakur4.git
git push -u origin master --follow-tags
```

`v0.1.0` is already tagged locally, so `--follow-tags` starts the release workflow and produces
the archives. Confirm with:

```bash
git ls-remote --tags origin | grep v0.1.0
gh run list --limit 3
```

## Cutting a release

### 1. Make sure the tree is green

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features
cargo test --workspace --all-targets
cargo doc --workspace --no-deps
```

CI enforces all four with warnings denied, so a warning is a failure there.

**The second one needs OpenSSL development files.** `--all-features` enables the `encryption`
feature, whose build script links SQLCipher, and without a crypto provider it dies with:

```text
Missing environment variable OPENSSL_DIR or OPENSSL_DIR is not set
```

That message points at the linter and the failure is not a lint failure. On Debian or Ubuntu:

```bash
sudo apt-get install -y libssl-dev
```

**On Windows it cannot be made to work** without installing OpenSSL development files and
setting `OPENSSL_DIR`; the runtime package is not enough. So on a Windows machine, run the other
three and lint with the default features:

```bash
cargo clippy --workspace --all-targets
```

That is a real gap in local verification rather than a failure — the feature is off by default
and CI lints it on Linux, where the provider is present. Worth knowing before you spend an hour
on a C build script.

Every other step below runs anywhere.

### 2. Update the changelog

Move everything under `## [Unreleased]` into a new `## [x.y.z] - YYYY-MM-DD`
section, and update the two link definitions at the bottom of the file.

The release workflow extracts the section matching the tag to use as the GitHub
release body, so this file *is* the release notes. If no matching section exists it
falls back to GitHub's generated notes, which is a worse outcome and prints a
warning in the job log.

### 3. Bump the version

The version lives in one place, `[workspace.package] version` in the root
`Cargo.toml`. All three crates inherit it, and the two path dependencies in
`[workspace.dependencies]` carry the same version — bump those together or
`cargo package` will fail to resolve.

In practice those are the three in `Cargo.toml`, so the bump starts as one substitution:

```bash
sed -i 's/0\.2\.0/0.2.1/g' Cargo.toml     # or your editor's replace-all
cargo build --workspace                  # refresh Cargo.lock
```

**And then three more files, which the version number does not lead you to.** The `0.2.1` release needed:

| File | What it holds |
|---|---|
| `integrations/omp-plugin/package.json` | the plugin's own version |
| `integrations/omp-plugin/install.mjs` | the same version, twice — a dependency string and a written `package.json` |
| `docs/verification/install-path.sh` | the tag the install check downloads by default |

So the bump is not finished when `cargo build` succeeds. **Confirm nothing was missed with the step that
does it rather than a checklist that drifts:**

```bash
# Every version-shaped string outside the changelog and the design notes. Anything here that is not
# the new version is either a deliberate historical reference or a miss.
grep -rn '0\.2\.1' --include='*.md' --include='*.json' --include='*.mjs' --include='*.sh' --include='*.ps1' \
  . | grep -v node_modules | grep -v '^\./target' | grep -v CHANGELOG | grep -v DESIGN
```

**The failure mode of a half-done bump is caught, which is worth knowing before you worry about it.**
Bumping the workspace version while leaving the path dependencies behind fails immediately and legibly:

```text
error: failed to select a version for the requirement `sakur4-core = "^0.2.1"`
```

So a mistyped bump cannot reach a release; it stops at `cargo build`. After the bump the built
binary reports the new version, which is the cheapest confirmation:

```console
$ ./target/debug/sakur4d --version
sakur4d 0.2.1
```

### 4. Commit and tag

```bash
git add -A
git commit -m "release: v0.2.1"
git tag -a v0.2.1 -m "Sakur4 v0.2.1"
git push -u origin master --follow-tags
```

The tag triggers `.github/workflows/release.yml`, which:

1. Re-runs the full suite against the tagged commit. A tag can point at anything;
   the release must not.
2. Builds prebuilt binaries for `x86_64`/`aarch64` Linux, `x86_64`/`aarch64`
   macOS, and `x86_64` Windows, each with a SHA-256 checksum.
3. Publishes the GitHub release with those artifacts and the changelog section.

To rehearse without publishing, run the workflow manually
(`workflow_dispatch`) with `dry_run` left at its default of `true`. It builds and
verifies; it does not create a release.

### 5. Publish to crates.io — manual, and in order

This step is deliberately not automated. A published version cannot be
unpublished in any meaningful sense: the name stays taken, and a yanked version
stays yanked. That is a decision for a person.

**Order matters.** `sakur4d` depends on `sakur4-core` by version, so
`sakur4-core` must exist on crates.io before `sakur4d` can be packaged at all.
This is also why CI cannot run `cargo package --workspace`: it would fail on a
first release with a confusing "no matching package named `sakur4-core`" error.

```bash
# 1. The library first, and wait for it to be indexed.
cargo publish -p sakur4-core

# 2. The daemon, once the registry has 0.2.1 available.
cargo publish -p sakur4d
```

`sakur4-testkit` is `publish = false` on purpose: it exists so the other two can be
tested, and nothing outside this workspace has a reason to depend on it.

If the registry has not caught up, `cargo publish -p sakur4d` fails with a
dependency-resolution error rather than anything about the code. Waiting a minute
and retrying is the fix.

**What CI can and cannot check here.** Only `sakur4-core` is dry-run published, because it is
the only crate verifiable before a first release. `cargo package --list` succeeds for `sakur4d`
regardless — it does not resolve dependencies at all — so it cannot stand in for "publishable",
and the CI job states the distinction rather than implying all three are checked.

**Most users never take this path.** Releases ship as prebuilt archives and `install.sh`
installs them. The registry exists so `cargo install sakur4d` works for people who prefer it.
To install from a checkout with no registry involved at all:

```bash
cargo install --path crates/sakur4d
```

Verified end to end: builds in about eight minutes and installs a working binary.

## Version policy

While the major version is `0`, the MCP tool surface — tool names, argument shapes,
result shapes, resource URIs, the prompt name — is the stable contract, and a
breaking change to it requires a minor bump plus a migration note. The Rust APIs
are published so the daemon has a home rather than as a stability promise.

`rust-version` is `1.94` and CI builds against exactly that, so the promise in
`Cargo.toml` is verified rather than assumed.

## If a release goes wrong

The GitHub release can be deleted and recreated from the same tag, or the tag can
be moved and the workflow re-run. A crates.io version cannot be fixed that way:
yank it, and publish a patch that supersedes it.

## What is not automated yet

- `cargo deny` now runs in CI against `deny.toml`. It was listed here as a policy question this project has not answered.
  **`cargo audit` now runs in the lint job**: it found a medium-severity `rustls` advisory on its first
  hand-run, ten days after that advisory was published. Advisory checking is covered; licence policy is
  open.
- Homebrew, Scoop, or any package-manager formula.
- A signed release. The artifacts are checksummed but not GPG- or cosign-signed.
