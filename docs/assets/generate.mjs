#!/usr/bin/env node
/**
 * Generates the README's SVG figures.
 *
 * # Why a generator rather than hand-written SVG files
 *
 * These figures share a palette, a type scale, a grid and a set of primitives
 * (rounded panels, pill labels, token cells, arrows, dimension lines). Hand-editing
 * nine files that each re-declare those means every colour change is nine edits and
 * every new figure drifts slightly from the last. Here the shared vocabulary is
 * defined once and each figure is a function.
 *
 * It also keeps the numbers honest. The figures quote real measurements — 4034
 * tokens reused, 13080 prefilled, 24% saved — and those come from `sakur4d demo`.
 * When the engine changes, this file is regenerated rather than a number being
 * quietly left stale in a picture nobody re-reads.
 *
 * # Constraints the output must satisfy
 *
 * * **GitHub sanitises SVG.** No `<script>`, no external references, no CSS
 *   `@import`, no event handlers. Presentation attributes and inline `<style>` with
 *   class selectors are fine.
 * * **Both themes.** GitHub renders a README's images on white *or* dark. These are
 *   dark-canvas figures with their own background panel, so they read the same in
 *   either — which is why every figure declares its own `rect` background rather
 *   than relying on transparency.
 * * **No fonts to download.** Only generic families (`ui-monospace`, `SFMono-Regular`,
 *   `Menlo`, `Consolas`, `monospace`), so nothing 404s and nothing shifts layout.
 *
 * Usage:  node docs/assets/generate.mjs
 */

import { mkdirSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const HERE = dirname(fileURLToPath(import.meta.url));

// ===========================================================================
// Design tokens
// ===========================================================================

const T = {
  // Canvas
  bg: "#0b0f14",
  bgAlt: "#0d1319",
  panel: "#111823",
  panelEdge: "#1e2a3a",
  grid: "#16202c",

  // Text
  ink: "#e8eef6",
  inkDim: "#96a7bd",
  inkFaint: "#5d7089",

  // Semantic colours. Each carries one meaning across every figure:
  //   cyan   — what Sakur4 is doing / the boundary decision
  //   green  — reused, preserved, verified, safe
  //   amber  — evicted, rewritten, a cost, a warning
  //   violet — memory and interpretation (the Semantic Atlas)
  //   red    — the failure this project exists to remove
  cyan: "#22d3ee",
  cyanDim: "#0e7490",
  green: "#34d399",
  greenDim: "#065f46",
  amber: "#fbbf24",
  amberDim: "#78350f",
  violet: "#a78bfa",
  violetDim: "#4c1d95",
  red: "#f87171",
  redDim: "#7f1d1d",
  blue: "#60a5fa",
  blueDim: "#1e3a8a",
};

const MONO = "ui-monospace, SFMono-Regular, 'SF Mono', Menlo, Consolas, 'Liberation Mono', monospace";
const SANS = "-apple-system, BlinkMacSystemFont, 'Segoe UI', Inter, Helvetica, Arial, sans-serif";

// ===========================================================================
// Primitives
// ===========================================================================

const esc = (s) =>
  String(s).replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;").replace(/"/g, "&quot;");

/** A dark canvas with a subtle technical grid and a vignette. */
function canvas(w, h, { grid = true, id = "c" } = {}) {
  return `
  <defs>
    <pattern id="${id}-grid" width="32" height="32" patternUnits="userSpaceOnUse">
      <path d="M32 0H0V32" fill="none" stroke="${T.grid}" stroke-width="1"/>
    </pattern>
    <pattern id="${id}-dots" width="16" height="16" patternUnits="userSpaceOnUse">
      <circle cx="1" cy="1" r="1" fill="${T.grid}"/>
    </pattern>
    <radialGradient id="${id}-vig" cx="50%" cy="0%" r="95%">
      <stop offset="0%" stop-color="#0f1720" stop-opacity="0"/>
      <stop offset="100%" stop-color="#05080b" stop-opacity="0.85"/>
    </radialGradient>
    <linearGradient id="${id}-sheen" x1="0" y1="0" x2="1" y2="1">
      <stop offset="0%" stop-color="#ffffff" stop-opacity="0.05"/>
      <stop offset="60%" stop-color="#ffffff" stop-opacity="0"/>
    </linearGradient>
    <!--
      Two off-centre washes and a hairline edge, shared by every figure.

      A single centred glow made the whole set read as evenly lit, which is what makes a diagram
      look generated rather than drawn: with no light direction there is no sense of a surface.
      Two washes — cyan where the title sits, violet at the opposite corner — give the eye a
      direction to follow, and the hairline stops the top edge dissolving into the page.
    -->
    <radialGradient id="${id}-washA" cx="16%" cy="8%" r="64%">
      <stop offset="0%" stop-color="${T.cyan}" stop-opacity="0.13"/>
      <stop offset="100%" stop-color="${T.cyan}" stop-opacity="0"/>
    </radialGradient>
    <radialGradient id="${id}-washB" cx="86%" cy="97%" r="60%">
      <stop offset="0%" stop-color="${T.violet}" stop-opacity="0.16"/>
      <stop offset="100%" stop-color="${T.violet}" stop-opacity="0"/>
    </radialGradient>
    <linearGradient id="${id}-edge" x1="0" y1="0" x2="1" y2="0">
      <stop offset="0%" stop-color="${T.cyan}" stop-opacity="0.8"/>
      <stop offset="55%" stop-color="${T.violet}" stop-opacity="0.45"/>
      <stop offset="100%" stop-color="${T.violet}" stop-opacity="0"/>
    </linearGradient>
  </defs>
  <rect width="${w}" height="${h}" fill="${T.bg}"/>
  <rect width="${w}" height="${h}" fill="url(#${id}-washA)"/>
  <rect width="${w}" height="${h}" fill="url(#${id}-washB)"/>
  ${grid ? `<rect width="${w}" height="${h}" fill="url(#${id}-grid)" opacity="0.55"/>` : ""}
  <rect width="${w}" height="${h}" fill="url(#${id}-vig)"/>
  <rect x="0" y="0" width="${w}" height="2" fill="url(#${id}-edge)"/>`;
}

/** Rounded panel with a hairline border and an optional glow. */
function panel(x, y, w, h, { r = 12, fill = T.panel, stroke = T.panelEdge, glow = null, sw = 1 } = {}) {
  const glowDef = glow
    ? `<filter id="g${x}${y}" x="-30%" y="-30%" width="160%" height="160%">
         <feGaussianBlur stdDeviation="7" result="b"/>
         <feFlood flood-color="${glow}" flood-opacity="0.4"/>
         <feComposite in2="b" operator="in"/>
         <feMerge><feMergeNode/><feMergeNode in="SourceGraphic"/></feMerge>
       </filter>`
    : "";
  return `${glowDef}<rect x="${x}" y="${y}" width="${w}" height="${h}" rx="${r}" fill="${fill}" stroke="${stroke}" stroke-width="${sw}"${glow ? ` filter="url(#g${x}${y})"` : ""}/>`;
}

function text(x, y, content, { size = 13, fill = T.ink, family = SANS, weight = 400, anchor = "start", opacity = 1, spacing = 0 } = {}) {
  return `<text x="${x}" y="${y}" font-family="${family}" font-size="${size}" font-weight="${weight}" fill="${fill}" text-anchor="${anchor}" opacity="${opacity}"${spacing ? ` letter-spacing="${spacing}"` : ""}>${esc(content)}</text>`;
}

/** Monospace text, for anything that is literally a value the program prints. */
function mono(x, y, content, opts = {}) {
  return text(x, y, content, { family: MONO, size: 12, fill: T.inkDim, ...opts });
}

/** A pill label. */
function pill(x, y, label, { fill = T.panel, stroke = T.panelEdge, ink = T.inkDim, size = 10.5, padX = 9, h = 20, weight = 500 } = {}) {
  const w = label.length * size * 0.62 + padX * 2;
  return `<rect x="${x}" y="${y}" width="${w.toFixed(1)}" height="${h}" rx="${h / 2}" fill="${fill}" stroke="${stroke}" stroke-width="1"/>
  ${text(x + padX, y + h / 2 + size * 0.36, label, { size, fill: ink, weight, spacing: 0.4 })}`;
}

/** A left-to-right arrow, optionally labelled above and below. */
function arrow(x1, y1, x2, y2, { color = T.cyan, dash = null, width = 1.6, above = null, below = null, head = 7 } = {}) {
  const angle = Math.atan2(y2 - y1, x2 - x1);
  const bx = x2 - head * Math.cos(angle);
  const by = y2 - head * Math.sin(angle);
  const mx = (x1 + x2) / 2;
  const my = (y1 + y2) / 2;
  return `
  <line x1="${x1}" y1="${y1}" x2="${bx}" y2="${by}" stroke="${color}" stroke-width="${width}"${dash ? ` stroke-dasharray="${dash}"` : ""} stroke-linecap="round"/>
  <path d="M${x2} ${y2} L${bx - head * 0.35 * Math.sin(angle)} ${by + head * 0.35 * Math.cos(angle)} L${bx + head * 0.35 * Math.sin(angle)} ${by - head * 0.35 * Math.cos(angle)} Z" fill="${color}"/>
  ${above ? text(mx, my - 9, above, { size: 10.5, fill: T.inkDim, anchor: "middle" }) : ""}
  ${below ? text(mx, my + 18, below, { size: 10.5, fill: T.inkFaint, anchor: "middle" }) : ""}`;
}

/** A row of token cells, used everywhere the figures talk about prompt structure. */
function tokenRow(x, y, count, cellW, { h = 22, gap = 2, fill = T.panel, stroke = T.panelEdge, from = 0, to = null } = {}) {
  const end = to ?? count;
  let out = "";
  for (let i = from; i < end; i++) {
    out += `<rect x="${(x + i * (cellW + gap)).toFixed(1)}" y="${y}" width="${cellW}" height="${h}" rx="3" fill="${fill}" stroke="${stroke}" stroke-width="1"/>`;
  }
  return out;
}

/** A labelled dimension line with ticks. */
function dimension(x1, x2, y, label, { color = T.cyan, tick = 5 } = {}) {
  return `
  <line x1="${x1}" y1="${y - tick}" x2="${x1}" y2="${y + tick}" stroke="${color}" stroke-width="1.2"/>
  <line x1="${x2}" y1="${y - tick}" x2="${x2}" y2="${y + tick}" stroke="${color}" stroke-width="1.2"/>
  <line x1="${x1}" y1="${y}" x2="${x2}" y2="${y}" stroke="${color}" stroke-width="1.2" marker-start="url(#dim-s)" marker-end="url(#dim-e)"/>
  ${text((x1 + x2) / 2, y - 9, label, { size: 10.5, fill: color, anchor: "middle", family: MONO })}`;
}

/** Blinking-free caret used to mark the eviction boundary. */
function boundaryMarker(x, yTop, yBottom, label, color = T.cyan) {
  return `
  <line x1="${x}" y1="${yTop}" x2="${x}" y2="${yBottom}" stroke="${color}" stroke-width="1.6" stroke-dasharray="4 3"/>
  <circle cx="${x}" cy="${yTop}" r="3.5" fill="${color}"/>
  <circle cx="${x}" cy="${yBottom}" r="3.5" fill="${color}"/>
  ${text(x, yTop - 10, label, { size: 10, fill: color, anchor: "middle", family: MONO })}`;
}

function svg(w, h, body, { title, desc }) {
  return `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 ${w} ${h}" width="${w}" height="${h}" role="img" aria-labelledby="t d">
  <title id="t">${esc(title)}</title>
  <desc id="d">${esc(desc)}</desc>
  <defs>
    <marker id="dim-s" markerWidth="6" markerHeight="6" refX="3" refY="3" orient="auto"><path d="M6 0L0 3L6 6" fill="none" stroke="${T.cyan}" stroke-width="1"/></marker>
    <marker id="dim-e" markerWidth="6" markerHeight="6" refX="3" refY="3" orient="auto"><path d="M0 0L6 3L0 6" fill="none" stroke="${T.cyan}" stroke-width="1"/></marker>
  </defs>
${body}
</svg>
`;
}

const write = (name, content) => {
  mkdirSync(HERE, { recursive: true });
  writeFileSync(join(HERE, name), content, "utf8");
  process.stdout.write(`  ${name}  ${(content.length / 1024).toFixed(1)} KB\n`);
};

/**
 * Wrap text to a character budget, breaking at spaces.
 *
 * SVG has no text wrapping, so every multi-line label in these figures is
 * pre-split. Doing it by slicing at a fixed offset — which an earlier version did
 * — splits mid-word and leaves orphans like "t, within tolerance" on a line of
 * their own, which reads as a typo rather than as a layout.
 */
function wrap(text, maxChars) {
  const lines = [];
  let line = "";
  for (const word of String(text).split(/\s+/)) {
    if (line === "") {
      line = word;
    } else if (`${line} ${word}`.length <= maxChars) {
      line += ` ${word}`;
    } else {
      lines.push(line);
      line = word;
    }
  }
  if (line) lines.push(line);
  return lines;
}

/** Emit wrapped body text as consecutive lines. */
function paragraph(x, y, content, { maxChars = 44, size = 11, fill = T.inkDim, leading = 16 } = {}) {
  return wrap(content, maxChars)
    .map((line, i) => text(x, y + i * leading, line, { size, fill }))
    .join("");
}

// ===========================================================================
// 1 · Hero — the whole thesis in one picture
// ===========================================================================

function hero() {
  const W = 1280;
  const H = 610;
  const id = "hero";

  let s = canvas(W, H, { id });

  // Only the wordmark gradient is hero-specific; the washes, the vignette and the top edge now
  // come from `canvas`, so all eight figures share one atmosphere rather than eight variations.
  s += `<defs><linearGradient id="${id}-word" x1="0" y1="0" x2="1" y2="0.4">
      <stop offset="0%" stop-color="#ffffff"/>
      <stop offset="52%" stop-color="${T.ink}"/>
      <stop offset="100%" stop-color="${T.cyan}"/>
    </linearGradient></defs>`;

  // --- Wordmark -----------------------------------------------------------
  s += text(64, 104, "Sakur4", { size: 64, weight: 700, fill: `url(#${id}-word)`, spacing: -2 });
  s += `<rect x="64" y="120" width="248" height="3" rx="1.5" fill="url(#${id}-rule)"/>`;
  s += `<defs><linearGradient id="${id}-rule" x1="0" y1="0" x2="1" y2="0">
    <stop offset="0%" stop-color="${T.cyan}"/><stop offset="70%" stop-color="${T.violet}"/><stop offset="100%" stop-color="${T.violet}" stop-opacity="0"/>
  </linearGradient></defs>`;

  // The version, set as a value rather than a claim — monospaced, because that is what it is.
  s += pill(504, 60, "v0.1.0", { fill: T.panel, stroke: T.panelEdge, ink: T.inkFaint });

  s += text(64, 162, "Cache-coherent memory and context for local coding agents", {
    size: 19,
    fill: T.inkDim,
  });
  s += text(64, 190, "Compaction that keeps the prompt prefix the inference server already holds.", {
    size: 19,
    fill: T.inkFaint,
  });

  // --- The core figure: three prompt layouts ------------------------------
  //
  // Every band below is a named constant, in the order it is drawn, with its
  // vertical pitch made explicit. The first version derived these positions and
  // three labels landed on top of each other; deriving layout from content is how
  // that happens, so the geometry is stated instead.
  const px = 64;
  const cw = 9.2;
  const gap = 1.6;
  const cells = 78;
  const railX = px + 76;
  const rowH = 26;
  const rightLabelX = railX + cells * (cw + gap) + 12;

  const panelTop = 228;
  const legendY = 252;
  const cells1Y = 280;
  const braceY = 312;
  const braceLabelY = 332;
  const decisionY = 364;
  const cells2Y = 382;
  const cells3Y = 428;
  const boundaryTop = 404;
  const panelBottom = cells3Y + rowH + 22;

  s += panel(64, panelTop, 1152, panelBottom - panelTop, { fill: T.bgAlt, stroke: T.panelEdge });

  // Legend
  let lx = px;
  for (const [colour, label] of [
    [T.green, "reused from cache"],
    [T.amber, "evicted & rewritten"],
    [T.blue, "new tail"],
    [T.red, "rewritten by a summary"],
  ]) {
    s += `<rect x="${lx}" y="${legendY - 11}" width="10" height="10" rx="2.5" fill="${colour}"/>`;
    s += text(lx + 17, legendY - 2, label, { size: 11.5, fill: T.inkDim });
    lx += label.length * 6.5 + 40;
  }

  // Row 1 — what the server holds.
  s += mono(px, cells1Y + rowH - 9, "turn N", { size: 12, fill: T.inkFaint });
  for (let i = 0; i < cells; i++) {
    const x = railX + i * (cw + gap);
    const keep = i < 26;
    s += `<rect x="${x.toFixed(1)}" y="${cells1Y}" width="${cw}" height="${rowH}" rx="2.5"
      fill="${keep ? T.green : T.red}" opacity="${keep ? 0.85 : 0.45}"
      stroke="${keep ? T.green : T.red}" stroke-width="0.7"/>`;
  }
  s += text(rightLabelX, cells1Y + 17, "the server's cache covers this", { size: 11, fill: T.green });

  // The shared head, bracketed, with its own label line below the bracket and the
  // decision label on the next line down.
  const bx1 = railX;
  const bx2 = railX + 26 * (cw + gap) - gap;
  s += `<path d="M${bx1} ${cells1Y + rowH} L${bx1} ${braceY} L${bx2} ${braceY} L${bx2} ${cells1Y + rowH}"
    fill="none" stroke="${T.green}" stroke-width="1.4"/>`;
  s += text((bx1 + bx2) / 2, braceLabelY, "4034 tokens — byte-identical in both prompts", {
    size: 11,
    fill: T.green,
    anchor: "middle",
    family: MONO,
  });

  // Row 2 — what a summarising compaction turns it into.
  s += text(railX, decisionY, "after a summarising compaction — nothing matches", {
    size: 11.5,
    fill: T.red,
  });
  s += mono(px, cells2Y + rowH - 9, "turn N+1", { size: 12, fill: T.inkFaint });
  for (let i = 0; i < cells; i++) {
    const x = railX + i * (cw + gap);
    s += `<rect x="${x.toFixed(1)}" y="${cells2Y}" width="${cw}" height="${rowH}" rx="2.5"
      fill="${T.red}" opacity="0.14" stroke="${T.red}" stroke-width="0.6" stroke-dasharray="2 2"/>`;
  }
  s += text(rightLabelX, cells2Y + 17, "full re-prefill", { size: 11, fill: T.red });

  // Row 3 — what Sakur4 sends.
  s += text(railX, cells3Y - 10, "evict after a checkpoint the server can rewind to", {
    size: 11.5,
    fill: T.green,
  });
  s += mono(px, cells3Y + rowH - 9, "Sakur4", { size: 12, fill: T.cyan, weight: 700 });
  for (let i = 0; i < cells; i++) {
    const x = railX + i * (cw + gap);
    const [fill, stroke, opacity] =
      i < 26
        ? [T.green, T.green, 0.85]
        : i < 44
          ? [T.amber, T.amber, 0.55]
          : [T.blue, T.blue, 0.5];
    s += `<rect x="${x.toFixed(1)}" y="${cells3Y}" width="${cw}" height="${rowH}" rx="2.5"
      fill="${fill}" opacity="${opacity}" stroke="${stroke}" stroke-width="0.7"/>`;
  }
  // The boundary marker runs from the label band above the row to below it, so the
  // dashed line reads as passing *through* the cut rather than sitting beside it.
  s += boundaryMarker(bx2, boundaryTop, cells3Y + rowH + 12, "boundary", T.cyan);

  // --- Result strip -------------------------------------------------------
  const ry = 536;
  s += panel(64, ry - 34, 1152, 62, { fill: T.panel, stroke: T.cyanDim, glow: T.cyan });

  const stats = [
    ["4034", "tokens reused from the cache"],
    ["13080", "tokens prefilled"],
    ["24%", "of the prefill avoided"],
    ["7812", "tokens reclaimed by eviction"],
  ];
  let sx = px + 26;
  for (const [big, small] of stats) {
    s += text(sx, ry + 2, big, { size: 30, weight: 700, fill: T.cyan, family: MONO });
    s += text(sx, ry + 20, small, { size: 11, fill: T.inkFaint });
    sx += 290;
  }

  return svg(W, H, s, {
    title: "Sakur4 keeps the prompt prefix the inference server already holds",
    desc:
      "Three prompt layouts compared. A summarising compaction rewrites the prefix, so the server's cache matches nothing and the whole context is re-prefilled. Sakur4 evicts after a checkpoint the server can rewind to, so 4034 tokens stay byte-identical and 24 percent of the prefill is avoided.",
  });
}

// ===========================================================================
// 2 · Architecture
// ===========================================================================

function architecture() {
  const W = 1280;
  const H = 1060;
  const id = "arch";
  let s = canvas(W, H, { id });

  s += text(64, 62, "Architecture", { size: 26, weight: 700, fill: T.ink });
  s += text(64, 88, "Sakur4 is a subsystem, not a harness. It never calls a model and never owns the agent loop.", {
    size: 13,
    fill: T.inkFaint,
  });

  // --- Vertical bands, stated once. Three earlier versions derived these and
  // something collided every time; explicit constants are the fix.
  const harnessTop = 148;
  const railY = 446;
  const daemonTop = 120;
  const storageTop = 560;
  const backendTop = 762;

  const hh = 62;
  const hgap = 12;

  s += text(64, harnessTop - 14, "HARNESSES", { size: 11, weight: 700, fill: T.inkFaint, spacing: 1.6 });

  const harnesses = [
    ["Hermes Agent", "MCP · stdio", T.cyan],
    ["Claude Code / Desktop", "MCP · stdio + HTTP", T.cyan],
    ["Oh My Pi", "native extension", T.violet],
    ["Any Agent Skills host", "skill + CLI", T.green],
  ];
  const boxY = harnesses.map((_, i) => harnessTop + i * (hh + hgap));
  harnesses.forEach(([name, how, colour], i) => {
    s += panel(64, boxY[i], 268, hh, { fill: T.panel, stroke: T.panelEdge });
    s += `<rect x="64" y="${boxY[i]}" width="4" height="${hh}" rx="2" fill="${colour}"/>`;
    s += text(84, boxY[i] + 27, name, { size: 13.5, weight: 600, fill: T.ink });
    s += mono(84, boxY[i] + 47, how, { size: 11, fill: colour });
  });

  // --- The MCP path, drawn only from the two harnesses that use it. --------
  const gx = 452;
  const gy = boxY[0];
  const gw = 190;
  const gh = boxY[1] + hh - boxY[0];

  s += panel(gx, gy, gw, gh, { fill: T.bgAlt, stroke: T.cyanDim });
  s += text(gx + gw / 2, gy + 32, "MCP", { size: 15, weight: 700, fill: T.cyan, anchor: "middle" });
  s += text(gx + gw / 2, gy + 52, "revision 2026-07-28", {
    size: 10,
    fill: T.inkFaint,
    anchor: "middle",
    family: MONO,
  });
  s += `<line x1="${gx + 24}" y1="${gy + 68}" x2="${gx + gw - 24}" y2="${gy + 68}" stroke="${T.panelEdge}"/>`;
  s += text(gx + gw / 2, gy + 92, "17 tools", { size: 12, fill: T.inkDim, anchor: "middle" });
  s += text(gx + gw / 2, gy + 110, "4 resources", { size: 12, fill: T.inkDim, anchor: "middle" });
  s += text(gx + gw / 2, gy + 128, "1 prompt", { size: 12, fill: T.inkDim, anchor: "middle" });
  s += `<line x1="${gx + 24}" y1="${gy + 146}" x2="${gx + gw - 24}" y2="${gy + 146}" stroke="${T.panelEdge}"/>`;
  s += text(gx + gw / 2, gy + 170, "stdio or HTTP", { size: 10.5, fill: T.inkFaint, anchor: "middle" });

  s += arrow(332, boxY[0] + hh / 2, gx - 8, boxY[0] + hh / 2, { color: T.cyan });
  s += arrow(332, boxY[1] + hh / 2, gx - 8, boxY[1] + hh / 2, { color: T.cyan });

  // --- The non-MCP rail, in the clear band below the harness column. -------
  // Terminates at x=430 and turns up into the daemon, so it never crosses the
  // gateway box or any harness.
  s += `<line x1="64" y1="${railY}" x2="404" y2="${railY}" stroke="${T.panelEdge}" stroke-width="1" stroke-dasharray="6 5"/>`;
  s += pill(64, railY - 11, "NO MCP CLIENT", { fill: T.bg, stroke: T.inkFaint, ink: T.inkFaint, size: 9.5, h: 22 });
  s += arrow(196, boxY[3] + hh, 196, railY - 6, { color: T.green, dash: "5 4", width: 1.4 });
  s += text(232, railY - 26, "reaches the daemon directly —", { size: 11, fill: T.inkFaint });
  s += text(232, railY + 20, "a native extension, or a spawned process", { size: 11, fill: T.inkFaint });
  s += arrow(404, railY, 596, railY, { color: T.green, dash: "5 4", width: 1.4 });
  s += arrow(656, railY, 656, 700, { color: T.cyan });

  // --- The daemon ---------------------------------------------------------
  const dx = 676;
  const dw = 540;
  const comps = [
    ["C1", "Memory Fabric", "episodic · symbolic · semantic · anchors", T.violet],
    ["C2", "Graduated Eviction", "four tiers · dependency-aware", T.amber],
    ["C3", "Cache Coherence", "checkpoint-aligned boundaries", T.cyan],
    ["C4", "Repo Cortex", "tree-sitter · call graph", T.blue],
    ["C5", "Hybrid Recall", "BM25 + dense + graph", T.green],
    ["C6", "Idle Consolidator", "promote · regenerate · archive", T.inkDim],
    ["C7", "MCP Gateway", "tools · resources · prompt", T.cyan],
    ["C8", "Ledger Receipt", "tokens · cache · provider cache", T.cyan],
  ];
  const dh = 44 + comps.length * 46 + 16;

  s += panel(dx, daemonTop, dw, dh, { fill: T.bgAlt, stroke: T.cyanDim, glow: T.cyan });
  s += mono(dx + 22, daemonTop + 30, "sakur4d", { size: 17, fill: T.ink, weight: 700 });
  s += text(dx + 122, daemonTop + 30, "one binary, no daemon to babysit", { size: 11, fill: T.inkFaint });

  let cy = daemonTop + 44;
  for (const [code, name, detail, colour] of comps) {
    s += panel(dx + 18, cy, dw - 36, 38, { fill: T.panel, stroke: T.panelEdge, r: 9 });
    s += `<rect x="${dx + 18}" y="${cy}" width="3" height="38" rx="1.5" fill="${colour}"/>`;
    s += mono(dx + 34, cy + 17, code, { size: 10.5, fill: colour, weight: 700 });
    s += text(dx + 66, cy + 17, name, { size: 12.5, weight: 600, fill: T.ink });
    s += text(dx + 66, cy + 31, detail, { size: 10.5, fill: T.inkFaint });
    cy += 46;
  }

  // --- Storage, in the clear band below the rail. -------------------------
  s += text(64, storageTop, "STORAGE", { size: 11, weight: 700, fill: T.inkFaint, spacing: 1.6 });

  const stores = [
    ["SQLite · WAL", "episodes · facts · atlas", "anchors · dependency graph"],
    ["FTS5", "lexical index (BM25)", "external-content, trigger-maintained"],
    ["vectors", "exact cosine scan", "or sqlite-vec when loaded"],
    ["snapshots", "slot KV state on disk", "bounded by a retention policy"],
  ];
  stores.forEach(([name, line1, line2], i) => {
    const bx = 64 + (i % 2) * 278;
    const by = storageTop + 16 + Math.floor(i / 2) * 86;
    s += panel(bx, by, 266, 76, { fill: T.panel, stroke: T.panelEdge });
    s += text(bx + 16, by + 27, name, { size: 12.5, weight: 600, fill: T.ink });
    s += text(bx + 16, by + 46, line1, { size: 10.5, fill: T.inkFaint });
    s += text(bx + 16, by + 62, line2, { size: 10.5, fill: T.inkFaint });
  });

  // --- Inference backend --------------------------------------------------
  s += text(64, backendTop, "INFERENCE BACKEND — probed, never assumed", {
    size: 11,
    weight: 700,
    fill: T.inkFaint,
    spacing: 1.2,
  });
  s += text(
    64,
    backendTop + 20,
    "The PRD's risk register notes that llama.cpp's slot API is a moving target, so Sakur4 records what it found and routes every cache decision through that.",
    { size: 11, fill: T.inkFaint },
  );

  const backends = [
    ["llama.cpp", "/slots · /save · /restore", "full coherence", T.green],
    ["embedded", "simulates a checkpoint ring", "no GPU needed", T.cyan],
    ["none", "coherence disabled, logged", "always correct", T.inkDim],
  ];
  backends.forEach(([name, detail, verdict, colour], i) => {
    const bx = 64 + i * 184;
    s += panel(bx, backendTop + 36, 172, 78, { fill: T.panel, stroke: T.panelEdge });
    s += `<rect x="${bx}" y="${backendTop + 36}" width="3" height="78" rx="1.5" fill="${colour}"/>`;
    s += mono(bx + 16, backendTop + 64, name, { size: 13, fill: T.ink, weight: 600 });
    s += text(bx + 16, backendTop + 84, detail, { size: 10, fill: T.inkFaint });
    s += text(bx + 16, backendTop + 102, verdict, { size: 11, fill: colour });
  });

  return svg(W, H, s, {
    title: "Sakur4 architecture",
    desc:
      "Two harnesses reach Sakur4 over MCP; two have no MCP client and reach the daemon directly. The daemon holds eight components over a SQLite store, and probes its inference backend rather than assuming its capabilities.",
  });
}

// ===========================================================================
// 3 · The cache-coherence decision, in detail
// ===========================================================================

function coherence() {
  const W = 1280;
  const H = 560;
  const id = "coh";
  let s = canvas(W, H, { id });

  s += text(64, 60, "The eviction boundary is chosen before the eviction", {
    size: 25,
    weight: 700,
    fill: T.ink,
  });
  s += text(64, 86, "Order of operations is the whole design. Get it wrong and every compaction silently reports a full re-prefill.", {
    size: 13,
    fill: T.inkFaint,
  });

  // --- Wrong order --------------------------------------------------------
  const wy = 130;
  s += panel(64, wy, 552, 176, { fill: T.bgAlt, stroke: T.redDim });
  s += pill(84, wy + 18, "WRONG ORDER", { fill: "none", stroke: T.red, ink: T.red, size: 10 });
  s += text(84, wy + 66, "1.  decide what to evict", { size: 13, fill: T.ink });
  s += text(84, wy + 92, "2.  ask the cache where it can cut", { size: 13, fill: T.ink });
  s += arrow(112, wy + 108, 112, wy + 128, { color: T.red, width: 1.4 });
  s += text(126, wy + 124, "the plan already rewrote token 0", { size: 11, fill: T.red });
  s += text(84, wy + 152, "boundary = 0  →  no checkpoint aligns  →  full re-prefill", {
    size: 11.5,
    fill: T.red,
    family: MONO,
  });

  // --- Right order --------------------------------------------------------
  s += panel(664, wy, 552, 176, { fill: T.bgAlt, stroke: T.greenDim, glow: T.green });
  s += pill(684, wy + 18, "THE DESIGN", { fill: "none", stroke: T.green, ink: T.green, size: 10 });
  s += text(684, wy + 66, "1.  ask the cache where the boundary can fall", { size: 13, fill: T.ink });
  s += text(684, wy + 92, "2.  evict after it", { size: 13, fill: T.ink });
  s += arrow(712, wy + 108, 712, wy + 128, { color: T.green, width: 1.4 });
  s += text(726, wy + 124, "the surviving head stays LCP-matchable", { size: 11, fill: T.green });
  s += text(684, wy + 152, "boundary = 4034  →  snapped to a checkpoint  →  partial reuse", {
    size: 11.5,
    fill: T.green,
    family: MONO,
  });

  // --- The four verdicts --------------------------------------------------
  const vy = 348;
  s += text(64, vy - 12, "EVERY PLAN ENDS IN ONE OF THESE", { size: 11, weight: 700, fill: T.inkFaint, spacing: 1.4 });

  const verdicts = [
    ["aligned", "the prefix is preserved exactly at a checkpoint", T.green],
    ["snapped", "moved back onto an older checkpoint, within tolerance", T.cyan],
    ["partial-reuse", "some prefix survives; the rest is prefilled", T.amber],
    ["full-re-prefill", "nothing survived — reported, with the reason", T.red],
  ];
  let vx = 64;
  const vw = 288;
  for (const [name, detail, colour] of verdicts) {
    s += panel(vx, vy, vw, 92, { fill: T.panel, stroke: T.panelEdge });
    s += `<circle cx="${vx + 24}" cy="${vy + 26}" r="5" fill="${colour}"/>`;
    s += mono(vx + 40, vy + 31, name, { size: 13, fill: colour, weight: 700 });
    s += paragraph(vx + 20, vy + 58, detail, { maxChars: 40, size: 11, fill: T.inkDim, leading: 15 });
    vx += vw + 16;
  }

  // --- The fallback promise ----------------------------------------------
  s += panel(64, vy + 116, 1152, 58, { fill: T.panel, stroke: T.panelEdge });
  s += text(88, vy + 150, "The fallback is first class:", { size: 13, weight: 600, fill: T.ink });
  s += text(
    268,
    vy + 150,
    "with no server, an older build, or a sliding-window model, the plan is still produced, still evicts, and says why alignment was impossible.",
    { size: 12.5, fill: T.inkDim },
  );

  return svg(W, H, s, {
    title: "Choosing the eviction boundary from the cache first",
    desc:
      "Evicting before consulting the cache produces a boundary at token zero, which no checkpoint can align to. Asking the cache first and evicting after it preserves a reusable prefix. Every plan ends in one of four reported verdicts, and the fallback path always produces a correct plan.",
  });
}

// ===========================================================================
// 4 · The dual-track memory model
// ===========================================================================

function dualTrack() {
  const W = 1280;
  const H = 660;
  const id = "dual";
  let s = canvas(W, H, { id });

  s += text(64, 60, "Two tracks, and no path from a model into the first", {
    size: 25,
    weight: 700,
    fill: T.ink,
  });
  s += text(64, 86, "This is what makes a stale answer detectable instead of merely unlikely.", {
    size: 13,
    fill: T.inkFaint,
  });

  // --- Source -------------------------------------------------------------
  const sy = 130;
  s += panel(64, sy, 1152, 74, { fill: T.bgAlt, stroke: T.panelEdge });
  s += text(88, sy + 30, "A tool result arrives", { size: 13, weight: 600, fill: T.ink });
  s += mono(88, sy + 52, '{ "user": { "id": 41, "email": "a@b.c" } }', { size: 11.5, fill: T.inkFaint });
  s += pill(1000, sy + 24, "verbatim, append-only", { fill: "none", stroke: T.inkFaint, ink: T.inkFaint, size: 10 });

  // --- Split --------------------------------------------------------------
  const ty = 250;
  const colW = 552;

  // Left: symbolic
  s += arrow(340, sy + 80, 340, ty - 12, { color: T.green, above: "deterministic parse" });
  s += panel(64, ty, colW, 250, { fill: T.panel, stroke: T.greenDim });
  s += text(88, ty + 34, "Symbolic Ledger", { size: 16, weight: 700, fill: T.green });
  s += text(88, ty + 56, "one constructor, and it demands a FactSource", { size: 11, fill: T.inkFaint });
  s += `<line x1="88" y1="${ty + 72}" x2="${64 + colW - 24}" y2="${ty + 72}" stroke="${T.panelEdge}"/>`;

  const facts = [
    ["json_field", "user.id", "41"],
    ["json_field", "user.email", "a@b.c"],
  ];
  let fy = ty + 92;
  for (const [kind, path, value] of facts) {
    s += mono(88, fy, kind, { size: 10.5, fill: T.green });
    s += mono(186, fy, path, { size: 11, fill: T.ink });
    s += mono(360, fy, `= ${value}`, { size: 11, fill: T.inkDim });
    fy += 22;
  }
  s += `<line x1="88" y1="${fy + 6}" x2="${64 + colW - 24}" y2="${fy + 6}" stroke="${T.panelEdge}"/>`;
  s += text(88, fy + 32, "No model can write here. The type system says so,", { size: 11.5, fill: T.inkDim });
  s += text(88, fy + 50, "not a code-review convention.", { size: 11.5, fill: T.inkDim });
  s += pill(88, fy + 64, "cannot hallucinate", { fill: "none", stroke: T.green, ink: T.green, size: 10 });

  // Right: semantic
  s += arrow(940, sy + 80, 940, ty - 12, { color: T.violet, above: "model interpretation" });
  s += panel(664, ty, colW, 250, { fill: T.panel, stroke: T.violetDim });
  s += text(688, ty + 34, "Semantic Atlas", { size: 16, weight: 700, fill: T.violet });
  s += text(688, ty + 56, "anchoring is mandatory, not optional", { size: 11, fill: T.inkFaint });
  s += `<line x1="688" y1="${ty + 72}" x2="${664 + colW - 24}" y2="${ty + 72}" stroke="${T.panelEdge}"/>`;

  s += text(688, ty + 100, '"checkUser looks a user up by email"', { size: 12, fill: T.ink });
  s += mono(688, ty + 124, "anchored to → sym_01a096a5…", { size: 10.5, fill: T.violet });
  s += `<line x1="688" y1="${ty + 144}" x2="${664 + colW - 24}" y2="${ty + 144}" stroke="${T.panelEdge}"/>`;
  s += text(688, ty + 168, "The anchor's hash is stored alongside. Staleness is", { size: 11.5, fill: T.inkDim });
  s += text(688, ty + 186, "computed when the entry is read — never cached.", { size: 11.5, fill: T.inkDim });
  s += pill(688, ty + 200, "cannot silently drift", { fill: "none", stroke: T.violet, ink: T.violet, size: 10 });

  // --- Read-time resolution ----------------------------------------------
  const ry = 556;
  s += text(64, ry - 14, "LATER — THE SOURCE CHANGED AND NOBODY TOLD THE ATLAS", {
    size: 11,
    weight: 700,
    fill: T.inkFaint,
    spacing: 1.2,
  });
  s += panel(64, ry, 1152, 68, { fill: T.bgAlt, stroke: T.amberDim });
  s += mono(88, ry + 28, "read-time check", { size: 11, fill: T.amber });
  s += text(232, ry + 28, "stored hash ≠ current hash", { size: 12, fill: T.ink });
  s += arrow(430, ry + 23, 480, ry + 23, { color: T.amber, width: 1.4 });
  s += text(496, ry + 28, "the recall result is returned flagged STALE, carrying the anchor's current value", {
    size: 12,
    fill: T.inkDim,
  });
  s += mono(88, ry + 50, "→ trust the current value, never the summary.", { size: 11.5, fill: T.green });

  return svg(W, H, s, {
    title: "The dual-track memory model",
    desc:
      "A tool result is stored verbatim, then split: deterministic parsers write facts to the Symbolic Ledger, which no model can write to, while model interpretation goes to the Semantic Atlas with a mandatory anchor. Staleness is computed at read time by comparing the anchor's stored hash with its current one.",
  });
}

// ===========================================================================
// 5 · A turn, end to end
// ===========================================================================

function turnLifecycle() {
  const W = 1280;
  const H = 830;
  const id = "life";
  let s = canvas(W, H, { id });

  s += text(64, 58, "What happens in one turn", { size: 25, weight: 700, fill: T.ink });
  s += text(64, 84, "Everything here is optional. With no daemon reachable, the harness behaves exactly as it did before.", {
    size: 13,
    fill: T.inkFaint,
  });

  const cx = 118;
  const steps = [
    ["session start", "probe the daemon once; report a missing binary before ten turns go unrecorded", T.cyan],
    ["before turn", "assemble the prompt; ask the cache where the eviction boundary may fall", T.cyan],
    ["retrieve", "hybrid recall, staleness-checked, prepended and capped — its own cost recorded", T.green],
    ["model", "the harness calls the model. Sakur4 is not in this path and cannot be", T.inkDim],
    ["usage", "the provider's cached-token counts are forwarded; the verdict is computed", T.amber],
    ["commit", "the turn is appended. Tool results are parsed into facts, text stays verbatim", T.violet],
    ["pin?", "a stated constraint is proposed for pinning — proposed, never auto-pinned", T.violet],
    ["compact", "past the trigger: evict after a checkpoint, preserve the prefix, report the cost", T.cyan],
    ["session end", "flush; report stale summaries, because the next session inherits them", T.inkDim],
  ];

  const rowH = 44;
  const pitch = 56;
  const y0 = 122;
  const rowW = 1152;

  steps.forEach(([name, detail, colour], i) => {
    const y = y0 + i * pitch;
    if (i < steps.length - 1) {
      s += `<line x1="${cx}" y1="${y + rowH / 2 + 15}" x2="${cx}" y2="${y + pitch + rowH / 2 - 15}" stroke="${T.panelEdge}" stroke-width="1.4"/>`;
    }
    s += `<circle cx="${cx}" cy="${y + rowH / 2}" r="14" fill="${T.bg}" stroke="${colour}" stroke-width="1.6"/>`;
    s += mono(cx, y + rowH / 2 + 5, String(i + 1), { size: 12.5, fill: colour, anchor: "middle", weight: 700 });
    s += panel(cx + 38, y, rowW, rowH, { fill: T.panel, stroke: T.panelEdge, r: 9 });
    s += `<rect x="${cx + 38}" y="${y}" width="3" height="${rowH}" rx="1.5" fill="${colour}"/>`;
    s += mono(cx + 60, y + 27, name, { size: 12.5, fill: T.ink, weight: 600 });
    s += text(cx + 206, y + 27, detail, { size: 11.5, fill: T.inkFaint });
  });

  // The offline promise, as a full-width strip below the steps. An earlier
  // version put it in a side column, where it ran off the right edge because the
  // step rows already occupy almost the whole width.
  const stripY = y0 + steps.length * pitch + 12;
  s += panel(64, stripY, 1152, 68, { fill: T.bgAlt, stroke: T.greenDim });
  s += text(88, stripY + 30, "No daemon?", { size: 13.5, weight: 700, fill: T.green });
  s += text(
    200,
    stripY + 30,
    "Every hook returns nothing and the harness runs exactly as if Sakur4 were not installed.",
    { size: 12.5, fill: T.inkDim },
  );
  s += text(
    88,
    stripY + 52,
    "A sidecar that breaks your session when it is down is worse than no sidecar at all.",
    { size: 12, fill: T.inkFaint },
  );

  return svg(W, H, s, {
    title: "One turn, end to end",
    desc:
      "Nine steps across a session: probe, assemble and plan the boundary, retrieve, call the model, report usage, commit, propose pins, compact at the trigger, and flush. Each is optional, and with no daemon the harness behaves as if Sakur4 were absent.",
  });
}

// ===========================================================================
// 8 · Why prefix preservation is the whole game
// ===========================================================================

function prefillCost() {
  const W = 1280;
  const H = 520;
  const id = "cost";
  let s = canvas(W, H, { id });

  s += text(64, 60, "Why the prefix is the whole game", { size: 25, weight: 700, fill: T.ink });
  s += text(64, 86, "Prompt processing is not incremental. A cache hit skips work; a miss repeats it, at full price.", {
    size: 13,
    fill: T.inkFaint,
  });

  // Two bars: the same session, compacted two ways.
  const bars = [
    ["Summarising compaction", 100, T.red, "every token reprocessed from scratch"],
    ["Sakur4 eviction", 76, T.green, "the preserved head is skipped entirely"],
  ];

  const bx = 64;
  const bw = 900;
  const by0 = 148;
  const barH = 52;
  const pitch = 108;

  bars.forEach(([name, pct, colour, note], i) => {
    const y = by0 + i * pitch;
    s += text(bx, y - 12, name, { size: 13.5, weight: 600, fill: T.ink });
    s += text(bx + bw + 16, y - 12, `${pct}% of the prompt processed`, { size: 11.5, fill: colour });

    s += `<rect x="${bx}" y="${y}" width="${bw}" height="${barH}" rx="6" fill="${T.panel}" stroke="${T.panelEdge}"/>`;
    s += `<rect x="${bx}" y="${y}" width="${(bw * pct) / 100}" height="${barH}" rx="6" fill="${colour}" opacity="0.28" stroke="${colour}" stroke-width="1"/>`;
    s += text(bx + 16, y + barH / 2 + 5, note, { size: 12, fill: T.ink });
  });

  // The measured detail, from `sakur4d demo`.
  const dy = 380;
  s += panel(64, dy, 1152, 100, { fill: T.bgAlt, stroke: T.panelEdge });
  s += text(88, dy + 28, "Measured on a 32K-window session", { size: 12, weight: 600, fill: T.inkDim });

  const cells = [
    ["4034", "tokens reused", T.green],
    ["13080", "tokens prefilled", T.amber],
    ["24%", "prefill avoided", T.cyan],
  ];
  let cx = 88;
  for (const [big, small, colour] of cells) {
    s += mono(cx, dy + 68, big, { size: 24, fill: colour, weight: 700 });
    s += text(cx + big.length * 15 + 10, dy + 66, small, { size: 11.5, fill: T.inkFaint });
    cx += 300;
  }
  s += text(88, dy + 90, "Produced by `sakur4d demo`; the receipt prints in full with `context.receipt`.", {
    size: 10.5,
    fill: T.inkFaint,
  });

  return svg(W, H, s, {
    title: "Why prefix preservation determines prefill cost",
    desc:
      "A summarising compaction rewrites the prompt so the server's cache matches nothing and the whole context is processed again. Sakur4's eviction preserves a byte-identical prefix, so that portion is skipped: of 17114 prompt tokens, 4034 were reused and 13080 prefilled, avoiding 24 percent of the work.",
  });
}

// ===========================================================================
// 9 · Repository map
// ===========================================================================

function repoMap() {
  const W = 1280;
  const H = 480;
  const id = "map";
  let s = canvas(W, H, { id });

  s += text(64, 56, "Where everything lives", { size: 23, weight: 700, fill: T.ink });
  s += text(64, 80, "Three Rust crates, plus the harness integrations and the portable skill.", {
    size: 12.5,
    fill: T.inkFaint,
  });

  const groups = [
    [
      "crates/sakur4-core/",
      "the engine — no transport, no MCP",
      [
        ["memory/", "C1 · dual-track fabric"],
        ["evict.rs", "C2 · four tiers, fold/unfold"],
        ["cache/", "C3 · coherence layer"],
        ["llama/", "C3 · backend trait + adapters"],
        ["repo.rs", "C4 · tree-sitter cortex"],
        ["recall.rs", "C5 · hybrid retrieval"],
        ["consolidate.rs", "C6 · idle consolidator"],
        ["receipt.rs", "C8 · ledger receipt"],
        ["provider_cache.rs", "C8 · hosted-provider caching"],
        ["prompt.rs", "the one prompt assembler"],
        ["tokens.rs", "the one tokenizer"],
        ["store/", "schema · WAL · FTS5 · vectors"],
      ],
      T.cyan,
    ],
    [
      "crates/sakur4d/",
      "the daemon — CLI + MCP gateway",
      [
        ["tools.rs", "17 tools · 4 resources · 1 prompt"],
        ["gateway.rs", "stdio + streamable HTTP"],
        ["cli.rs", "every command, and `config`"],
        ["main.rs", "arg parsing and dispatch"],
      ],
      T.violet,
    ],
    [
      "integrations/ · skills/",
      "harness integration",
      [
        ["omp-plugin/", "native Oh My Pi extension"],
        ["  index.ts", "9 tools · 7 hooks"],
        ["  install.mjs", "no-symlink installer"],
        ["skills/sakur4/", "portable Agent Skill"],
        ["  scripts/", "dependency-free Node CLI"],
        ["sakur4-testkit/", "fake llama.cpp server"],
      ],
      T.green,
    ],
  ];

  const rowPitch = 26;
  let gx = 64;
  const gw = 376;
  for (const [title, subtitle, items, colour] of groups) {
    // The panel is sized from the items it holds. Sizing it from a constant is how
    // the first version clipped its own last row.
    const firstRowY = 186;
    const panelH = firstRowY - 104 + items.length * rowPitch + 6;
    s += panel(gx, 104, gw, panelH, { fill: T.bgAlt, stroke: T.panelEdge });
    s += `<rect x="${gx}" y="104" width="${gw}" height="3" rx="1.5" fill="${colour}"/>`;
    s += mono(gx + 20, 138, title, { size: 13, fill: T.ink, weight: 700 });
    s += text(gx + 20, 158, subtitle, { size: 11, fill: T.inkFaint });
    items.forEach(([name, detail], i) => {
      const iy = firstRowY + i * rowPitch;
      s += mono(gx + 20, iy, name, { size: 10.5, fill: colour });
      s += text(gx + 20 + name.length * 6.4 + 10, iy, detail, { size: 10, fill: T.inkFaint });
    });
    gx += gw + 12;
  }

  return svg(W, H, s, {
    title: "Repository layout",
    desc:
      "Three Rust crates — the engine, the daemon and a test kit — plus harness integrations and a portable skill package.",
  });
}

// ===========================================================================
// 7 · Verification status
// ===========================================================================

function verification() {
  const W = 1280;
  const H = 400;
  const id = "ver";
  let s = canvas(W, H, { id });

  s += text(64, 56, "What is verified, and what is not", {
    size: 23,
    weight: 700,
    fill: T.ink,
  });
  s += text(64, 80, "A release claim is worth exactly as much as the evidence behind it.", {
    size: 12.5,
    fill: T.inkFaint,
  });

  const cols = [
    [
      "VERIFIED",
      T.green,
      [
        "224 tests, workspace-wide, green",
        "MCP over stdio — real binary, real pipes",
        "MCP over HTTP — SDK client, live listener",
        "Hermes discovers all 17 tools",
        "OMP loads 9 tools; a live model called one",
        "Restart mid-session preserves the store",
        "Malformed input rejected, session survives",
        "Zero unwrap/expect/panic outside tests",
        "cargo publish dry-run builds standalone",
      ],
    ],
    [
      "NOT VERIFIED",
      T.amber,
      [
        "No real llama.cpp server contacted",
        "Hermes has not been driven by a live model",
        "OMP compaction hook never fired for real",
        "No MCP conformance run against a client",
        "No LoCoMo / Endurance benchmark run",
        "NFR latency and memory numbers unmeasured",
        "Encryption at rest (FR-20) not implemented",
        "cargo deny / cargo audit not in CI",
        "No signed release artifacts",
      ],
    ],
  ];

  let cx = 64;
  const cw = 576;
  for (const [title, colour, items] of cols) {
    s += panel(cx, 104, cw, 264, { fill: T.bgAlt, stroke: T.panelEdge });
    s += `<rect x="${cx}" y="104" width="${cw}" height="3" rx="1.5" fill="${colour}"/>`;
    s += text(cx + 22, 140, title, { size: 12, weight: 700, fill: colour, spacing: 1.6 });
    let iy = 172;
    for (const item of items) {
      s += `<circle cx="${cx + 28}" cy="${iy - 4}" r="3" fill="${colour}" opacity="0.85"/>`;
      s += text(cx + 42, iy, item, { size: 11.5, fill: T.inkDim });
      iy += 21;
    }
    cx += cw + 16;
  }

  return svg(W, H, s, {
    title: "Verification status",
    desc:
      "Nine verified behaviours including the test suite, both MCP transports, and live harness discovery, against nine unverified items including a real llama.cpp server, a live Hermes session, and the OMP compaction hook.",
  });
}

// ===========================================================================
// Run
// ===========================================================================

process.stdout.write("generating figures:\n");
write("hero.svg", hero());
write("architecture.svg", architecture());
write("coherence.svg", coherence());
write("dual-track.svg", dualTrack());
write("turn-lifecycle.svg", turnLifecycle());
write("prefill-cost.svg", prefillCost());
write("repo-map.svg", repoMap());
write("verification.svg", verification());
process.stdout.write("done.\n");
