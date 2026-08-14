# shep — marketing site + docs

Astro, static output, plain CSS (no Tailwind — the design is token-driven
and hand-tuned). Source of truth for the design: `../docs/shep-design/`.

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
- `src/layouts/Base.astro` — fonts, the no-flash theme script
  (`localStorage['shep-theme']`, falls back to `prefers-color-scheme`).
- `src/pages/index.astro` — a scaffold-verification page, not the landing
  page. The real landing page is a separate milestone.
