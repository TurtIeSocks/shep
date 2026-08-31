# Handoff: shep marketing site + docs

## Overview

Three connected pages for **shep**, a process manager written in Rust (a pm2 rewrite). The daemon is called the *shepherd*, managed processes are the *flock*, and the whole product carries a farm vocabulary that is deliberately dropped wherever it would cost clarity.

- **Landing page** — a single scrolling scene. The sky gradient runs night → dawn → afternoon top to bottom, so scrolling "brings the sun up" behind sticky pasture scenery. Five content scenes sit on top of it, then a lexicon section, a features grid, a chalkboard of what isn't built, and a CTA footer.
- **Docs** — sidebar + article layout with a light/dark toggle. Two pages are written (Getting started, Terminology); eight are honest stubs marked `soon`.
- **Design language** — the house style reference: marks, color, type, components, shape and motion, voice.

## About the design files

The files in `design-files/` are **design references created in HTML** — prototypes showing intended look and behavior, not production code to copy directly. They are authored in a small in-house component format (`.dc.html`, driven by `support.js`), which exists only in the design tool. Do not try to port `support.js`, `<x-dc>`, `<sc-for>`, `<sc-if>` or `{{ }}` holes.

**The task is to recreate these designs in the target codebase's existing environment**, using its established patterns and libraries. If no web environment exists yet, this is a static marketing site + docs — Astro or Next.js with plain CSS (or Tailwind, if preferred) is a good fit. The repo itself (`pm2-rs` / `github.com/shep-pm/shep`) is a Rust workspace, so the site likely lives in its own directory or its own repo.

Read the files as: the template markup between `<x-dc>` and `</x-dc>` is the DOM; the `class Component` block at the bottom is the data and the event handlers. Anything referenced as `{{ name }}` in markup is defined by the `renderVals()` return at the bottom of the same file. `style-hover="…"` means a `:hover` rule with those declarations.

## Fidelity

**High-fidelity.** Colors, typography, spacing, border weights, shadow offsets and animation timings are final and are all listed below. Recreate pixel-for-pixel. Copy is final — use it verbatim; it was written to a specific voice spec (see *Voice*, below).

The only intentionally unfinished thing is the eight stub docs pages, which are meant to ship as stubs.

---

## Design tokens

### Color

| Token | Light | Dark | Use |
|---|---|---|---|
| `--paper` | `#FBF6E7` | `#131E18` | Page background. Every page starts here. |
| `--paper-2` (Fleece) | `#FFFDF5` | `#1B2A21` | Cards and panels lifted off the paper. |
| `--ink` | `#17251C` | `#F0ECDC` | Text, every outline, every hard shadow. |
| `--ink-2` | `#3D5245` | `#BFCFC2` | Body prose, secondary text. |
| `--ink-3` | `#7A8C80` | `#7E9186` | Labels, captions, muted values. |
| `--line` | `#17251C` | `#35493C` | Outlines. |
| `--hair` | `rgba(23,37,28,.14)` | `rgba(240,236,220,.14)` | Table rules, soft dividers. |
| `--meadow` | `#2E8B57` | `#59C47D` | Primary. Online, healthy, go. |
| `--grass` | `#6FCB6B` | `#7FD98E` | Large happy fills, terminal success. |
| `--grass-deep` | `#2A7444` | — | Primary button fill, CTA band. |
| `--bark` | `#E0552B` | `#FF7B4F` | Errored, refused, destructive. Nothing else. |
| `--butter` | `#F3C44C` | `#F6D072` | Attention: launching, notes, highlights. |
| `--sky` | `#4E9DD3` | `#78BEEC` | Reference material and inline doc links. |
| `--barn` | `#C6432E` | — | **Scenery only** — barn walls and roofs. Never UI or status. |
| `--shadow` | `#17251C` | `#050A07` | Hard shadow color. |
| `--code-bg` | `#F1EBD8` | `#22332A` | Inline `<code>` background (docs). |
| `--dot` | `rgba(23,37,28,.10)` | `rgba(240,236,220,.08)` | Dot-grid background. |

Non-token literals used in illustration: dog body `#2A1C15`, dog far legs `#1E140F`, dog muzzle `#E8D9C3`, barn door `#A8351F`, hayloft glass `#F7E4B0`, foundation `#3D5245`, foundation joints `#2A3B31`, far hill `#8CD08A`, moon `#F4F0E2` with craters `#DCD6C2`. Terminal chrome: bg `#0D1712`, border `#35493C`, body text `#E8E4D4`, muted `#7E9186`.

### Type

Google Fonts, one link:
`https://fonts.googleapis.com/css2?family=Bricolage+Grotesque:opsz,wght@12..96,400;12..96,600;12..96,800&family=Space+Grotesk:wght@400;500;700&family=Space+Mono:wght@400;700&display=swap`

- **Bricolage Grotesque** 600/800 — display and headings only. Never body.
- **Space Grotesk** 400/500/700 — prose and UI. Body default: `font-family:'Space Grotesk', system-ui, sans-serif`.
- **Space Mono** 400/700 — terminals, labels, values, code.

| Role | Spec |
|---|---|
| Hero | Bricolage 800, `clamp(40px,6vw,88px)`, line-height `.87`, tracking `-.055em` |
| Section h2 | Bricolage 800, `clamp(32px,4.4vw,58px)`, line-height `.98`, tracking `-.04em` |
| Card h3 | Bricolage 800, 21px, tracking `-.025em`, line-height 1.15 |
| Body | Space Grotesk 400/500, 15.5–19px, line-height 1.5–1.65 |
| Label | Space Mono 400, 11–12.5px, tracking `.12–.16em`, uppercase |
| Code | Space Mono 400, 13–14.5px, line-height 1.85–1.9 |

`text-wrap: pretty` on long prose, `text-wrap: balance` on the design-language hero.

### Shape, shadow, motion

- **Outline:** 3px on UI, 4px on illustration, hero panels and pasture bands. The only thinner line is a 2px table rule (`--hair`).
- **Radius:** `999px` pills, 22–26px cards, 16–18px blocks inside cards, 14–16px buttons, 10–11px small nav buttons.
- **Shadow:** solid ink, equal x and y, **never blurred**. 3px on pills, 5–7px on cards, 9px on hero panels. Some inverted cases use a non-ink shadow deliberately: ink button on grass gets `6px 6px 0 #FFFDF5`; the nav GitHub button gets `4px 4px 0 var(--barn)`.
- **Tilt:** cards rotate ±1.4deg max; feature cards straighten on hover.
- **Press state:** on hover, `transform: translate(2px,2px)` (3px on large buttons) and the shadow shrinks by the same amount. No transition — it is a state, not an animation.
- **Keyframes** (all `ease-in-out infinite`):
  - `bob` 5–6.6s — `translateY(0 → -7px)` with `rotate(-1.5deg → 1.5deg)`. Sheep idle.
  - `chew` 2.4s — `rotate(0 → -7deg)`, transform-origin `34px 40px`. Lead sheep's head only.
  - `wag` 1.1s — `rotate(-14deg → 16deg)`, transform-origin `106px 50px`. The dog's tail; fastest thing on the page.
  - `twinkle` 2.6–4.8s — `opacity .35 → 1`. Stars.
  - Give any two animals different durations *and* offset delays. Two sheep bobbing in lockstep reads as a loading spinner.
- Nothing animates on scroll. The only scroll effect is the fixed sky gradient revealed behind sticky scenery.

---

## Screens

### 1. Landing page — `Shep Landing v3 scene.dc.html`

**Purpose:** convince a pm2 user to build shep from source, and be honest about pre-release state.

**Structure.** One relative wrapper (`[data-stage="1"]`) contains, in z-order:

| z | Layer | Notes |
|---|---|---|
| 0 | Sky | `position:absolute; inset:0`, one `linear-gradient(180deg, …)` with 14 stops from `#0E1730` at 0% through `#E7A468` at 27.3% to `#CFEBF8` from 68.2% down. This one gradient spans the whole stage — that's the day-break effect. |
| 1 | Stars, moon, sheep-clouds, sun | Absolutely positioned in `vh` units down the stage (e.g. sun at `top:224vh`, clouds at 52/128/262/332/392vh). |
| 2 | Pasture | `position:sticky; top:0; height:100vh; margin-bottom:-100vh; overflow:hidden; pointer-events:none` — the scenery pins while content scrolls past. |
| 3 | Night wash | `mix-blend-mode:multiply` gradient, `rgba(16,26,52,.72)` at top fading to transparent at 32.7%, so early content sits on a dark sky and later content doesn't. |
| 4 | Barn glow | Second sticky layer, a radial butter glow positioned to the barn's hayloft window. |
| 5 | Content scenes | Five `<section>`s, each `min-height:100vh; display:flex; align-items:center; padding:110px 0 60px`, content constrained to `max-width:1240px; padding:0 28px` and a `width:min(600px,54vw)` column on the **left** so scenery stays visible on the right. |
| 6 | Lexicon (`#pasture`) | Grass background, signpost cards. |
| 7 | Features / chalkboard / CTA footer | Paper and grass-deep bands below the stage. |

**Sticky pasture contents** (bottom-anchored, inside the z-2 layer):
- Far hill: `viewBox="0 0 1200 210"`, `preserveAspectRatio="none"`, `height:43vh`, fill `#8CD08A`, stroke ink 4px, inset `left:-4%; right:-4%; bottom:8%`.
- Near hill: `viewBox="0 0 1200 260"`, `height:27vh`, fill `--grass`, stroke ink 4px, at `bottom:0`.
- Fence: `viewBox="0 0 1400 76"`, `height:54px`, at `bottom:7vh`. Two 12px-tall rails plus posts at x = 30, 180, 330, 480, 630, 780, 930, 1080, 1230, 1350, each `M{x} 74V14l14-10 14 10v60Z`.
- Barn: `right:6%; bottom:26%; width:min(300px,26vw)`.
- Three sheep at `left:7%/31%/56%`, `bottom:9vh/4vh/11vh`, widths 110/86/68px, `bob` at 5s/6.4s/5.6s with 0/.9s/.4s delays. Only the first chews.
- Dog at `right:25%; bottom:5vh; width:138px`.

**Nav.** Sticky, `margin-bottom:-91px` so it overlaps the hero. Pill-shaped logo lockup (`paper-2`, 3px ink border, `4px 4px 0` shadow) + three pills: Lexicon (`paper-2`), Docs (`butter`), GitHub (ink fill, cream text, barn-colored shadow).

**Scene copy** (verbatim):
1. Eyebrow pill `pm2, rewritten in Rust`; h1 `shep keeps / your flock / alive.` in cream with `text-shadow:5px 5px 0 rgba(14,23,48,.45)`; paragraph in a fleece card; primary button `Get started →` (grass-deep) + a mono copy-to-clipboard button showing `$ git clone github.com/shep-pm/shep` with a `copy`/`copied` label that reverts after 1800ms; a scroll hint pill `scroll — the sun comes up ↓`.
2. `the flock, listed` — a terminal window (mac dots, `~/apps` title) showing `shep ls` output, then a note card about the CPU column printing `-` instead of `0.0%`.
3. Fleece card, `rotate(-1.2deg)`, heading `A typo fails at load, not at 3am.` with a small TOML block.
4. Barn-red card, `rotate(1deg)`, heading `Dogs work for the shepherd.` with a dark terminal block.
5. Fleece card with bark border, `rotate(-.8deg)`, `pre-release` chip, and a `See exactly what's missing` button linking to `#chalkboard`.

**Lexicon (`#pasture`).** Grass band, h2 `Fifteen words carry the whole product.`, then six signpost cards in `repeat(auto-fit,minmax(258px,1fr))` with 26px gap. Each card: a 6px ink post above it (absolutely positioned, `translateX(-50%)`), then a tilted card with two ink screw-dots, term (Bricolage 800 23px), meaning, and a mono CLI line at 62% opacity. Per-card background/foreground/tilt come from data — three fleece, one butter, one meadow-deep (`#2A7444`, cream text), one barn (cream text).

**Features.** Paper band, h2 `Why you'd switch`, six cards in `repeat(auto-fit,minmax(280px,1fr))`, each with a 44px numbered chip (butter/grass/`#93D6F0`/bark rotating), tilted ±1deg, straightening and lifting on hover (`translate(-2px,-3px)`, shadow grows to 10px).

**Chalkboard (`#chalkboard`).** A `#8A5A33` wooden frame (4px ink border, 26px radius, 18px padding) around a `#25423A` slate (16px radius, 38px 34px padding). Thirteen dashed pills, each `✗` in `#FF9A72` + the item name.

**CTA footer.** `--grass-deep` band, two-column grid, butter and cream buttons with ink shadows, then five bobbing sheep at widths 72/104/58/88/66px along the bottom. Below it a `#17251C` bar with the license line and four text links.

### 2. Docs — `Shep Docs.dc.html`

**Layout.** Sticky header (paper, 3px ink bottom border) over `max-width:1340px; display:grid; grid-template-columns:264px minmax(0,1fr)`. Sidebar is `position:sticky; top:64px`, 3px ink right border, `min-height:calc(100vh - 64px)`. Main is `padding:44px 56px 90px; max-width:860px`.

**Header.** Logo lockup linking to the landing page, a `docs · pre-release` mono pill, text links Home / Design language, a 36px circular theme toggle showing `☾`/`☀`, and an ink GitHub button with a meadow shadow.

**Sidebar.** Three groups — Start here (Getting started, Your first Flockfile, Coming from pm2), Concepts (Terminology, Folds, The shepherd channel, Dogs), Reference (CLI, JSON output, What's not built). Group labels are mono 10.5px uppercase, tracking `.16em`. Items are buttons: active = butter fill, ink text, 700 weight, 2px ink border; inactive = transparent, `--ink-2`, 400. Unbuilt items carry a `soon` tag in bark on the right. Footer note explains the tag.

**Routing.** Client-side page state only, no URLs per page (`state.page`, default `start`). On mount, `location.hash === "#terminology"` selects the Terminology page — the landing page links there. Changing page scrolls to top. **In a real implementation, give each page a real route** and keep `#terminology` working as a redirect.

**Getting started** — h1 + lede, a bark-bordered `careful` callout about no install script and Windows exiting 1, then five numbered `<h2>` steps: Build it (cargo block), Write a Flockfile (TOML panel + butter `note` callout on strict parsing), Start the flock (`shep ls` terminal + note on the `-` CPU column), Watch what it prints (four alias cards: `shep bleats`/`logs`, `shep flock`/`list, ls`, `shep muster`/`resurrect`, `shep thatlldo`/`graceful stop`), Pipe it somewhere (syntax-highlighted JSON envelope). Ends with three `Where to go next` cards that navigate.

**Terminology** — h1, two lede paragraphs, then the lexicon table: a 4-column grid `150px minmax(0,1.1fr) minmax(0,1.1fr) 74px`, 14px gap, ink header row with butter first label, 15 rows on `paper-2` separated by 2px `--hair`. Columns: term (Bricolage 800 meadow), meaning, where you meet it (mono), built (`yes` meadow / `partly` butter / `no` bark, right-aligned). Then a card on sheepdogs-vs-sheep, then five numbered usage rules.

**Stub pages** — h1, blurb, and a dashed-border panel with a sleeping-sheep SVG (closed eye = a 2px line instead of the pupil), the line `This page hasn't been written yet.`, the repo path the material lives at, and a `Read it on GitHub →` button. Eight pages use this: Your first Flockfile, Coming from pm2, Folds, The shepherd channel, Dogs, CLI, JSON output, What's not built.

**Theme.** `data-theme="dark"` on `<html>`, persisted in `localStorage` under `shep-theme`, read on mount. `body` transitions `background` and `color` at `.25s ease`. The design-language page shares the same key.

### 3. Design language — `Shep Design Language.dc.html`

Reference page on a `radial-gradient` dot grid (`--dot` 1.5px dots, `24px 24px`). Header with the sheep mark, `design language v1.0` label, and a `☾ night pasture` / `☀ day pasture` theme toggle. Hero `Cute on the outside. Exact where it counts.` beside a *Three rules* card. Then six numbered sections: 01 The marks (sheep, dog, wordmark + Do/Don't dashed panels), 02 Color (nine swatches on a `paper-2` band), 03 Type (three specimen cards + a five-row type-scale table), 04 Components (on an ink band: buttons, status pills, terminal block, callouts), 05 Shape and motion (shape rules with live specimens, the motion table, scenery rules), 06 Voice (four registers with a yes/no column pair, then a grass CTA card).

Use this page as the source of truth for anything the other two pages don't show.

---

## Illustration

Three SVG marks, all flat fills with ink outlines, no gradients, no blur.

**Sheep** — `viewBox="0 0 104 78"`. Two ink legs (`rect` 8×21, rx 4, at x=40 and x=66, y=52), a cream fleece blob (`M40 20c6-9 22-10 28-2 10-3 19 4 18 13 7 4 7 15-1 19-2 8-12 11-19 8-8 5-21 4-26-3-10 1-17-7-15-16-4-8 4-18 15-19Z`, 4px ink stroke), an ink ear ellipse (rx 9, ry 7, `rotate(-28 26 34)`), an ink head ellipse (cx 27, cy 48, rx 15, ry 14), and a cream eye (r 3) with an ink pupil (r 1.4). Below 32px wide, drop the legs.

**Dog (border collie)** — `viewBox="0 0 154 112"`. Ground shadow ellipse; tail as an 11px ink stroke `M104 52c18-2 27-14 24-29` with a 7px cream tip, wagging from origin `106px 50px`; two far legs in `#1E140F`; torso `M40 56c0-13 15-22 37-22s33 8 33 21-13 22-33 22-37-8-37-21Z` in `#2A1C15`; a cream chest wedge; two near legs in `#2A1C15`; four cream paws; then the head group at `translate(4,-8) scale(0.8)` — same head as the design-language dog card, with stroke widths pre-multiplied (5, 3.75, 2.5) so they render at 4/3/2px. The head faces the viewer while the body is in profile; that is intentional and keeps it consistent with the logo.

**Barn** — `viewBox="0 0 320 262"`. Ground shadow; weathervane (4px ink stem + butter pennant); cupola with its own gable roof; gambrel roof `M160 42 262 78 288 112H32L58 78Z` in `--barn` with 6px ink stroke; a `#F7E4B0` hayloft door (38×32, ink mullion) with a beam below; body `rect` 44,110 232×112; a `#3D5245` stone foundation (252×22, rx 4) with four darker joints; cream trim — two horizontals at y=124 and y=214 plus two verticals framing the doors; a `#A8351F` double door (110,142 100×74) with a cream X and center post; a sliding-door rail with two cream hangers; and two `#F7E4B0` side windows (30×30) with ink cross mullions.

Do not add a second animal — one sheep and one dog is the whole cast. Never draw the sheep in distress to illustrate an error; errors get a color, not a face.

## Interactions

- **Copy button** (landing hero): writes `git clone https://github.com/shep-pm/shep.git` to the clipboard, label flips `copy` → `copied`, reverts after 1800ms.
- **Anchor nav:** `html { scroll-behavior: smooth }` on the landing page; `#pasture`, `#chalkboard`, `#scene-1` targets.
- **Docs nav:** page state + scroll to top; `#terminology` deep link.
- **Theme toggle:** docs and design language, `localStorage` key `shep-theme`.
- **Hover:** the press state described under *Shape*, plus feature cards straightening from their tilt.
- **Tweakable props** on the landing page (design-tool concepts, useful as feature flags or not at all): `nightSky`, `sheepClouds`, `showBarn` — each default `true`, each hides its layer.

## Responsive

Currently desktop-first and not finished for small screens — the landing scenes use `width:min(600px,54vw)` columns beside scenery, and the docs grid has a fixed 264px sidebar. **This needs a mobile pass you'll have to design:** likely a single-column stack with scenery reduced to a band, and a collapsible docs sidebar. Type already scales via `clamp()`; grids already use `auto-fit`/`minmax`, so the card sections reflow on their own.

## Voice

Four registers. Getting these wrong undoes the design.

| Where | Register | Yes | No |
|---|---|---|---|
| Landing page | playful | `shep keeps your flock alive.` | `Enterprise-grade process orchestration.` |
| Docs prose | playful, precise | `A dog watches the flock rather than being part of it.` | `Woof! Let's get your doggos going!` |
| Config reference | plain | `instances — number of processes to run.` | `instances — how many sheep in this pen.` |
| Errors and logs | technical only | `no shepherd channel — set channel = true` | `Oh no, the sheep wandered off! 🐑` |

Three standing rules: whimsy in prose, plain in reference; never charming about damage (`kill`, `delete`, errors, exit codes stay literal); every themed word has a straight twin, forever (`bleats` is also `logs`).

## Assets

No raster assets, no icon library. Every graphic is inline SVG defined in these files. Fonts come from Google Fonts. Emoji appear only as three glyphs used as icons — `☾`, `☀`, `✗` — and nowhere else.

## Files

In `design-files/`:

| File | Contents |
|---|---|
| `Shep Landing v3 scene.dc.html` | Landing page. The keeper version. |
| `Shep Docs.dc.html` | Docs shell, Getting started, Terminology, stub template. |
| `Shep Design Language.dc.html` | Style reference — tokens, marks, components, motion, voice. |
| `support.js` | Runtime for the design-tool format. Reference only; do not port. |

Open any of the three HTML files directly in a browser to see the live design.

In `screenshots/`:

| File | Shows |
|---|---|
| `01-landing.png` … `08-landing.png` | The landing page top to bottom, one viewport per step: hero at night, the flock terminal, the Flockfile card, the dogs card, the pre-release card, the lexicon signposts, the chalkboard, the CTA footer. **Read these in order** — they are the only way to see how the sky gradient advances from night to afternoon while the pasture stays pinned. |
| `01-docs.png` | Getting started, light. |
| `02-docs.png` | Terminology — the lexicon table. |
| `03-docs.png` | A stub page (Dogs). |
| `04-docs.png` | Getting started, dark. |
| `05-docs.png` | Docs back in light after toggling, for comparison. |
| `01-design-language.png` … `06-design-language.png` | The design language page top to bottom: hero and three rules, the marks, color swatches, type, components on the ink band, shape and motion, voice. |

Screenshots are captured at the design tool's viewport width, so the exact column widths in them are one sample of the `clamp()`/`vw` behavior — trust the specs above over pixel-measuring the images.
