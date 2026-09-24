#!/bin/sh
# Install sakur4d from a GitHub release.
#
#   curl -fsSL https://raw.githubusercontent.com/sc4rfurry/Sakur4/main/install.sh | sh
#
# # Why a script and not a package
#
# Sakur4 is one self-contained binary with no runtime dependencies, so a package manager
# would add a maintenance surface without adding anything a download does not. A `cargo
# install` would compile the whole tree — tree-sitter grammars and a bundled SQLite — for a
# tool whose entire point is to be cheap to run.
#
# # What this refuses to do
#
# It verifies the checksum before installing, and it stops rather than continuing if the
# checksum cannot be fetched. An installer that silently skips verification is worse than no
# installer, because it teaches people to trust the output of a pipe.
#
# Environment:
#   SAKUR4_VERSION   tag to install, e.g. v0.1.0. Default: the latest release.
#   SAKUR4_BIN_DIR   where to put the binary. Default: ~/.local/bin if it exists or can be
#                    created, otherwise /usr/local/bin when that is writable.

set -eu

# The repository, not an environment default.
#
# This read `sakur4/sakur4` — an organisation nobody owns — while the header four lines above
# named the correct URL. Every install path went to a 404: the latest-release lookup, the archive,
# and the checksum file. It survived a verified release because the guard for placeholder URLs no
# longer listed that owner, and the substitution that fixed the documented URL did not reach here,
# since this string carries no URL prefix.
REPO="${SAKUR4_REPO:-sc4rfurry/Sakur4}"
BIN="sakur4d"

say() { printf '%s\n' "$*"; }
die() { printf 'install.sh: %s\n' "$*" >&2; exit 1; }

need() {
    command -v "$1" >/dev/null 2>&1 || die "$1 is required but not installed"
}

need uname
need mkdir
need chmod
need mv

# A downloader and a checksum tool, either of which is present nearly everywhere. Both are
# required; a partial set would mean installing without verifying.
if command -v curl >/dev/null 2>&1; then
    fetch() { curl -fsSL "$1"; }
    fetch_to() { curl -fsSL -o "$2" "$1"; }
elif command -v wget >/dev/null 2>&1; then
    fetch() { wget -qO- "$1"; }
    fetch_to() { wget -qO "$2" "$1"; }
else
    die "curl or wget is required"
fi

if command -v sha256sum >/dev/null 2>&1; then
    checksum() { sha256sum "$1" | awk '{print $1}'; }
elif command -v shasum >/dev/null 2>&1; then
    checksum() { shasum -a 256 "$1" | awk '{print $1}'; }
else
    die "sha256sum or shasum is required; this installer verifies before it installs"
fi

# ---------------------------------------------------------------------------
# Which target
# ---------------------------------------------------------------------------
os="$(uname -s)"
arch="$(uname -m)"

case "$os" in
    Linux)  os_part="unknown-linux-gnu" ;;
    Darwin) os_part="apple-darwin" ;;
    *) die "unsupported OS: $os. Download from https://github.com/$REPO/releases" ;;
esac

case "$arch" in
    x86_64|amd64)  arch_part="x86_64" ;;
    arm64|aarch64) arch_part="aarch64" ;;
    *) die "unsupported architecture: $arch. Download from https://github.com/$REPO/releases" ;;
esac

target="${arch_part}-${os_part}"

# ---------------------------------------------------------------------------
# Which version
# ---------------------------------------------------------------------------
version="${SAKUR4_VERSION:-}"
if [ -z "$version" ]; then
    # The API, not the `releases/latest` redirect. The redirect is cheaper and is not
    # rate-limited, but resolving it needs `curl -L` and a header parse, and this script
    # already has a `fetch` helper that follows redirects -- so the API call is the one that
    # keeps the failure mode the same for a user as for CI.
    version="$(fetch "https://api.github.com/repos/$REPO/releases/latest" \
        | sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' \
        | head -n 1)"
    [ -n "$version" ] || die "could not determine the latest release; set SAKUR4_VERSION"
fi

archive="${BIN}-${version}-${target}.tar.gz"
base="https://github.com/$REPO/releases/download/$version"

say "sakur4 installer"
say "  version  $version"
say "  target   $target"

# ---------------------------------------------------------------------------
# Download, verify, extract
# ---------------------------------------------------------------------------
tmp="$(mktemp -d 2>/dev/null || mktemp -d -t sakur4)"
trap 'rm -rf "$tmp"' EXIT INT TERM

say "  fetching $archive"
fetch_to "$base/$archive" "$tmp/$archive" || die "download failed: $base/$archive"

# The checksum file is required. Fetching it separately means a failure to reach it is a
# failure to install, rather than a quiet downgrade to installing something unverified.
fetch_to "$base/SHA256SUMS.txt" "$tmp/SHA256SUMS.txt" \
    || die "could not fetch SHA256SUMS.txt; refusing to install without verification"

# # Accept both checksum-file formats
#
# `sha256sum ./*.tar.gz` writes `hash  *./name`, while bare names give `hash  name`. Both
# appear in the wild — the first is what a shell glob produces and the second is what the
# release workflow writes now — so the lookup normalises the filename before comparing rather
# than assuming one form.
#
# This was found by testing the installer against a real archive rather than by reading it: the
# first version matched only `name` and every install would have been refused with "not listed
# in SHA256SUMS.txt", which reads as a corrupt download rather than as an installer bug.
expected="$(awk -v f="$archive" '
    {
        name = $2
        sub(/^\*/, "", name)   # binary-mode marker
        sub(/^\.\//, "", name) # leading ./ from a glob
        if (name == f) { print $1; exit }
    }
' "$tmp/SHA256SUMS.txt")"
[ -n "$expected" ] || die "$archive is not listed in SHA256SUMS.txt; refusing to install"

actual="$(checksum "$tmp/$archive")"
if [ "$expected" != "$actual" ]; then
    die "checksum mismatch for $archive
  expected $expected
  actual   $actual
Do not use this download. Report it at https://github.com/$REPO/issues"
fi
say "  checksum ok"

tar -xzf "$tmp/$archive" -C "$tmp" || die "could not extract $archive"

# ---------------------------------------------------------------------------
# Install
# ---------------------------------------------------------------------------
#
# # Prefer a directory the shell will actually find
#
# The header documents the default as "~/.local/bin, or /usr/local/bin when writable and
# ~/.local/bin is not on PATH". It was neither of those: the code created ~/.local/bin and used it
# whenever `mkdir` succeeded, never consulting PATH at all.
#
# That matters on the distributions where ~/.local/bin is not on the default PATH — Debian and
# Ubuntu before `~/.profile` has been sourced, and most containers. The install then "succeeds" and
# `sakur4d` is not on the path, which reads as a broken install rather than as a directory choice.
#
# The order is now: whatever the user asked for, then ~/.local/bin if it is already on PATH, then
# /usr/local/bin if it can be written, then ~/.local/bin regardless. The last fallback is
# deliberate — somewhere is better than nowhere — but the message below says whether the directory
# is on PATH, so the user is not left guessing.
bindir="${SAKUR4_BIN_DIR:-}"
if [ -z "$bindir" ]; then
    on_path() {
        case ":${PATH:-}:" in
            *":$1:"*) return 0 ;;
            *) return 1 ;;
        esac
    }
    if on_path "$HOME/.local/bin"; then
        bindir="$HOME/.local/bin"
    elif [ -w /usr/local/bin ] 2>/dev/null || mkdir -p /usr/local/bin 2>/dev/null; then
        bindir="/usr/local/bin"
    else
        bindir="$HOME/.local/bin"
    fi
fi
mkdir -p "$bindir" 2>/dev/null || die "cannot create $bindir; set SAKUR4_BIN_DIR"

# `mv` across filesystems fails, so copy into place and then make it executable. A
# half-written binary is worse than none, so the copy goes to a temporary name first.
mv "$tmp/$BIN-$version-$target/$BIN" "$bindir/$BIN.new"
chmod +x "$bindir/$BIN.new"
mv "$bindir/$BIN.new" "$bindir/$BIN"

say "  installed $bindir/$BIN"

# ---------------------------------------------------------------------------
# What is next, and whether this will actually be found
# ---------------------------------------------------------------------------
say ""
"$bindir/$BIN" --version >/dev/null 2>&1 || die "the installed binary does not run"

case ":${PATH}:" in
    *":$bindir:"*) ;;
    *)
        say "Add it to your PATH:"
        say ""
        say "  export PATH=\"$bindir:\$PATH\""
        say ""
        ;;
esac

# ---------------------------------------------------------------------------
# The skill, which used to be thrown away
# ---------------------------------------------------------------------------
#
# # The instruction this replaces could not be followed
#
# The old code printed:
#
#     cp -r "$tmp/$BIN-$version-$target/skills/sakur4" ~/.agents/skills/
#
# and `$tmp` is a directory this script deletes on exit — `trap 'rm -rf "$tmp"' EXIT`. So the one
# step it told the user to run referenced a path that no longer existed, and the only file that
# survived the install was the binary. The archives ship the skill and both integrations precisely
# so a download is a complete install; everything but the binary was being discarded.
#
# It installs the skill now. `SAKUR4_SKILL_DIR` overrides the destination, and the copy is skipped
# when the destination already holds a `sakur4` skill unless `SAKUR4_FORCE` is set — replacing a
# skill a user has edited is worse than telling them it is already there.
release_root="$tmp/$BIN-$version-$target"
skill_dst="${SAKUR4_SKILL_DIR:-$HOME/.agents/skills}"
if [ -d "$release_root/skills/sakur4" ]; then
    if [ -e "$skill_dst/sakur4" ] && [ -z "${SAKUR4_FORCE:-}" ]; then
        say "  skill      already at $skill_dst/sakur4 (set SAKUR4_FORCE=1 to replace)"
    else
        mkdir -p "$skill_dst" 2>/dev/null || true
        if rm -rf "$skill_dst/sakur4" 2>/dev/null && \
           cp -r "$release_root/skills/sakur4" "$skill_dst/" 2>/dev/null; then
            say "  skill      installed to $skill_dst/sakur4"
        else
            say "  skill      could not be written to $skill_dst; set SAKUR4_SKILL_DIR"
        fi
    fi
fi

# The integrations ship in the archive too. They are not installed, because where a harness keeps
# its plugins differs per harness and guessing wrong is worse than saying where to look — but the
# first version of this note named a path inside `$tmp`, which is deleted on exit, so it repeated
# the exact defect the block above exists to fix. A directory this script removes is not somewhere
# to send a reader.
if [ -d "$release_root/integrations" ]; then
    say "  plugins    the archive also carries the OMP and Hermes integrations; this script does"
    say "             not place them, because each harness keeps plugins elsewhere. They are in"
    say "             the same release archive you just installed from:"
    say "             https://github.com/sc4rfurry/Sakur4/releases/tag/$version"
fi

say ""
say "Contact the daemon:"
say ""
say "  $BIN doctor"
say "  $BIN config hermes"
say ""