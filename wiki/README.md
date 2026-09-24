# Publishing this wiki

The pages in this directory are written as **GitHub Wiki** pages: `_Sidebar.md` becomes the navigation,
`_Footer.md` the footer, and every other `.md` a page whose URL is its filename.

GitHub keeps a wiki in a **separate git repository** (`<repo>.wiki.git`), so publishing is a one-time
setup and then a push.

---

## First time

```bash
# 1. Create one page in the web UI so the wiki repository exists.
#    Repository → Wiki → "Create the first page" → Save. The content does not matter;
#    it will be replaced. GitHub will not serve a wiki repository that has never been initialised.

# 2. Clone it next to the checkout.
git clone https://github.com/sc4rfurry/Sakur4.wiki.git ../Sakur4.wiki

# 3. Copy the pages in.
cp Sakur4/wiki/*.md ../Sakur4.wiki/

# 4. Push.
cd ../Sakur4.wiki
git add -A
git commit -m "docs: the wiki"
git push origin master
```

---

## After that

`Sakur4.wiki` is an ordinary git repository. To update a page, edit it here, copy it across, and push —
or edit `Sakur4.wiki` directly, since the wiki is editable in the browser too.

---

## Verifying it landed

```bash
curl -sI https://github.com/sc4rfurry/Sakur4/wiki | head -1     # expect: 200
```

Then check these resolve, because they are the two that break silently when a filename is wrong:

- `https://github.com/sc4rfurry/Sakur4/wiki` — the Home page
- `https://github.com/sc4rfurry/Sakur4/wiki/Limitations` — a page whose name is easy to typo

Wiki links are **filename-based and case-sensitive on the rendered site**: `[Limitations](Limitations)`
resolves to `Limitations.md`. A link to a page that does not exist renders in red rather than erroring,
so a typo is visible but not loud — worth clicking through the sidebar once after the first push.

---

## Why the pages live here rather than only in the wiki

`wiki/` is tracked in the main repository, so:

- a change to the docs shows up in a pull request and can be reviewed with the code it describes;
- the pages are covered by the `documentation links resolve` check in `verify.mjs`, which is what stops
  a cross-link rotting;
- the wiki cannot drift silently away from the code, because a stale page here is visible in `git log`
  rather than only on the website.

The trade-off is that publishing is a copy rather than an automatic sync. That is deliberate: a wiki
push is a published artifact, and it should be a decision.
