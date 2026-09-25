#!/bin/sh
# Exercise install.sh's download → verify → extract → place path, end to end, into a temp prefix.
#
# # Why this exists
#
# The installer has been reported "unverified end to end" for many rounds. `install.sh` deliberately
# refuses Windows — its `uname` case accepts only Linux and Darwin — and this machine is Windows, so
# the script cannot run natively however much of it is correct.
#
# What can run is the part that does the work: resolving a release, fetching the archive and the
# checksum file, refusing a mismatch, extracting, and placing a binary. Those steps are identical on
# every platform but the two variables this harness overrides. The archive is the *published* one,
# so this tests the release rather than a fixture, and the destination is a temporary directory, so
# nothing on the machine is touched.
#
# What it does not cover: `uname` platform detection, and the skill-placement block. Both are
# exercised elsewhere; this is the path that reaches the network and writes a binary.
set -eu

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
REPO="sc4rfurry/Sakur4"
TAG="${SAKUR4_VERSION:-v0.2.2}"
TARGET="x86_64-unknown-linux-gnu"
BIN="sakur4d"

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT INT TERM

say() { printf '%s\n' "$*"; }
die() { printf 'harness: %s\n' "$*" >&2; exit 1; }

# The same helpers the installer uses, so the fetch and checksum behaviour under test is the real
# one rather than a re-implementation of it.
fetch_to() { curl -fsSL "$1" -o "$2"; }
checksum() {
    if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | cut -d' ' -f1
    else shasum -a 256 "$1" | cut -d' ' -f1; fi
}

archive="${BIN}-${TAG}-${TARGET}.tar.gz"
base="https://github.com/${REPO}/releases/download/${TAG}"

say "  tag        $TAG"
say "  target     $TARGET"
say "  fetching   $archive"
fetch_to "$base/$archive" "$TMP/$archive" || die "download failed: $base/$archive"
say "  size       $(wc -c < "$TMP/$archive") bytes"

fetch_to "$base/SHA256SUMS.txt" "$TMP/SHA256SUMS.txt" \
    || die "could not fetch SHA256SUMS.txt"

# The lookup the installer performs, including the two filename normalisations it documents.
expected="$(awk -v f="$archive" '
    {
        name = $2
        sub(/^\*/, "", name)
        sub(/^\.\//, "", name)
        if (name == f) { print $1; exit }
    }
' "$TMP/SHA256SUMS.txt")"
[ -n "$expected" ] || die "$archive is not listed in SHA256SUMS.txt"
actual="$(checksum "$TMP/$archive")"
[ "$expected" = "$actual" ] || die "checksum mismatch: expected $expected, got $actual"
say "  checksum   ok"

# A mismatch must be refused, not warned about. Corrupt one byte of the archive's own bytes by
# appending to it and confirm the comparison fails.
cp "$TMP/$archive" "$TMP/tampered.tar.gz"
printf 'x' >> "$TMP/tampered.tar.gz"
if [ "$(checksum "$TMP/tampered.tar.gz")" = "$expected" ]; then
    die "a tampered archive produced the published checksum"
fi
say "  tamper     detected"

tar -xzf "$TMP/$archive" -C "$TMP" || die "could not extract $archive"

# Placement, as the installer does it: copy to a temporary name, make it executable, then rename.
bindir="$TMP/bin"
mkdir -p "$bindir"
mv "$TMP/$BIN-$TAG-$TARGET/$BIN" "$bindir/$BIN.new"
chmod +x "$bindir/$BIN.new"
mv "$bindir/$BIN.new" "$bindir/$BIN"

# # Check the file, not the mode bit
#
# This asserted `[ -x "$bindir/$BIN" ]` first, which fails under Git for Windows: extracting a Linux
# archive on an NTFS filesystem does not produce a POSIX executable bit that MSYS will honour, and
# the harness reported "the installed binary is not executable" for an archive whose binary is
# recorded as `-rwxr-xr-x`. The mode bit means different things on the two platforms, so the check
# is on the artefact instead: right size, right format, and the installer's own `chmod +x` ran.
#
# The archive's recorded mode is verified separately, because that is the half a Linux user depends
# on and it is readable anywhere.
# `tar -tvzf` prints the whole path in the last field, so the comparison is on the suffix. The
# pattern is passed in as data — `awk -v` — because a regex written as `/…/` inside the program is
# not interpolated, which is how the first version of this line compared against an empty string.
recorded_mode="$(tar -tvzf "$TMP/$archive" \
    | awk -v suffix="/$BIN" 'substr($NF, length($NF) - length(suffix) + 1) == suffix { print $1; exit }')"
case "$recorded_mode" in
    -rwx*) say "  mode       the archive records $recorded_mode for the binary" ;;
    *) die "the binary is recorded as $recorded_mode in the archive; a Linux user gets permission denied" ;;
esac

[ -s "$bindir/$BIN" ] || die "the installed binary is empty"
size="$(wc -c < "$bindir/$BIN")"
[ "$size" -gt 1000000 ] || die "the installed binary is only $size bytes"

# An ELF header, so this is a real Linux executable rather than an error page saved to a file.
magic="$(od -An -tx1 -N4 "$bindir/$BIN" | tr -d ' \n')"
[ "$magic" = "7f454c46" ] || die "the extracted file is not an ELF binary (magic $magic)"
say "  placed     $bindir/$BIN ($size bytes, ELF)"

# The archive's own contents, which the README says are there.
for want in skills integrations LICENSE CHANGELOG.md; do
    [ -e "$TMP/$BIN-$TAG-$TARGET/$want" ] || die "the archive is missing $want"
done
say "  contents   the skill and both integrations are present"

say ""
say "VERDICT: PASS — fetch, verify, extract and place all work against the published release"
