# Publishing this wiki

The pages here are **GitHub Wiki** pages: `_Sidebar.md` becomes the navigation, `_Footer.md` the footer,
and every other `.md` a page whose URL is its filename.

GitHub keeps a wiki in a **separate git repository** (`<repo>.wiki.git`), so publishing is a clone, a
copy and a push — with **one step that cannot be automated.**

---

## The step that needs a browser

**GitHub creates the wiki repository when the first page is saved in the web UI.** Until then the
`.wiki.git` remote is pull-only and answers `Repository not found`, and *nothing* can bootstrap it:

| Attempt | Result |
|---|---|
| `git push https://github.com/sc4rfurry/Sakur4.wiki.git` | `remote: Repository not found` |
| `gh api repos/…/wiki` | `Not Found` — GitHub has no REST or GraphQL endpoint for wiki pages |

And because an uninitialised wiki **redirects to the repository root** rather than 404ing, an empty wiki
looks like a missing feature rather than an empty one. That is worth knowing before debugging it.

So, once:

1. Open **<https://github.com/sc4rfurry/Sakur4/wiki/_new>**
2. Title `Home`, body anything — it is replaced.
3. **Save.**

That is the whole manual step. Everything after it is one command.

---

## Then

```bash
./wiki/publish.sh
```

Clones the wiki repository, copies every page in, commits, and pushes. It refuses with the instructions
above if the first page has not been created yet, so running it too early tells you what to do rather
than failing obscurely.

```bash
SAKUR4_WIKI_CLONE=/tmp/w ./wiki/publish.sh   # keep the clone somewhere specific
SAKUR4_REPO=owner/name ./wiki/publish.sh     # publish a fork's wiki
```

---

## Verifying it landed

```bash
# The uninitialised wiki redirects to the repo root; an initialised one does not.
curl -s -o /dev/null -w '%{http_code} %{url_effective}\n' -L https://github.com/sc4rfurry/Sakur4/wiki
```

Then click through the sidebar once. **A wiki link to a page that does not exist renders in red rather
than failing**, so a filename typo is visible but not loud — and the sidebar is the first thing a reader
touches.

`docs/verification/wiki-check.mjs` catches most of that before it is published: every link resolves to a
page that exists, every page is reachable from somewhere, every page has a footer, and every
`sakur4d <command>` in a code span names a real subcommand.

---

## Why the pages live here rather than only in the wiki

`wiki/` is tracked in the main repository, so:

- a documentation change appears in a pull request and is reviewed **with the code it describes**;
- the pages are covered by the checks in `verify.mjs`, which is what stops a cross-link rotting;
- the wiki cannot drift silently, because a stale page is visible in `git log` rather than only on the
  website.

The trade-off is that publishing is a copy rather than an automatic sync. That is deliberate: a wiki
push is a published artifact, and it should be a decision.
