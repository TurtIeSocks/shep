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

**Target: GitHub Pages**, at `https://shep.turtlesocks.dev`. The Pages source
is set to **GitHub Actions** in the repository settings, and the custom domain
is configured there too — so there is no `CNAME` file in `public/`, which is a
branch-deploy mechanism and would only be dead weight here. If the domain ever
resets itself, adding `web/public/CNAME` containing the bare hostname is the
belt-and-braces fix.

`.github/workflows/pages.yml` does the work: `npm ci` and `npm run build` in
`web/`, then `upload-pages-artifact` and `deploy-pages`. Node comes from
`web/.nvmrc` via `node-version-file`, so the pinned version has exactly one
home rather than being restated in the workflow.

Two things about it are deliberate.

**It runs on push, where the Rust CI does not.** `test.yml` is dispatch-only
because one run is 19 jobs, five of them on the macOS and Windows runners that
bill at 10x and 2x on a private repo. This is one ubuntu job of a couple of
minutes at 1x, so that arithmetic does not reach it.

**Its paths filter names three files outside `web/`** — `README.md`,
`docs/specs/deferred.md` and `docs/terminology.md`. The site parses those at
build time rather than restating them (see `src/data/*.ts`), so a claim that
goes stale fails the build instead of shipping. That only holds if editing one
of them actually triggers a deploy, hence the filter. Add to it if a new
`src/data/` file starts reading somewhere new.

The site's internal links are hardcoded root-relative paths — `href="/docs/terminology"`,
`href="/design-language"`, and so on; grep `src/` for `href="/` for the full
set — rather than going through Astro's `base`. At the apex of a custom domain
those resolve exactly as written. This is the one thing that would break if the
site were ever served from a project URL like `user.github.io/shep/`, where
every one of them would 404; moving there would mean routing them all through
`import.meta.env.BASE_URL` first.

### Checking a change before it ships

`npm run build && npm run preview` serves the production build locally, which
is the same output the workflow uploads. Worth doing for anything touching
`src/data/`, since those files fail the build loudly on a mismatch and the
error is much easier to read locally than in a workflow log.
