/*
 * Design-language page content (docs/shep-design/README.md, "Screens > 3.
 * Design language"; design-files/Shep Design Language.dc.html is the pixel
 * source — copy below is verbatim from that file's `renderVals()`).
 *
 * This page is itself the house style reference — the handoff says so
 * directly ("Use this page as the source of truth for anything the other
 * two pages don't show"). So unlike docsRules.ts/docsLexicon.ts/
 * chalkboard.ts, these arrays are curated content, not parsed from a
 * decaying spec doc — there is no more-current source to track.
 *
 * What IS a decay risk, and what this file fixes relative to the original
 * design file: every color in the original `swatches`/`statuses` arrays
 * was a literal hex (`bg: "#2E8B57"`), even though seven of those nine
 * colors already have a token in ../styles/tokens.css. A literal hex here
 * would silently go stale the moment that token's value changes (a rename,
 * a contrast fix) — so every color below is a CSS custom property
 * reference (`"var(--meadow)"`), never a hex, and the swatch hex TEXT
 * shown on the page is read live from computed style in
 * ColorSection.astro's script, not stored here at all.
 *
 * Two colors have no matching UI token and are literals on purpose:
 * `statuses` "stopped" reuses --code-bg (closest existing neutral, already
 * a token) rather than inventing an untracked one; the terminal window's
 * three chrome dots and body/muted text reuse the --illus-terminal-*
 * tokens tokens.css already defines for exactly this "terminal chrome"
 * case.
 */

export interface Swatch {
  name: string;
  /** CSS custom property name, e.g. "--meadow" — read live, never a stored hex. */
  cssVar: string;
  use: string;
}

// The nine identity colors the handoff calls out as swatches (not every
// token in tokens.css — --hair, --dot, --code-bg, --shadow and
// --grass-deep are utility/derived colors, not part of this identity set).
export const swatches: Swatch[] = [
  { name: "Cream", cssVar: "--paper", use: "Paper. Every page starts here." },
  { name: "Ink", cssVar: "--ink", use: "Text, every outline, every hard shadow." },
  { name: "Meadow", cssVar: "--meadow", use: "Primary. Online, healthy, go." },
  { name: "Grass", cssVar: "--grass", use: "Large happy fills and terminal success." },
  { name: "Bark", cssVar: "--bark", use: "Errored, refused, destructive. Nothing else." },
  { name: "Butter", cssVar: "--butter", use: "Attention: launching, notes, highlights." },
  { name: "Sky", cssVar: "--sky", use: "Reference material and inline links in docs." },
  { name: "Fleece", cssVar: "--paper-2", use: "Cards and panels lifted off the paper." },
  { name: "Barn", cssVar: "--barn", use: "Scenery only — barn walls and roofs. Never UI or status." },
];

export interface TypeSpecimen {
  fontFamily: string;
  label: string;
  spec: string;
}

export const typeSpecimens: TypeSpecimen[] = [
  { fontFamily: "'Bricolage Grotesque', sans-serif", label: "Bricolage Grotesque", spec: "Display · 600, 800 · headings only" },
  { fontFamily: "'Space Grotesk', sans-serif", label: "Space Grotesk", spec: "Body · 400, 500, 700 · prose and UI" },
  { fontFamily: "'Space Mono', monospace", label: "Space Mono", spec: "Code · 400, 700 · terminals, labels, values" },
];

export interface TypeScaleRow {
  role: string;
  spec: string;
  family: string;
  weight: number;
  px: string;
  tracking: string;
  sample: string;
}

export const typeScale: TypeScaleRow[] = [
  { role: "Hero", spec: "Bricolage 800 · 86/80 · −4.5%", family: "'Bricolage Grotesque', sans-serif", weight: 800, px: "clamp(28px,4vw,44px)", tracking: "-.045em", sample: "keeps your flock alive" },
  { role: "Section", spec: "Bricolage 800 · 38/44 · −3%", family: "'Bricolage Grotesque', sans-serif", weight: 800, px: "clamp(24px,3vw,32px)", tracking: "-.03em", sample: "What works today" },
  { role: "Body", spec: "Space Grotesk 400 · 17/27", family: "'Space Grotesk', sans-serif", weight: 400, px: "17px", tracking: "0", sample: "It restarts them when they die, captures what they print, and says plainly when something is wrong." },
  { role: "Label", spec: "Space Mono 400 · 12 · +14% caps", family: "'Space Mono', monospace", weight: 400, px: "12px", tracking: ".14em", sample: "PRE-RELEASE, AND HONEST ABOUT IT" },
  { role: "Code", spec: "Space Mono 400 · 13/23", family: "'Space Mono', monospace", weight: 400, px: "13px", tracking: "0", sample: "shep bleats web --no-follow" },
];

export interface StatusPill {
  label: string;
  bg: string;
  fg: string;
}

export const statuses: StatusPill[] = [
  { label: "online", bg: "var(--grass)", fg: "var(--ink)" },
  { label: "launching", bg: "var(--butter)", fg: "var(--ink)" },
  { label: "stopped", bg: "var(--code-bg)", fg: "var(--ink-2)" },
  { label: "errored", bg: "var(--bark)", fg: "var(--paper-2)" },
  { label: "dog", bg: "var(--ink)", fg: "var(--butter)" },
];

export interface ShapeRule {
  name: string;
  w: string;
  h: string;
  fill: string;
  border: string;
  radius: string;
  shadow: string;
  tilt: string;
  rule: string;
}

export const shapeRules: ShapeRule[] = [
  { name: "Outline", w: "58px", h: "38px", fill: "var(--paper-2)", border: "3px", radius: "12px", shadow: "none", tilt: "0deg", rule: "3px on UI, 4px on illustration and hero panels. The only thinner line is a 2px table rule." },
  { name: "Radius", w: "58px", h: "38px", fill: "var(--butter)", border: "3px", radius: "999px", shadow: "none", tilt: "0deg", rule: "999px pills, 22px cards, 16px blocks inside cards, 14px buttons." },
  { name: "Shadow", w: "52px", h: "36px", fill: "var(--paper-2)", border: "3px", radius: "12px", shadow: "6px 6px 0 var(--shadow)", tilt: "0deg", rule: "Solid ink, equal x and y, never blurred. 3px on pills, 6px on cards, 9px on hero panels." },
  { name: "Tilt", w: "56px", h: "38px", fill: "var(--grass)", border: "3px", radius: "12px", shadow: "5px 5px 0 var(--shadow)", tilt: "-4deg", rule: "±1.4deg maximum, and a card straightens on hover. Anything more looks broken, not hand-set." },
];

export interface MotionRow {
  name: string;
  what: string;
  spec: string;
}

export const motionRows: MotionRow[] = [
  { name: "bob", what: "Sheep idling in the pasture", spec: "5–6.6s ease-in-out infinite · −7px, ±1.5deg" },
  { name: "chew", what: "The lead sheep's head only", spec: "2.4s ease-in-out infinite · −7deg" },
  { name: "wag", what: "The dog's tail — the fastest thing on any page", spec: "1.1s ease-in-out infinite · −14 to 16deg" },
  { name: "twinkle", what: "Stars before the sun comes up", spec: "2.6–4.8s ease-in-out infinite · opacity .35 → 1" },
  { name: "press", what: "Buttons and pills under the cursor", spec: "no transition · translate 2–3px, shadow shrinks the same" },
];

export interface SceneryRule {
  name: string;
  rule: string;
}

export const sceneryRules: SceneryRule[] = [
  { name: "The sky does the work", rule: "One fixed gradient behind the whole scene, night at the top to afternoon at the bottom. Scrolling moves you through the day; nothing else reacts to scroll." },
  { name: "Two hill bands, always", rule: "A pale far band and a saturated near band, both 4px outlined, both cropped past the viewport edge so no seam shows." },
  { name: "The barn is scenery", rule: "Barn red belongs to walls and roofs only. It never becomes a status, a button, or a border on a UI surface." },
  { name: "Population", rule: "Three to five sheep and exactly one dog per scene. The dog stands apart from the flock — it watches, it is not counted." },
];

export interface VoiceRegister {
  where: string;
  register: string;
  tone: string;
  yes: string;
  no: string;
}

export const voiceRegisters: VoiceRegister[] = [
  { where: "Landing page", register: "playful", tone: "var(--meadow)", yes: "shep keeps your flock alive.", no: "Enterprise-grade process orchestration." },
  { where: "Docs prose", register: "playful, precise", tone: "var(--meadow)", yes: "A dog watches the flock rather than being part of it.", no: "Woof! Let's get your doggos going!" },
  { where: "Config reference", register: "plain", tone: "var(--sky)", yes: "instances — number of processes to run.", no: "instances — how many sheep in this pen." },
  { where: "Errors and logs", register: "technical only", tone: "var(--bark)", yes: "no shepherd channel — set channel = true", no: "Oh no, the sheep wandered off! 🐑" },
];

export interface StandingRule {
  title: string;
  body: string;
}

// The hero's "Three rules" card. Same three standing rules as
// docs/terminology.md's "Usage rules (readability > theme)" section (rules
// 4, 2 and 1, in that order) and web/src/data/docsRules.ts's five cards —
// worded to this page's own hero-card register rather than parsed from
// either, since neither source splits into exactly these three sentences.
export const threeRules: StandingRule[] = [
  { title: "Whimsy in prose, plain in reference.", body: "The homepage says flock. The config table says process." },
  { title: "Never charming about damage.", body: "kill, delete, errors and exit codes stay literal." },
  { title: "Every themed word has a straight twin.", body: "bleats is also logs. Forever." },
];

export interface DoDontList {
  heading: "Do" | "Don't";
  tone: "meadow" | "bark";
  items: string[];
}

export const marksDoDont: DoDontList[] = [
  {
    heading: "Do",
    tone: "meadow",
    items: [
      "Keep 4px of clear fleece around the mark's outline at every size.",
      "Below 32px wide, drop the legs and use the head-and-fleece silhouette only.",
      "Put the mark on meadow, butter or ink. Never on grass — the fleece stops reading.",
    ],
  },
  {
    heading: "Don't",
    tone: "bark",
    items: [
      "Add a second animal. One sheep, one dog, that's the cast.",
      "Rotate, gradient-fill, or add a drop shadow inside the mark.",
      "Draw the sheep in distress to illustrate an error. Errors get a color, not a face.",
    ],
  },
];
