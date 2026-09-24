#!/usr/bin/env bash
# Publish `wiki/` to the GitHub wiki.
#
# # Why this cannot be automatic
#
# GitHub keeps a wiki in a **separate git repository** (`<repo>.wiki.git`) and creates it only when the
# first page is saved **in the web UI**. Pushing cannot bootstrap one:
#
#     $ git push https://github.com/sc4rfurry/Sakur4.wiki.git master
#     remote: Repository not found.
#
# and until it exists, `https://github.com/<owner>/<repo>/wiki` does not 404 — it **redirects to the
# repository root**, which is why an empty wiki looks like a missing feature rather than an empty one.
#
# So this script does everything except that one click, and says so when the click has not happened.
#
# Usage:
#   ./wiki/publish.sh                 # publish
#   GITHUB_TOKEN=… ./wiki/publish.sh  # publish without an interactive prompt
set -eu

REPO="${SAKUR4_REPO:-sc4rfurry/Sakur4}"
HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
CLONE="${SAKUR4_WIKI_CLONE:-$(mktemp -d)/wiki}"

die() { printf '\npublish: %s\n' "$*" >&2; exit 1; }
say() { printf '%s\n' "$*"; }

[ -d "$HERE" ] || die "no wiki directory beside this script"
[ -f "$HERE/Home.md" ] || die "wiki/Home.md is missing — that is the page GitHub requires"

# --- the one thing a script cannot do ------------------------------------------------------------
if ! git ls-remote "https://github.com/$REPO.wiki.git" >/dev/null 2>&1; then
    cat >&2 <<EOF

publish: the wiki repository does not exist yet.

  GitHub creates it when the **first page is saved in the web UI**. No push can do it — the
  \`.wiki.git\` remote is pull-only until then, and \`https://github.com/$REPO/wiki\`
  redirects to the repository root rather than 404ing, so an uninitialised wiki looks like a
  missing feature.

  Do this once, in a browser:

      1.  Open  https://github.com/$REPO/wiki/_new
      2.  Title:  Home       Body:  anything — it is replaced below.
      3.  Save.

  Then run this script again. It takes about thirty seconds.

EOF
    exit 1
fi

say "publish: cloning the wiki repository"
git clone --quiet "https://github.com/$REPO.wiki.git" "$CLONE" \
    || die "could not clone the wiki; if it is private, set GITHUB_TOKEN"

say "publish: copying $(find "$HERE" -maxdepth 1 -name '*.md' | wc -l | tr -d ' ') page(s)"
cp "$HERE"/*.md "$CLONE/"

cd "$CLONE"
git add -A
if git diff --cached --quiet; then
    say "publish: nothing changed — the wiki already matches wiki/"
    exit 0
fi

git -c user.name="${GIT_AUTHOR_NAME:-Sakur4}" \
    -c user.email="${GIT_AUTHOR_EMAIL:-sakur4@localhost}" \
    commit --quiet -m "docs: the wiki

Published from wiki/ in the main repository, which is where the pages are tracked and reviewed
alongside the code they describe. See wiki/README.md."

say "publish: pushing"
git push --quiet origin HEAD \
    || die "push failed; if it is private, set GITHUB_TOKEN and retry"

say ""
say "publish: done. Verify at https://github.com/$REPO/wiki"
say "  the sidebar and footer come from _Sidebar.md and _Footer.md"
say "  a link to a page that does not exist renders red rather than failing — click through once"
