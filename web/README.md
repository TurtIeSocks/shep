# shep — marketing site + docs

Astro, static output, plain CSS (no Tailwind — the design is token-driven
and hand-tuned). Source of truth for the design: `../docs/shep-design/`.
Node version is pinned in `.nvmrc` (`nvm use` picks it up automatically).

## Commands

| Command             | Action                                      |
| :------------------- | :------------------------------------------ |
| `npm install`         | Install dependencies                        |
| `npm run dev`          | Local dev server at `localhost:4321`        |
| `npm run build`        | Build to `./dist/`                          |
| `npm run preview`      | Preview the production build locally        |
| `npx astro check`      | Typecheck `.astro` files                    |

## Structure

- `src/styles/tokens.css` — color tokens (light + dark), plus non-token
  illustration literals. `--barn` is scenery-only; `--bark` is
  errored/refused/destructive and nothing else — see the comments there
  before reaching for either.
- `src/styles/motion.css` — the `bob`/`chew`/`wag`/`twinkle` keyframes and
  the `.press` hover-state utility, both gated behind
  `prefers-reduced-motion`/applied unconditionally per the handoff.
- `src/components/marks/` — the three SVG marks (sheep, dog, barn), path
  data ported verbatim from the design files.
- `src/components/landing/` — the scrolling landing page (`src/pages/index.astro`).
- `src/components/docs/` + `src/pages/docs/` — the docs shell and its
  eleven pages (two written, eight honest stubs marked `soon`).
- `src/components/design-language/` — the house-style reference page
  (`src/pages/design-language.astro`).
- `src/layouts/Base.astro` — fonts, the no-flash theme script
  (`localStorage['shep-theme']`, falls back to `prefers-color-scheme`).
- `src/data/*.ts` — everything on the site that could go stale (the
  lexicon table, the "not built yet" chalkboard, terminology) is read from
  `../docs/` or `../README.md` at build time rather than typed inline, so a
  build fails loudly instead of shipping a false claim. See each file's own
  header comment for which doc it tracks.

## Deploy

**Target: Cloudflare Pages**, connected directly to this GitHub repo. Two
reasons drove that over the other static hosts:

1. **The repo is private**, and Cloudflare Pages builds from a private repo
   for free via its own GitHub App — it needs neither a public repo nor a
   paid GitHub plan, unlike GitHub Pages (whose Pages feature is a paid-plan
   feature on a private repo, and which would also run its build through
   GitHub Actions — the same billing surface `docs/specs/deferred.md`
   documents keeping on manual dispatch only, to avoid metered minutes on a
   private repo).
2. **The site's internal links are hardcoded root-relative paths**
   (`href="/docs/terminology"`, `href="/design-language"`, etc. — grep
   `src/` for `href="/` to see the full set), not run through Astro's `base`
   config. Cloudflare Pages serves a project at the root of its own
   subdomain (`<project>.pages.dev`), so those links resolve correctly with
   zero changes. A GitHub Pages *project* page, by contrast, serves from
   `/shep/` — every one of those hardcoded links would 404 there unless
   they were all rewritten to go through `import.meta.env.BASE_URL`, which
   is real surface area this pass didn't take on.

Nothing in this repo needs to change for that to work — Cloudflare Pages'
git integration takes the build command and output directory as dashboard
settings, not a config file. These are the exact values to enter, and the
steps to connect it for the first time.

### First-time setup (dashboard, no CLI)

1. Go to [dash.cloudflare.com](https://dash.cloudflare.com) → **Workers &
   Pages** → **Create** → **Pages** → **Connect to Git**. Sign in (or create
   a free account) and authorize the Cloudflare Pages GitHub App.
2. Grant it access to `TurtIeSocks/shep` specifically (or all repos, if
   preferred) — the private repo will show up in the picker once the app
   has access. Select it.
3. On the "Set up builds" screen, since the site lives in `web/` rather
   than the repo root:
   - **Root directory:** `web`
   - **Framework preset:** Astro (Cloudflare autodetects this from
     `package.json`; if it doesn't, set it manually)
   - **Build command:** `npm run build`
   - **Build output directory:** `dist`
   - **Environment variable:** `NODE_VERSION` = `22.12.0` (matches
     `web/.nvmrc` — Cloudflare doesn't read `.nvmrc` on its own, so this is
     the one setting that needs typing in by hand)
4. Click **Save and Deploy**. First build takes a couple of minutes; watch
   it stream in the dashboard.
5. Once it's green, the site is live at the `*.pages.dev` URL Cloudflare
   assigns (shown on the project's dashboard page). A custom domain can be
   attached later from the project's **Custom domains** tab — no rebuild
   needed, just DNS.

### After that

Every future push to `main` redeploys automatically — that's the point of
the git integration, and it's the one piece of this pipeline that isn't
manual-dispatch-gated, because it costs Cloudflare's build minutes, not
GitHub Actions minutes, so the private-repo billing concern that keeps the
Rust CI on manual dispatch doesn't apply here. Pull requests get their own
preview deployment URL automatically too, useful for reviewing a docs or
design change before it lands on the real domain.

To redeploy without a new commit (e.g. after changing an environment
variable), use **Retry deployment** on the project's **Deployments** tab —
still no CLI needed.
