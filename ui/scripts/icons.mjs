// Renders public/icons/dvb.svg to the PNG sizes and draws design/icon-preview.png.
// Chrome renders them because the SVG's media queries pick the small-size marks.
// Run from ui/: node scripts/icons.mjs (CHROME=/path/to/chrome when Chrome is not installed).
import { readFile, writeFile } from 'node:fs/promises';
import { chromium } from 'playwright-core';

const sizes = [16, 32, 48, 64, 128, 256, 512];
const icons = new URL('../public/icons/', import.meta.url);
const svg = await readFile(new URL('dvb.svg', icons));
const source = `data:image/svg+xml;base64,${svg.toString('base64')}`;
const browser = await chromium.launch(
  process.env.CHROME ? { executablePath: process.env.CHROME } : { channel: 'chrome' },
);
try {
  const page = await browser.newPage({ deviceScaleFactor: 1 });
  const pngs = {};
  for (const size of sizes) {
    await page.setViewportSize({ width: size, height: size });
    await page.setContent(
      `<style>*{margin:0}</style><img src="${source}" width="${size}" height="${size}">`,
    );
    await page.locator('img').evaluate((img) => img.decode());
    pngs[size] = await page.screenshot({ omitBackground: true });
    await writeFile(new URL(`dvb-${size}.png`, icons), pngs[size]);
  }
  const image = (size, zoom = 1) =>
    `<figure><img src="data:image/png;base64,${pngs[size].toString('base64')}" width="${size * zoom}"><figcaption>${size}${zoom > 1 ? ` ×${zoom}` : ''}</figcaption></figure>`;
  const row = (background, ink) =>
    `<section style="background:${background};color:${ink}">${sizes
      .toReversed()
      .map((size) => image(size))
      .join('')}${image(32, 4)}${image(16, 8)}</section>`;
  await page.setViewportSize({ width: 1660, height: 100 });
  await page.setContent(
    `<style>
      * { margin: 0 }
      section { display: flex; align-items: flex-end; gap: 28px; padding: 28px 32px }
      figure { display: flex; flex-direction: column; align-items: center; gap: 10px }
      figure img { image-rendering: pixelated }
      figcaption { font: 13px ui-monospace, monospace }
    </style>
    ${row('#f5f5f3', '#4c4f69')}${row('#0e0f13', '#cbced0')}`,
  );
  await writeFile(
    new URL('../design/icon-preview.png', import.meta.url),
    await page.screenshot({ fullPage: true }),
  );
} finally {
  await browser.close();
}
