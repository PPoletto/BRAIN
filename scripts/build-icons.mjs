#!/usr/bin/env node
// Render `src-tauri/icons/brain-source.svg` into all PNG / ICO / ICNS files
// the Tauri bundler + tray expect.
//
// The Tauri bundler regenerates platform-specific icons (.icns, .ico) from
// `pnpm tauri icon icon.png` — so this script writes the master `icon.png`
// at 1024×1024, the in-window assets (`32x32.png`, `128x128.png`,
// `128x128@2x.png`), and the four tray-state PNGs (idle/busy/error/disconnected).
//
// Run: `pnpm icons` (added as an npm script).

import { readFile, writeFile, mkdir } from "node:fs/promises";
import { fileURLToPath } from "node:url";
import path from "node:path";
import sharp from "sharp";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const repoRoot = path.resolve(__dirname, "..");
const iconsDir = path.join(repoRoot, "src-tauri", "icons");
const sourcePath = path.join(iconsDir, "brain-source.svg");

const tints = {
  idle: { fill: "#34d399", stroke: "#065f46" }, // green
  busy: { fill: "#fbbf24", stroke: "#78350f" }, // amber
  error: { fill: "#f87171", stroke: "#7f1d1d" }, // red
  disconnected: { fill: "#9ca3af", stroke: "#374151" }, // grey
};

/// Returns a tinted SVG variant by swapping the two gradient stops and the
/// stroke colours. The source uses a single linear gradient with two stops
/// — we just rewrite the SVG text since regex is plenty for a 60-line file.
function tintedSvg(svg, fill, stroke) {
  return svg
    .replace(/stop-color="#34d399"/g, `stop-color="${fill}"`)
    .replace(/stop-color="#0d9488"/g, `stop-color="${stroke}"`)
    .replace(/stroke="#0f766e"/g, `stroke="${stroke}"`)
    .replace(/stroke="#065f46"/g, `stroke="${stroke}"`);
}

/// The tray icons need a transparent background; the app icon keeps the
/// dark rounded-square background so it pops in light-mode launchers.
function transparentBg(svg) {
  return svg.replace(
    /<rect[^>]*fill="#0a0a0a"[^>]*\/>/,
    "",
  );
}

async function renderPng(svg, size, outPath) {
  const buf = Buffer.from(svg);
  await sharp(buf, { density: 300 })
    .resize(size, size)
    .png()
    .toFile(outPath);
  console.log(`  wrote ${path.relative(repoRoot, outPath)}  (${size}×${size})`);
}

async function main() {
  await mkdir(iconsDir, { recursive: true });
  const baseSvg = await readFile(sourcePath, "utf8");

  console.log("App icons (rounded square background):");
  await renderPng(baseSvg, 1024, path.join(iconsDir, "icon.png"));
  await renderPng(baseSvg, 32, path.join(iconsDir, "32x32.png"));
  await renderPng(baseSvg, 128, path.join(iconsDir, "128x128.png"));
  await renderPng(baseSvg, 256, path.join(iconsDir, "128x128@2x.png"));

  console.log("Tray icons (transparent background, per-state tint):");
  const traySvg = transparentBg(baseSvg);
  for (const [state, { fill, stroke }] of Object.entries(tints)) {
    const tinted = tintedSvg(traySvg, fill, stroke);
    await renderPng(tinted, 64, path.join(iconsDir, `tray-${state}.png`));
  }

  // Windows-bundled .ico — sharp can't write multi-resolution ICO directly
  // but Tauri's bundler regenerates it from icon.png at build time. Same
  // for icon.icns on macOS. We document the manual fallback in the script
  // header.
  console.log(
    "\nDone. To regenerate platform-bundled .ico/.icns from icon.png run:",
  );
  console.log("    pnpm tauri icon src-tauri/icons/icon.png");
}

main().catch((err) => {
  console.error(err);
  process.exit(1);
});

// Silence the "writeFile imported but unused" linter — kept around so a
// future enhancement (e.g. emitting brain-source-tinted.svg artefacts for
// review) can drop the import without re-editing the header.
void writeFile;
