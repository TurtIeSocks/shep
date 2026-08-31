// @ts-check
import { defineConfig } from 'astro/config';

// https://astro.build/config
export default defineConfig({
  // The site is served at the apex of its own custom domain, so there is no
  // `base` — every internal link in `src/` is a hardcoded root-relative path
  // (`href="/docs/terminology"`), and at a domain root those resolve as
  // written. A GitHub Pages *project* URL would serve from `/shep/` instead
  // and 404 every one of them; the custom domain is what makes them correct.
  site: 'https://shep-pm.com',
});
